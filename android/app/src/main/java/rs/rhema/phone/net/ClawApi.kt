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
    data class Event(val event: ClawEvent) : SseFrame

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
    suspend fun postMessage(sessionKey: String, text: String): ClawResult<Unit> {
        val builder = v1("sessions", sessionKey, "messages")
            ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(MessageRequest(text)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    suspend fun ingest(source: String, key: String, payload: JsonObject): ClawResult<Unit> {
        val builder = v1("ingest") ?: return ClawResult.Network("no base URL configured")
        val body = json.encodeToString(IngestRequest(source, key, payload)).toRequestBody(jsonBody)
        return execute(builder.post(body).build()) { }
    }

    suspend fun notifications(deviceId: String, since: String): ClawResult<List<RhemaNotification>> {
        val base = baseOrNull() ?: return ClawResult.Network("no base URL configured")
        val url = base.toHttpUrlOrNull()?.newBuilder()
            ?.addPathSegments("v1/notifications")
            ?.addQueryParameter("device", deviceId)
            ?.addQueryParameter("since", since)
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

    // GET /v1/attachments/{id}: raw bytes, not JSON. The generic execute()
    // rejects any 2xx body that does not open like JSON, so this endpoint
    // gets its own plain read with the same error vocabulary.
    suspend fun attachment(id: String): ClawResult<ByteArray> {
        val builder = v1("attachments", id)
            ?: return ClawResult.Network("no base URL configured")
        return withContext(Dispatchers.IO) {
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

    fun eventStream(sessionKey: String): Flow<SseFrame> = callbackFlow {
        val builder = v1("sessions", sessionKey, "events")
        if (builder == null) {
            trySend(SseFrame.Failed(null, "no base URL configured"))
            close()
            return@callbackFlow
        }
        val call = streamingClient.newCall(builder.get().build())
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
                    // SSE framing per contract: blank-line-delimited data lines,
                    // ":" comment lines are keep-alives. No event:/id: fields.
                    val data = StringBuilder()
                    try {
                        while (true) {
                            val line = source.readUtf8Line() ?: break
                            when {
                                line.startsWith(":") -> Unit
                                line.isEmpty() -> {
                                    if (data.isNotEmpty()) {
                                        val payload = data.toString().trim()
                                        data.setLength(0)
                                        runCatching { json.decodeFromString<ClawEvent>(payload) }
                                            .getOrNull()
                                            ?.let { trySend(SseFrame.Event(it)) }
                                    }
                                }
                                line.startsWith("data:") -> {
                                    data.append(line.removePrefix("data:").trimStart())
                                    data.append('\n')
                                }
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
