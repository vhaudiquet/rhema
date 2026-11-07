package rs.rhema.phone.data

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import kotlinx.coroutines.flow.first
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import rs.rhema.phone.net.ClawEvent

// One session's cached transcript tail: the events this device has
// applied, persisted as they stream. Opening a session seeds the screen
// from here instead of watching the claw replay the session again, and
// the stream opens with [cursor] as its `since` so the claw sends only
// the events past the cache.
@Serializable
data class CachedSession(
    // The seq of the newest event in [events] — the `since` cursor. -1
    // when nothing is cached.
    val cursor: Long = -1,
    // The seq of the oldest event in [events]: the anchor the walk back
    // through /history ends at.
    val first: Long = -1,
    val events: List<ClawEvent> = emptyList(),
    // The cursor of a cache written before the stream named seqs: a
    // count of the session's events. Read only to migrate one, see
    // [resumeCursor]; -1 for every cache written since.
    val total: Long = -1,
) {
    // A cache from before seqs still resumes exactly: the record numbers
    // events densely from zero, so the last one it applied is `total - 1`,
    // and the window above it is contiguous. The first open after the
    // upgrade costs nothing.
    val resumeCursor: Long
        get() = if (cursor >= 0) cursor else if (total > 0) total - 1 else -1

    val oldestSeq: Long
        get() = if (first >= 0) {
            first
        } else if (events.isNotEmpty() && resumeCursor >= 0) {
            resumeCursor - events.size + 1
        } else {
            -1
        }
}

// How many events the cache keeps per session. A resident session runs
// for weeks; a bounded window keeps the DataStore write flat, and a tail
// is enough to paint the screen — the claw still holds the whole record.
const val SESSION_WINDOW = 500

class SessionCache(private val store: DataStore<Preferences>, private val json: Json) {

    private fun key(sessionKey: String) = stringPreferencesKey("session_cache:$sessionKey")

    /** The cached tail, or an empty one for a session never opened here. */
    suspend fun get(sessionKey: String): CachedSession =
        store.data.first()[key(sessionKey)]?.let { raw ->
            runCatching { json.decodeFromString<CachedSession>(raw) }.getOrNull()
        } ?: CachedSession()

    suspend fun put(sessionKey: String, cache: CachedSession) {
        store.edit { it[key(sessionKey)] = json.encodeToString(cache) }
    }
}
