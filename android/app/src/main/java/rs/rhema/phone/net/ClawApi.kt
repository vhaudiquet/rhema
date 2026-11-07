package rs.rhema.phone.net

import rs.rhema.phone.data.SettingsRepository
import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import okhttp3.Call
import okhttp3.Callback
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response

sealed interface ClawResult<out T> {
    data class Ok<T>(val value: T) : ClawResult<T>
    data class HttpError(val code: Int, val body: String?) : ClawResult<Nothing>
    data class Network(val message: String) : ClawResult<Nothing>
}

sealed interface SseFrame {
    data object Opened : SseFrame

    // seq is the record's number for the event, from the frame's `id:`
    // line: the cursor a reconnect names as `since`. Null only when the
    // frame carried no id.
    data class Event(val event: ClawEvent, val seq: Long?) : SseFrame

    // The claw no longer holds the events our cursor named (a wiped or
    // reset database): the mirror is invalid, and the tail that follows
    // this notice is what replaces it.
    data object Reset : SseFrame

    // code != null carries an HTTP status; 401 means the token is wrong.
    data class Failed(val code: Int?, val message: String) : SseFrame
    data object Closed : SseFrame
}

// One object per claw endpoint. Paths mirror the claw's /v1 contract exactly;
// the bearer token rides on every /v1 call and never on /healthz.
class ClawApi(
    private val settings: SettingsRepository,
    baseClient: OkHttpClient,
    private val json: Json,
) {
    private val client = baseClient

    // The event stream must not time out while idle; keep-alive comments are
    // the only liveness signal. Regular calls keep the 30s default from baseClient.
    private val streamingClient = baseClient.newBuilder()
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .build()

    private val jsonBody = "application/json; charset=utf-8".toMediaType()

    // /healthz is the only unauthenticated endpoint; it is the settings gate.
    suspend fun health(): ClawResult<Unit> {
        val base = baseOrNull() ?: return ClawResult.Network("no base URL configured")
        return execute(Request.Builder().url("$base/healthz").build()) { }
    }

    suspend fun sessions(): ClawResult<List<SessionInfo>> {
        val builder = v1("sessions") ?: return ClawResult.Network("no base URL configured")
        return execute(builder.get().build()) { response ->
            json.decodeFromString<List<SessionInfo>>(response.body?.string() ?: "[]")
        }
    }

    // GET /v1/providers: every backend instance and the active pair.
    suspend fun providers(): ClawResult<ProvidersState> {
        val builder = v1("providers") ?: return ClawResult.Network("no base URL configured")
        return execute(builder.get().build()) { response ->
            json.decodeFromString<ProvidersState>(response.body?.string() ?: "{}")
        }
    }

    // GET /v1/providers/{id}/models: the instance's live listing. An id
    // without one (unknown, or the mock) is a 404 — an ordinary result,
    // not a failure: the instance still switches on its default model.
    suspend fun providerModels(id: String): ClawResult<ModelListing> {
        val builder = v1("providers", id, "models")
            ?: return ClawResult.Network("no base URL configured")
        return execute(builder.get().build()) { response ->
            json.decodeFromString<ModelListing>(response.body?.string() ?: "{}")
        }
    }

    // PUT /v1/providers/active: switch the backend instance (by id),
    // persisted claw-side;
    // it applies from the next turn. The answer carries the new pair only.
    suspend fun setActiveProvider(provider: String, model: String?): ClawResult<ActiveBackend> {
        val builder = v1("providers", "active")
            ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(ActiveRequest(provider, model)).toRequestBody(jsonBody)
        return execute(builder.put(body).build()) { response ->
            json.decodeFromString<ActiveBackend>(response.body?.string() ?: "{}")
        }
    }

    // The claw's documented dance: subscribe to the event stream first, then
    // POST; the reply arrives on the stream, not in the POST response (202).
    // Staged images ride the body only when there are some (the request
    // type's default keeps the old shape byte-stable otherwise).
    suspend fun postMessage(
        sessionKey: String,
        text: String,
        images: List<OutboundImage> = emptyList(),
    ): ClawResult<Unit> {
        val builder = v1("sessions", sessionKey, "messages")
            ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(MessageRequest(text, images)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    suspend fun ingest(source: String, key: String, payload: JsonObject): ClawResult<Unit> {
        val builder = v1("ingest") ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(IngestRequest(source, key, payload)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    // GET /v1/phones: the device fleet, for the settings screen's
    // management card.
    suspend fun phones(): ClawResult<PhonesPage> {
        val builder = v1("phones") ?: return ClawResult.Network("no base URL configured")
        return execute(builder.get().build()) { response ->
            json.decodeFromString<PhonesPage>(response.body?.string() ?: "{}")
        }
    }

    // DELETE /v1/phones/{device}: retire a device's records on the claw.
    // The claw answers {"forgotten": bool} — false for an unknown name,
    // which only means another surface got there first.
    suspend fun forgetPhone(device: String): ClawResult<Unit> {
        val builder = v1("phones", device) ?: return ClawResult.Network("no base URL configured")
        return execute(builder.delete().build()) { }
    }

    // GET /v1/notifications?device=&since=N. `since` is the last id the
    // device has acked; before the first ack there is no cursor at all,
    // and an empty `since=` came back
    // "HTTP 400 — Failed to deserialize query string: since: cannot
    // parse integer from empty string", which failed the whole poll.
    // So the parameter is sent only when there is a positive id to send:
    // absent and blank mean the same thing to the claw, and the shorter
    // request is the honest one.
    suspend fun notifications(deviceId: String, since: String): ClawResult<List<RhemaNotification>> {
        val base = baseOrNull() ?: return ClawResult.Network("no base URL configured")
        val cursor = since.trim().toLongOrNull()?.takeIf { it > 0 }
        val url = base.toHttpUrlOrNull()?.newBuilder()
            ?.addPathSegments("v1/notifications")
            ?.addQueryParameter("device", deviceId)
            ?.apply { cursor?.let { addQueryParameter("since", it.toString()) } }
            ?.build()
            ?: return ClawResult.Network("bad base URL")
        val request = Request.Builder().url(url).header("Authorization", bearer()).build()
        return execute(request) { response ->
            json.decodeFromString<NotificationPage>(response.body?.string() ?: "{}").notifications
        }
    }

    suspend fun ackNotification(id: String): ClawResult<Unit> {
        val builder = v1("notifications", id, "ack")
            ?: return ClawResult.Network("no base URL configured")
        return execute(builder.post(EMPTY_BODY).build()) { }
    }

    // GET /v1/attachments/{id} and GET /v1/agent/avatar: raw bytes, not
    // JSON. The generic execute() rejects any 2xx body that does not open
    // like JSON, so these endpoints share one plain read with the same
    // error vocabulary.
    suspend fun attachment(id: String): ClawResult<ByteArray> {
        val builder = v1("attachments", id)
            ?: return ClawResult.Network("no base URL configured")
        return rawGet(builder)
    }

    // The avatar behind GET /v1/agent; the identity document's `avatar`
    // flag says whether the fetch can succeed.
    suspend fun agentAvatar(): ClawResult<ByteArray> {
        val builder = v1("agent", "avatar")
            ?: return ClawResult.Network("no base URL configured")
        return rawGet(builder)
    }

    private suspend fun rawGet(builder: Request.Builder): ClawResult<ByteArray> =
        withContext(Dispatchers.IO) {
            try {
                client.newCall(builder.get().build()).execute().use { response ->
                    if (response.isSuccessful) {
                        ClawResult.Ok(response.body?.bytes() ?: ByteArray(0))
                    } else {
                        ClawResult.HttpError(response.code, response.body?.string()?.take(500))
                    }
                }
            } catch (e: IOException) {
                ClawResult.Network(e.message ?: "network error")
            }
        }

    // GET /v1/agent: the agent's presentation identity — the name, mark,
    // initials, and whether the avatar image is set.
    suspend fun agent(): ClawResult<AgentIdentity> {
        val builder = v1("agent") ?: return ClawResult.Network("no base URL configured")
        return execute(builder.get().build()) { response ->
            json.decodeFromString<AgentIdentity>(response.body?.string() ?: "{}")
        }
    }

    // GET /v1/consents: the pending questions, oldest first.
    suspend fun consents(): ClawResult<List<ConsentInfo>> {
        val builder = v1("consents") ?: return ClawResult.Network("no base URL configured")
        return execute(builder.get().build()) { response ->
            json.decodeFromString<ConsentsPage>(response.body?.string() ?: "{}").consents
        }
    }

    // POST /v1/consents/{id}: the kind decides the body — the secret
    // string for kind=secret, the grant flag for kind=phone. A 409 means
    // the question was already answered (here or on another surface).
    suspend fun revealSecret(id: String, secret: String): ClawResult<Unit> {
        val builder = v1("consents", id) ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(SecretConsentAnswer(secret)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    suspend fun grantPhone(id: String, grant: Boolean): ClawResult<Unit> {
        val builder = v1("consents", id) ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(PhoneConsentAnswer(grant)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    // DELETE /v1/consents/{id}: deny, or cancel a question left open.
    suspend fun denyConsent(id: String): ClawResult<Unit> {
        val builder = v1("consents", id) ?: return ClawResult.Network("no base URL configured")
        return execute(builder.delete().build()) { }
    }

    // GET /v1/phone/commands?device=: the control queue for this device.
    // Built by hand like notifications(): v1() bakes the URL into the
    // builder, leaving no room for query parameters.
    suspend fun phoneCommands(deviceId: String): ClawResult<List<PhoneCommand>> {
        val base = baseOrNull() ?: return ClawResult.Network("no base URL configured")
        val url = base.toHttpUrlOrNull()?.newBuilder()
            ?.addPathSegments("v1/phone/commands")
            ?.addQueryParameter("device", deviceId)
            ?.build()
            ?: return ClawResult.Network("bad base URL")
        val request = Request.Builder().url(url).header("Authorization", bearer()).build()
        return execute(request) { response ->
            json.decodeFromString<CommandPage>(response.body?.string() ?: "{}").commands
        }
    }

    // POST /v1/phone/commands/{id}/result: the ack. Until it lands the
    // claw keeps the command queued and the next run repeats it.
    suspend fun postPhoneCommandResult(id: String, ok: Boolean, detail: String?): ClawResult<Unit> {
        val builder = v1("phone", "commands", id, "result")
            ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(CommandResult(ok, detail)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    // GET /v1/sessions/{key}/events?since=N: the claw replays the events
    // past seq N, then goes live. `since` is the transcript cache's
    // cursor — the seq of the last event this device applied — so a
    // reopening screen receives only the gap instead of the whole
    // history. A negative `since` asks for nothing: the claw answers
    // with the newest turn, which is what a screen with no cache opens
    // on. Built by hand like notifications(): v1() bakes the URL into
    // the builder, leaving no room for a query.
    fun eventStream(sessionKey: String, since: Long): Flow<SseFrame> = callbackFlow {
        val url = baseOrNull()?.toHttpUrlOrNull()?.newBuilder()
            ?.addPathSegments("v1/sessions")
            ?.addPathSegment(sessionKey)
            ?.addPathSegment("events")
            ?.apply { if (since >= 0) addQueryParameter("since", since.toString()) }
            ?.build()
        if (url == null) {
            trySend(SseFrame.Failed(null, "no base URL configured"))
            close()
            return@callbackFlow
        }
        val call = streamingClient.newCall(
            Request.Builder().url(url).header("Authorization", bearer()).build(),
        )
        // The chat screen cancels its collector on dispose; call.cancel() is
        // what unblocks the blocking read inside onResponse below.
        call.enqueue(object : Callback {
            override fun onFailure(call: Call, e: IOException) {
                trySend(SseFrame.Failed(null, e.message ?: "connection failed"))
                close()
            }

            override fun onResponse(call: Call, response: Response) {
                response.use { resp ->
                    if (resp.code == 401) {
                        trySend(SseFrame.Failed(401, "unauthorized"))
                        close()
                        return
                    }
                    if (!resp.isSuccessful) {
                        trySend(SseFrame.Failed(resp.code, "HTTP ${resp.code}"))
                        close()
                        return
                    }
                    trySend(SseFrame.Opened)
                    val source = resp.body?.source()
                    if (source == null) {
                        trySend(SseFrame.Failed(resp.code, "empty stream body"))
                        close()
                        return
                    }
                    // SSE framing per contract: blank-line-delimited
                    // frames, `data:` the payload, `id:` the event's seq,
                    // ":" comment lines the keep-alives (plus the claw's
                    // lag and reset notices). `event:` lines do not occur.
                    val data = StringBuilder()
                    var id: String? = null
                    try {
                        while (true) {
                            val line = source.readUtf8Line() ?: break
                            when {
                                line.startsWith(":") -> {
                                    if (line.removePrefix(":").trimStart().startsWith("reset")) {
                                        trySend(SseFrame.Reset)
                                    }
                                }
                                line.isEmpty() -> {
                                    if (data.isNotEmpty()) {
                                        val payload = data.toString().trim()
                                        val seq = id?.trim()?.toLongOrNull()
                                        id = null
                                        data.setLength(0)
                                        runCatching { json.decodeFromString<ClawEvent>(payload) }
                                            .getOrNull()
                                            ?.let { trySend(SseFrame.Event(it, seq)) }
                                    }
                                }
                                line.startsWith("data:") -> {
                                    data.append(line.removePrefix("data:").trimStart())
                                    data.append('\n')
                                }
                                line.startsWith("id:") ->
                                    id = line.removePrefix("id:").trim()
                            }
                        }
                        trySend(SseFrame.Closed)
                    } catch (e: IOException) {
                        // call.cancel() from awaitClose lands here: normal close.
                        trySend(SseFrame.Closed)
                    }
                    close()
                }
            }
        })
        awaitClose { call.cancel() }
    }.buffer(Channel.UNLIMITED)

    // GET /v1/sessions/{key}/history?before=<seq>&limit=<n>: one page of
    // whole turns older than the seq this device holds oldest, for the
    // walk back through a session's history. `more` false means the page
    // reached the session's first event. Built by hand like
    // eventStream(): the query is not expressible through v1().
    suspend fun history(sessionKey: String, before: Long, limit: Int): ClawResult<HistoryPage> {
        val base = baseOrNull() ?: return ClawResult.Network("no base URL configured")
        val url = base.toHttpUrlOrNull()?.newBuilder()
            ?.addPathSegments("v1/sessions")
            ?.addPathSegment(sessionKey)
            ?.addPathSegment("history")
            ?.addQueryParameter("before", before.toString())
            ?.addQueryParameter("limit", limit.toString())
            ?.build() ?: return ClawResult.Network("base URL is not a URL")
        return execute(Request.Builder().url(url).header("Authorization", bearer()).build()) { response ->
            json.decodeFromString<HistoryPage>(response.body?.string() ?: "{}")
        }
    }

    private suspend fun baseOrNull(): String? =
        settings.current().baseUrl.trim().trimEnd('/').takeIf { it.isNotBlank() }

    // The claw's middleware strips the `Bearer ` prefix; the raw token
    // alone does not authorize.
    private suspend fun bearer(): String = "Bearer " + settings.current().token

    // Builds GET-ready /v1/{...} request with auth; null when unconfigured.
    private suspend fun v1(vararg segments: String): Request.Builder? {
        // The `v1` prefix lives here and nowhere else: the callers name
        // only their own segments. Without it every call lands in the
        // web UI's catch-all and reads as a 2xx HTML page.
        val base = baseOrNull() ?: return null
        val url = base.toHttpUrlOrNull()?.newBuilder() ?: return null
        url.addPathSegment("v1")
        for (segment in segments) url.addPathSegment(segment)
        return Request.Builder().url(url.build()).header("Authorization", bearer())
    }

    private suspend fun <T> execute(request: Request, parse: (Response) -> T): ClawResult<T> =
        withContext(Dispatchers.IO) {
            try {
                client.newCall(request).execute().use { response ->
                    if (response.isSuccessful) {
                        // Peek first: a 2xx body that is not the expected
                        // JSON (an HTML page from a wrong route, a captive
                        // portal) is an error result naming the body, never
                        // a crash — and the snippet says who actually
                        // answered.
                        val peeked = response.peekBody(256).string().trimStart()
                        if (!peeked.startsWith("{") && !peeked.startsWith("[")) {
                            ClawResult.HttpError(
                                response.code,
                                "unexpected 2xx body: " + peeked.take(150),
                            )
                        } else {
                            try {
                                ClawResult.Ok(parse(response))
                            } catch (e: SerializationException) {
                                // Valid JSON in the wrong shape fails here
                                // too; the serializer's message names the
                                // field, so it rides along with the body.
                                ClawResult.HttpError(
                                    response.code,
                                    "unexpected response body: " +
                                        (e.message ?: "unparseable JSON") +
                                        " — body: " + peeked.take(150),
                                )
                            }
                        }
                    } else {
                        ClawResult.HttpError(response.code, response.body?.string()?.take(500))
                    }
                }
            } catch (e: IOException) {
                ClawResult.Network(e.message ?: "network error")
            }
        }

    private companion object {
        val EMPTY_BODY = ByteArray(0).toRequestBody(null)
    }
}
