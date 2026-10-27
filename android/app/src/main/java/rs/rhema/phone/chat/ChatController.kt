package rs.rhema.phone.chat

import android.util.Base64
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import rs.rhema.phone.data.CachedSession
import rs.rhema.phone.data.SESSION_WINDOW
import rs.rhema.phone.data.SessionCache
import rs.rhema.phone.net.ClawApi
import rs.rhema.phone.net.ClawEvent
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.net.OutboundImage
import rs.rhema.phone.net.SseFrame
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.NonCancellable
import java.io.File
import java.io.IOException

// The claw's own image budget (POST .../messages): eight images a
// message, 5 MiB decoded each. The composer enforces it at stage time;
// send() checks again so a stale staged file cannot smuggle an
// oversized body past it.
const val MAX_STAGED_IMAGES = 8
const val MAX_IMAGE_BYTES = 5L * 1024 * 1024

// Holds the live transcript of one open chat screen. The transcript is
// seeded from the session cache before the first frame and kept in the
// cache as events stream, so opening a session paints it at once and the
// claw is asked only for the events past the cache (the stream's `since`
// cursor). Rotation re-seeds from the cache; process death no longer
// costs the transcript.
class ChatController(
    private val api: ClawApi,
    private val cache: SessionCache,
    private val scope: CoroutineScope,
    val sessionKey: String,
) {
    sealed interface Item {
        val id: Long
    }

    // One image a user row renders: the staged file behind a message
    // this surface just sent (previews locally, no id yet), or the
    // attachment id a replayed row names (bytes live at the claw).
    data class SentImage(
        val attachmentId: String?,
        val file: File?,
        val mediaType: String,
    )

    data class UserMessage(
        override val id: Long,
        val text: String,
        val images: List<SentImage> = emptyList(),
    ) : Item
    data class Thinking(override val id: Long, val text: String) : Item
    data class ToolCall(
        override val id: Long,
        val name: String,
        val summary: String?,
        val isError: Boolean,
        val running: Boolean,
        // Command tools: the shell line and the exit code, both null
        // until the tool_result lands (and null forever on the others).
        val command: String? = null,
        val exitCode: Long? = null,
    ) : Item
    data class Reply(override val id: Long, val text: String, val streaming: Boolean) : Item

    // A file the agent produced; the bytes stay at the claw until this
    // screen fetches them by attachmentId.
    data class Attachment(
        override val id: Long,
        val attachmentId: String,
        val name: String,
        val media: String,
        val size: Long,
        val caption: String?,
    ) : Item

    // The agent's question to the human, answered in place. granted is
    // null while open; consent_resolved (or a local answer) flips it.
    // Replay rebuilds the whole ladder, so nothing persists here.
    data class Consent(
        override val id: Long,
        val consentId: String,
        val kind: String,
        val summary: String,
        val detail: String,
        val granted: Boolean? = null,
    ) : Item

    // The turn's end tag ("Handled", "Failed", …) from turn_done; the
    // reply carries the text, this carries the outcome.
    data class Outcome(override val id: Long, val outcome: String) : Item

    sealed interface Conn {
        data object Connecting : Conn
        data object Live : Conn
        data object Reconnecting : Conn
        data class Failed(val code: Int?, val message: String) : Conn
    }

    var items by mutableStateOf<List<Item>>(emptyList())
        private set
    var connection by mutableStateOf<Conn>(Conn.Connecting)
        private set
    var sending by mutableStateOf(false)
        private set
    var sendError by mutableStateOf<String?>(null)
        private set

    private var nextId = 0L
    private fun newId() = nextId++

    // The applied event log: the cache's tail plus what this connection
    // delivered, and [appliedTotal] its count from the session's first
    // event — the cursor every (re)connection opens with.
    private var applied: MutableList<ClawEvent> = mutableListOf()
    private var appliedTotal = 0L

    private var streamJob: Job? = null

    fun start() {
        if (streamJob != null) return
        streamJob = scope.launch {
            // Seed before the first frame: the screen shows the cached
            // transcript at once, and the stream opens past it.
            val cached = cache.get(sessionKey)
            appliedTotal = cached.total
            applied = cached.events.toMutableList()
            items = applyEvents(emptyList(), applied)
            try {
                while (isActive) {
                    api.eventStream(sessionKey, appliedTotal).collect { frame ->
                        when (frame) {
                            // The replay was cut at the cursor, so nothing
                            // already on screen arrives twice; the feed just
                            // continues from where the cache left off.
                            SseFrame.Opened -> connection = Conn.Live
                            is SseFrame.Event -> record(frame.event)
                            SseFrame.Closed -> Unit
                            is SseFrame.Failed -> connection = Conn.Failed(frame.code, frame.message)
                        }
                    }
                    // A wrong token does not fix itself by retrying.
                    val conn = connection
                    if (conn is Conn.Failed && conn.code == 401) break
                    connection = Conn.Reconnecting
                    delay(RECONNECT_DELAY_MS)
                }
            } finally {
                // The screen's own cancellation lands here too: flush the
                // cache so the next open seeds from the true tail.
                withContext(NonCancellable) { persist() }
            }
        }
    }

    // Appends one delivered event to the log and the transcript. The
    // turn's end persists the cache — once per turn, not per delta.
    private suspend fun record(event: ClawEvent) {
        applied.add(event)
        appliedTotal += 1
        items = applyEvent(items, event)
        if (event is ClawEvent.TurnDone) persist()
    }

    private suspend fun persist() {
        val window = applied.takeLast(SESSION_WINDOW)
        cache.put(sessionKey, CachedSession(total = appliedTotal, events = window))
    }

    fun stop() {
        streamJob?.cancel()
        streamJob = null
    }

    // One image staged for the next send: the file the optimistic row
    // previews, the wire type the POST body carries, and its decoded
    // size for the budget's last look.
    data class StagedImage(val file: File, val mediaType: String, val sizeBytes: Long)

    // The stream is already collecting by the time the user can press send,
    // so this preserves the claw's subscribe-then-post order. Images are
    // read and encoded here — the send path's own budget look (the composer
    // already enforced it at stage time); a violation or an unreadable file
    // fails the send with the error surfaced, never a silent drop. A failed
    // POST restores the draft in the UI. The turn's user_message event is the
    // transcript's own record of the question — replayed on a reload, and
    // echoed live right about when this returns — so the local row is only
    // appended when the echo has not already landed.
    suspend fun send(text: String, staged: List<StagedImage> = emptyList()): Boolean {
        if (staged.size > MAX_STAGED_IMAGES) {
            sendError = "over the $MAX_STAGED_IMAGES-image budget"
            return false
        }
        sending = true
        sendError = null
        val outbound = mutableListOf<OutboundImage>()
        for (image in staged) {
            try {
                val bytes = withContext(Dispatchers.IO) { image.file.readBytes() }
                if (bytes.size > MAX_IMAGE_BYTES) {
                    sending = false
                    sendError = "image over the 5 MiB decoded budget"
                    return false
                }
                outbound += OutboundImage(
                    image.mediaType,
                    Base64.encodeToString(bytes, Base64.NO_WRAP),
                )
            } catch (e: IOException) {
                sending = false
                sendError = "image could not be read"
                return false
            }
        }
        val result = api.postMessage(sessionKey, text, outbound)
        sending = false
        return when (result) {
            is ClawResult.Ok -> {
                val last = items.lastOrNull()
                if (!(last is UserMessage && last.text == text && last.images.size == staged.size)) {
                    items = items + UserMessage(
                        newId(),
                        text,
                        staged.map { SentImage(attachmentId = null, file = it.file, mediaType = it.mediaType) },
                    )
                }
                true
            }
            is ClawResult.HttpError -> {
                sendError = "HTTP ${result.code}" +
                    (result.body?.take(200)?.let { " — $it" } ?: "")
                false
            }
            is ClawResult.Network -> {
                sendError = result.message
                false
            }
        }
    }

    // Folds recorded events into a transcript, the cache seed's path:
    // the same reducer the live stream applies, run from the cache's tail.
    private fun applyEvents(items: List<Item>, events: List<ClawEvent>): List<Item> =
        events.fold(items) { acc, event -> applyEvent(acc, event) }

    private fun applyEvent(items: List<Item>, event: ClawEvent): List<Item> {
        var list = items
        when (event) {
            is ClawEvent.UserMessage -> {
                // The recorded question — replayed on reload, echoed live
                // for a message this surface just sent. An identical last
                // row — same text, same image count — is that echo's
                // stand-in; skip it, previews and all.
                val last = list.lastOrNull()
                if (!(last is UserMessage && last.text == event.text && last.images.size == event.images.size)) {
                    list = list + UserMessage(
                        newId(),
                        event.text,
                        event.images.map { SentImage(attachmentId = it.id, file = null, mediaType = it.mediaType) },
                    )
                }
            }
            is ClawEvent.ThinkingDelta -> {
                val last = list.lastOrNull()
                list = if (last is Thinking) {
                    list.dropLast(1) + last.copy(text = last.text + event.text)
                } else {
                    list + Thinking(newId(), event.text)
                }
            }
            is ClawEvent.TextDelta -> {
                // The turn has ONE reply: deltas append to the last open
                // reply even after tool rows landed between them, so the
                // agent's text reads continuously around its tool cards.
                val index = list.indexOfLast { it is Reply && it.streaming }
                list = if (index >= 0) {
                    list.toMutableList().also { mutable ->
                        val reply = mutable[index] as Reply
                        mutable[index] = reply.copy(text = reply.text + event.text)
                    }
                } else {
                    list + Reply(newId(), event.text, streaming = true)
                }
            }
            is ClawEvent.ToolStart ->
                list = list + ToolCall(newId(), event.name, summary = null, isError = false, running = true)
            is ClawEvent.ToolResult -> {
                // tool_result has no index; match the last open row of the same tool.
                val index = list.indexOfLast { it is ToolCall && it.name == event.name && it.running }
                list = if (index >= 0) {
                    list.toMutableList().also { mutable ->
                        val tool = mutable[index] as ToolCall
                        mutable[index] = tool.copy(
                            summary = event.summary,
                            isError = event.isError,
                            running = false,
                            command = event.command,
                            exitCode = event.code,
                        )
                    }
                } else {
                    list + ToolCall(
                        newId(),
                        event.name,
                        event.summary,
                        event.isError,
                        running = false,
                        command = event.command,
                        exitCode = event.code,
                    )
                }
            }
            is ClawEvent.TurnDone -> {
                // Thinking is scratch: it disappears once the turn settles, and
                // the pinned reply replaces whatever streamed in. Every reply
                // stops streaming; the last one carries the final text.
                val kept = list.filterNot { it is Thinking }
                val replyIndex = kept.indexOfLast { it is Reply }
                list = kept.mapIndexed { i, item ->
                    if (item !is Reply) {
                        item
                    } else if (i == replyIndex && !event.reply.isNullOrEmpty()) {
                        item.copy(text = event.reply, streaming = false)
                    } else {
                        item.copy(streaming = false)
                    }
                }
                event.outcome?.let { outcome -> list = list + Outcome(newId(), outcome) }
            }
            is ClawEvent.Attachment ->
                list = list + Attachment(newId(), event.id, event.name, event.media, event.size, event.caption)
            is ClawEvent.ConsentRequest ->
                list = list + Consent(newId(), event.id, event.kind, event.summary, event.detail)
            is ClawEvent.ConsentResolved -> list = flipConsent(list, event.id, event.granted)
        }
        return list
    }

    // Answers a consent question. The claw's consent_resolved event is the
    // transcript's authority and flips the card again (idempotent); the
    // local flip covers the gap before the event lands. The raw result
    // rides back so the card can show what failed.
    suspend fun answerSecret(consentId: String, secret: String): ClawResult<Unit> =
        settle(consentId, granted = true) { api.revealSecret(consentId, secret) }

    suspend fun grantPhone(consentId: String, grant: Boolean): ClawResult<Unit> =
        settle(consentId, granted = grant) { api.grantPhone(consentId, grant) }

    suspend fun denyConsent(consentId: String): ClawResult<Unit> =
        settle(consentId, granted = false) { api.denyConsent(consentId) }

    private suspend fun settle(
        consentId: String,
        granted: Boolean,
        call: suspend () -> ClawResult<Unit>,
    ): ClawResult<Unit> {
        val result = call()
        if (result is ClawResult.Ok) items = flipConsent(items, consentId, granted)
        return result
    }

    private fun flipConsent(items: List<Item>, consentId: String, granted: Boolean): List<Item> {
        val index = items.indexOfLast { it is Consent && it.consentId == consentId }
        if (index < 0) return items
        return items.toMutableList().also { list ->
            val consent = list[index] as Consent
            list[index] = consent.copy(granted = granted)
        }
    }

    private companion object {
        const val RECONNECT_DELAY_MS = 2_000L
    }
}
