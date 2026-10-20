package rs.rhema.phone.data

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.longPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

// What the last successful phone-status run reported. A null field means the
// sensor was unreadable or ungranted on this device; null fields never produce
// ingest posts. New fields default to null, so a snapshot persisted by an
// older build decodes (and re-posts each new topic once, which is what a
// first report should do).
@Serializable
data class PhoneSnapshot(
    val charging: Boolean? = null,
    val batteryLevel: Int? = null,
    val network: String? = null,
    val dnd: String? = null,
    // The last posted location, kept for the move-distance rule.
    val locationLat: Double? = null,
    val locationLon: Double? = null,
    // The last posted foreground app package.
    val app: String? = null,
    // The last posted device-identity fingerprint.
    val device: String? = null,
    val wifi: String? = null,
    // The next alarm's trigger time, epoch millis.
    val alarm: Long? = null,
)

class StatusRepository(private val store: DataStore<Preferences>, private val json: Json) {

    private object Keys {
        val snapshot = stringPreferencesKey("last_phone_snapshot")
        val reportedAt = longPreferencesKey("last_phone_report_at")
        val notificationCursor = stringPreferencesKey("last_notification_id")
    }

    suspend fun lastSnapshot(): PhoneSnapshot? =
        store.data.first()[Keys.snapshot]?.let { raw ->
            runCatching { json.decodeFromString<PhoneSnapshot>(raw) }.getOrNull()
        }

    // When the last run landed every post, epoch millis; 0 before the
    // first one. The fleet card watches this so it reloads after a report
    // lands: the last-seen it draws is the claw's clock, which only moves
    // when a run has actually reached it.
    val lastReportAt: Flow<Long> =
        store.data.map { it[Keys.reportedAt] ?: 0L }

    // Only called after every ingest post of a run succeeded, so a failed run
    // retries the whole transition set next time instead of swallowing it.
    suspend fun setSnapshot(snapshot: PhoneSnapshot) {
        store.edit {
            it[Keys.snapshot] = json.encodeToString(snapshot)
            it[Keys.reportedAt] = System.currentTimeMillis()
        }
    }

    // Cursor into the claw's notification list: the last id this device showed.
    val notificationCursor: Flow<String> =
        store.data.map { it[Keys.notificationCursor].orEmpty() }

    suspend fun setNotificationCursor(id: String) {
        store.edit { it[Keys.notificationCursor] = id }
    }
}
