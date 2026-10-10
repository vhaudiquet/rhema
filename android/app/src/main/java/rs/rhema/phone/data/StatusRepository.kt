package rs.rhema.phone.data

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

// What the last successful phone-status run reported. A null field means the
// sensor was unreadable on this device; null fields never produce ingest posts.
@Serializable
data class PhoneSnapshot(
    val charging: Boolean? = null,
    val batteryLevel: Int? = null,
    val network: String? = null,
    val dnd: String? = null,
)

class StatusRepository(private val store: DataStore<Preferences>, private val json: Json) {

    private object Keys {
        val snapshot = stringPreferencesKey("last_phone_snapshot")
        val notificationCursor = stringPreferencesKey("last_notification_id")
    }

    suspend fun lastSnapshot(): PhoneSnapshot? =
        store.data.first()[Keys.snapshot]?.let { raw ->
            runCatching { json.decodeFromString<PhoneSnapshot>(raw) }.getOrNull()
        }

    // Only called after every ingest post of a run succeeded, so a failed run
    // retries the whole transition set next time instead of swallowing it.
    suspend fun setSnapshot(snapshot: PhoneSnapshot) {
        store.edit { it[Keys.snapshot] = json.encodeToString(snapshot) }
    }

    // Cursor into the claw's notification list: the last id this device showed.
    val notificationCursor: Flow<String> =
        store.data.map { it[Keys.notificationCursor].orEmpty() }

    suspend fun setNotificationCursor(id: String) {
        store.edit { it[Keys.notificationCursor] = id }
    }
}
