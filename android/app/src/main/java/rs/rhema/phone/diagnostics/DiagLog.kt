package rs.rhema.phone.diagnostics

import rs.rhema.phone.net.ClawResult

/**
 * The app's in-memory flight recorder: a bounded ring of one-line events
 * the workers and screens append to, newest last. It lives only as long
 * as the process — which is exactly the point: "what did the background
 * work actually do while nobody was watching" is the question the
 * diagnostics screen answers from it, and a log that outlives the
 * process would need storage and rotation this app does not otherwise
 * want.
 */
class DiagLog(private val capacity: Int = 200) {

    /** One recorded line: when it happened, and the line itself. */
    data class Entry(val at: Long, val line: String)

    private val lock = Any()
    private val entries = ArrayDeque<Entry>()

    /** Appends one line, dropping the oldest when the ring is full. */
    fun log(line: String) {
        val entry = Entry(System.currentTimeMillis(), line)
        synchronized(lock) {
            entries.addLast(entry)
            while (entries.size > capacity) {
                entries.removeFirst()
            }
        }
    }

    /** A copy of the ring, oldest first. */
    fun snapshot(): List<Entry> = synchronized(lock) { entries.toList() }
}

/**
 * The one failure vocabulary every worker logs: an HTTP status with the
 * body's head (the claw's error shapes carry the reason there), or the
 * network message. A token never appears here — bodies are error JSON.
 */
fun ClawResult<*>.diagText(): String = when (this) {
    is ClawResult.HttpError -> "HTTP $code" + (body?.let { " — ${it.take(160)}" } ?: "")
    is ClawResult.Network -> "network: $message"
    is ClawResult.Ok -> "ok"
}
