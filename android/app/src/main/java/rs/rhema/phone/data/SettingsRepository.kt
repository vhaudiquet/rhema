package rs.rhema.phone.data

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import java.util.UUID
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

// The bearer token lives in plain app-private DataStore, not
// EncryptedSharedPreferences: androidx.security:security-crypto is an
// alpha-quality, effectively unmaintained artifact, and the keystore ceremony
// it wraps buys little against the actual threat (another app) since the OS
// sandbox already isolates the file. Trade-off: root or a backup extraction
// can read it; backup is disabled in the manifest.
class SettingsRepository(private val store: DataStore<Preferences>) {

    data class Credentials(val baseUrl: String, val token: String) {
        val configured: Boolean get() = baseUrl.isNotBlank()
    }

    private object Keys {
        val baseUrl = stringPreferencesKey("claw_base_url")
        val token = stringPreferencesKey("claw_token")
        val deviceId = stringPreferencesKey("device_id")
    }

    val credentials: Flow<Credentials> =
        store.data.map { Credentials(it[Keys.baseUrl].orEmpty(), it[Keys.token].orEmpty()) }

    // True once a base URL is stored. The token is needed for /v1 but not for
    // /healthz, so only the URL gates the first-launch settings screen.
    val configured: Flow<Boolean> = credentials.map { it.configured }

    // UI-facing id stream; workers use the suspend deviceId() below.
    val deviceIdFlow: Flow<String> = store.data.map { it[Keys.deviceId].orEmpty() }

    suspend fun current(): Credentials = credentials.first()

    suspend fun setCredentials(rawBaseUrl: String, token: String) {
        // Normalize the base: trim, drop trailing slashes, and drop a
        // trailing `/v1` — the client appends the API paths itself, and a
        // pasted `…/v1` would double-prefix every call into the UI's
        // catch-all. Also downgrade the host spelling of a schemeless
        // paste by prepending http, the self-hosted default.
        val trimmed = rawBaseUrl.trim().trimEnd('/')
        val base = if (trimmed.endsWith("/v1")) trimmed.removeSuffix("/v1") else trimmed
        val baseUrl = when {
            base.startsWith("http://") || base.startsWith("https://") -> base
            base.isNotEmpty() -> "http://$base"
            else -> base
        }
        store.edit {
            it[Keys.baseUrl] = baseUrl
            it[Keys.token] = token
        }
    }

    // Generated once, then immutable for the life of the install; ingest keys
    // and the notification cursor are namespaced by it.
    suspend fun deviceId(): String {
        store.data.first()[Keys.deviceId]?.takeIf { it.isNotBlank() }?.let { return it }
        store.edit { prefs ->
            if (prefs[Keys.deviceId].isNullOrBlank()) {
                prefs[Keys.deviceId] = UUID.randomUUID().toString()
            }
        }
        val id = store.data.first()[Keys.deviceId]
        check(!id.isNullOrBlank())
        return id
    }
}
