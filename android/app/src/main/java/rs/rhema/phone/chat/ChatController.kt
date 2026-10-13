package rs.rhema.phone.chat

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import rs.rhema.phone.net.ClawApi
import rs.rhema.phone.net.ClawEvent
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.net.SseFrame
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

// Holds the live transcript of one open chat screen. Its lifetime is the
// screen's: rotation or process death drops the transcript (v1 trade-off;
// the claw-side session is unaffected).
class ChatController(
    private val api: ClawApi,
    private val scope: CoroutineScope,
    val sessionKey: String,
) {
    sealed interface Item {
        val id: Long
    }

    data class UserMessage(override val id: Long, val text: String) : Item
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

    private var streamJob: Job? = null

    fun start() {
        if (streamJob != null) return
        streamJob = scope.launch {
            while (isActive) {
                api.eventStream(sessionKey).collect { frame ->
                    when (frame) {
                        // Every (re)connection opens with the claw's replay of
                        // the recorded turns, then the live feed: drop the
                        // mirror and let the replay rebuild it, or the replay
                        // would double everything already on screen.
                        SseFrame.Opened -> {
                            items = emptyList()
                            connection = Conn.Live
                        }
                        is SseFrame.Event -> apply(frame.event)
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
        }
    }

    fun stop() {
        streamJob?.cancel()
        streamJob = null
    }

    // The stream is already collecting by the time the user can press send,
    // so this preserves the claw's subscribe-then-post order. A failed POST
    // restores the draft in the UI. The turn's user_message event is the
    // transcript's own record of the question — replayed on a reload, and
    // echoed live right about when this returns — so the local row is only
    // appended when the echo has not already landed.
    suspend fun send(text: String): Boolean {
        sending = true
        sendError = null
        val result = api.postMessage(sessionKey, text)
        sending = false
        return when (result) {
            is ClawResult.Ok -> {
                val last = items.lastOrNull()
                if (!(last is UserMessage && last.text == text)) {
                    items = items + UserMessage(newId(), text)
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

    private fun apply(event: ClawEvent) {
        when (event) {
            is ClawEvent.UserMessage -> {
                // The recorded question — replayed on reload, echoed live
                // for a message this surface just sent. An identical last
                // row is that echo's stand-in; skip it.
                val last = items.lastOrNull()
                if (!(last is UserMessage && last.text == event.text)) {
                    items = items + UserMessage(newId(), event.text)
                }
            }
            is ClawEvent.ThinkingDelta -> {
                val last = items.lastOrNull()
                items = if (last is Thinking) {
                    items.dropLast(1) + last.copy(text = last.text + event.text)
                } else {
                    items + Thinking(newId(), event.text)
                }
            }
            is ClawEvent.TextDelta -> {
                // The turn has ONE reply: deltas append to the last open
                // reply even after tool rows landed between them, so the
                // agent's text reads continuously around its tool cards.
                val index = items.indexOfLast { it is Reply && it.streaming }
                items = if (index >= 0) {
                    items.toMutableList().also { list ->
                        val reply = list[index] as Reply
                        list[index] = reply.copy(text = reply.text + event.text)
                    }
                } else {
                    items + Reply(newId(), event.text, streaming = true)
                }
            }
            is ClawEvent.ToolStart ->
                items = items + ToolCall(newId(), event.name, summary = null, isError = false, running = true)
            is ClawEvent.ToolResult -> {
                // tool_result has no index; match the last open row of the same tool.
                val index = items.indexOfLast { it is ToolCall && it.name == event.name && it.running }
                items = if (index >= 0) {
                    items.toMutableList().also { list ->
                        val tool = list[index] as ToolCall
                        list[index] = tool.copy(
                            summary = event.summary,
                            isError = event.isError,
                            running = false,
                            command = event.command,
                            exitCode = event.code,
                        )
                    }
                } else {
                    items + ToolCall(
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
                val kept = items.filterNot { it is Thinking }
                val replyIndex = kept.indexOfLast { it is Reply }
                items = kept.mapIndexed { i, item ->
                    if (item !is Reply) {
                        item
                    } else if (i == replyIndex && !event.reply.isNullOrEmpty()) {
                        item.copy(text = event.reply, streaming = false)
                    } else {
                        item.copy(streaming = false)
                    }
                }
                event.outcome?.let { outcome -> items = items + Outcome(newId(), outcome) }
            }
            is ClawEvent.Attachment ->
                items = items + Attachment(newId(), event.id, event.name, event.media, event.size, event.caption)
            is ClawEvent.ConsentRequest ->
                items = items + Consent(newId(), event.id, event.kind, event.summary, event.detail)
            is ClawEvent.ConsentResolved -> flipConsent(event.id, event.granted)
        }
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
        if (result is ClawResult.Ok) flipConsent(consentId, granted)
        return result
    }

    private fun flipConsent(consentId: String, granted: Boolean) {
        val index = items.indexOfLast { it is Consent && it.consentId == consentId }
        if (index >= 0) {
            items = items.toMutableList().also { list ->
                val consent = list[index] as Consent
                list[index] = consent.copy(granted = granted)
            }
        }
    }

    private companion object {
        const val RECONNECT_DELAY_MS = 2_000L
    }
}
