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
// from here instead of watching the claw's whole replay again, and the
// stream opens with `total` as its `since` cursor so the claw sends only
// the events past the cache.
@Serializable
data class CachedSession(
    // The session's full event count — the `since` cursor. Can exceed
    // [events]: the list keeps only the newest [SESSION_WINDOW] events,
    // the count counts from the session's first.
    val total: Long = 0,
    val events: List<ClawEvent> = emptyList(),
)

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
