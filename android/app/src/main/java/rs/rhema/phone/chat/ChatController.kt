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
// cursor, which is the seq of the last event applied). A screen with no
// cache opens on the newest turn and walks back through the history
// route as the reader scrolls up. Rotation re-seeds from the cache;
// process death no longer costs the transcript.
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

    // One applied event with the seq the claw gave it: the cache window
    // needs the seqs to resume from, and a lagged drop leaves a gap the
    // window's arithmetic must not paper over.
    private data class Applied(val seq: Long?, val event: ClawEvent)

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

    // The applied event log the cache window is taken from: the cached
    // tail plus what this connection delivered. Pages folded in above it
    // by [loadOlder] are transcript rows only — the walk back re-reads
    // them, so they never bloat what is persisted.
    private var applied: MutableList<Applied> = mutableListOf()
    // The seq of the newest event applied: the cursor every
    // (re)connection opens with. -1 asks for the tail.
    private var cursor = -1L
    // The seq of the oldest event on screen: the anchor [loadOlder]
    // ends its page at. -1 when the screen holds nothing.
    private var oldestSeq = -1L

    // The walk back through history: whether a page is in flight, and
    // whether any more exist below what the screen holds.
    var loadingOlder by mutableStateOf(false)
        private set
    var moreOlder by mutableStateOf(false)
        private set

    private var streamJob: Job? = null

    fun start() {
        if (streamJob != null) return
        streamJob = scope.launch {
            // Seed before the first frame: the screen shows the cached
            // transcript at once, and the stream opens past it.
            val cached = cache.get(sessionKey)
            cursor = cached.resumeCursor
            oldestSeq = cached.oldestSeq
            applied = cached.events.map { Applied(null, it) }.toMutableList()
            items = applyEvents(emptyList(), cached.events)
            // A cache holds a window, not the record: the walk back has
            // somewhere to go unless the session held nothing. (A seed
            // from an empty cache leaves this false; the tail's first
            // event re-arms it — see [record].)
            moreOlder = items.isNotEmpty()
            try {
                while (isActive) {
                    api.eventStream(sessionKey, cursor).collect { frame ->
                        when (frame) {
                            // The replay was cut at the cursor, so nothing
                            // already on screen arrives twice; the feed just
                            // continues from where the cache left off. With
                            // no cursor it is the newest turn — the screen
                            // paints it and the reader walks back.
                            SseFrame.Opened -> connection = Conn.Live
                            is SseFrame.Event -> record(frame.event, frame.seq)
                            // The cursor named events the claw no longer
                            // holds: drop the mirror, take the tail that
                            // follows.
                            SseFrame.Reset -> resetMirror()
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
    private suspend fun record(event: ClawEvent, seq: Long?) {
        if (seq != null) cursor = seq
        if (seq != null && oldestSeq < 0) {
            // The first event on an empty mirror: this is the tail the
            // claw opened with, and the record may hold more above it —
            // only a page that comes back empty (or `more` false) says
            // otherwise.
            oldestSeq = seq
            moreOlder = true
        }
        applied.add(Applied(seq, event))
        items = applyEvent(items, event)
        if (event is ClawEvent.TurnDone) persist()
    }

    // The claw no longer holds what this screen's cursor named: the
    // transcript goes, and the tail that follows the notice rebuilds it.
    private suspend fun resetMirror() {
        applied = mutableListOf()
        cursor = -1
        oldestSeq = -1
        items = emptyList()
        loadingOlder = false
        moreOlder = false
        cache.put(sessionKey, CachedSession())
    }

    // One page of turns older than the screen's oldest, appended above it.
    // Whole turns only, so the page never duplicates or strands a row; the
    // page ends exactly where the screen begins, and [moreOlder] says
    // whether another exists. A page folded from empty goes through the
    // same reducer the stream uses, so a replayed consent or attachment
    // card folds exactly as it would have live.
    suspend fun loadOlder() {
        if (loadingOlder || !moreOlder || oldestSeq < 0) return
        loadingOlder = true
        try {
            when (val result = api.history(sessionKey, oldestSeq, HISTORY_PAGE)) {
                is ClawResult.Ok -> {
                    val page = result.value.events.mapNotNull { entry ->
                        entry.event?.let { entry.seq to it }
                    }
                    val first = page.firstOrNull() ?: return
                    // The page is older than everything on screen, and the
                    // list runs oldest first: it goes above, never below —
                    // appended, it would land at the newest end.
                    items = applyEvents(emptyList(), page.map { it.second }) + items
                    oldestSeq = first.first
                    moreOlder = result.value.more
                }
                is ClawResult.HttpError, is ClawResult.Network -> Unit
            }
        } finally {
            loadingOlder = false
        }
    }

    private suspend fun persist() {
        // The window starts on a turn boundary: a page cut mid-turn would
        // paint a reply with no question above it on the next open, and
        // the walk back from the window's first seq would then have to
        // start inside a turn instead of at its top.
        val window = applied.takeLast(SESSION_WINDOW)
        val start = window.indexOfFirst { it.event is ClawEvent.UserMessage }.takeIf { it > 0 } ?: 0
        val kept = window.drop(start)
        cache.put(
            sessionKey,
            CachedSession(
                cursor = cursor,
                first = kept.firstOrNull()?.seq ?: -1,
                events = kept.map { it.event },
            ),
        )
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

        // The most events one history page may hold: the claw widens the
        // page back to a turn boundary, so this is a floor, not a cap.
        const val HISTORY_PAGE = 400
    }
}
