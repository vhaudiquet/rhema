package rs.rhema.phone.data

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.byteArrayPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import rs.rhema.phone.net.AgentIdentity
import rs.rhema.phone.net.ClawApi
import rs.rhema.phone.net.ClawResult

// The agent's face and name as the phone last saw them: the identity
// document plus the avatar bytes. Cached in DataStore so the last known
// identity renders before (and without) a network round trip — the chat
// screen must never fall back to a hardcoded "rhema" just because one
// refresh failed.
class AgentIdentityCache(
    val identity: AgentIdentity? = null,
    val avatar: ByteArray? = null,
)

class AgentIdentityRepository(
    private val store: DataStore<Preferences>,
    private val api: ClawApi,
    private val json: Json,
) {
    private object Keys {
        val identityJson = stringPreferencesKey("agent_identity_json")
        val avatarBytes = byteArrayPreferencesKey("agent_avatar_bytes")
    }

    private val refreshMutex = Mutex()

    val cached: Flow<AgentIdentityCache> = store.data.map { prefs ->
        val identity = prefs[Keys.identityJson]?.let { raw ->
            runCatching { json.decodeFromString<AgentIdentity>(raw) }.getOrNull()
        }
        AgentIdentityCache(identity, prefs[Keys.avatarBytes])
    }

    // Fetches the identity and, when one is set, the avatar bytes. Every
    // failure keeps the cache as it was: this is presentation state, not
    // a result the UI owes the user an error for.
    suspend fun refresh() = refreshMutex.withLock {
        val identity = when (val result = api.agent()) {
            is ClawResult.Ok -> result.value
            else -> return@withLock
        }
        val avatar = if (identity.avatar) {
            when (val result = api.agentAvatar()) {
                is ClawResult.Ok -> result.value.takeIf { it.isNotEmpty() }
                else -> null
            }
        } else {
            null
        }
        val encoded = json.encodeToString(identity)
        store.edit { prefs ->
            if (prefs[Keys.identityJson] != encoded) prefs[Keys.identityJson] = encoded
            val currentBytes = prefs[Keys.avatarBytes]
            when {
                // Rewrite only on change: the avatar can weigh megabytes and
                // every refresh would otherwise rewrite it into the store.
                identity.avatar && avatar != null ->
                    if (currentBytes?.contentEquals(avatar) != true) {
                        prefs[Keys.avatarBytes] = avatar
                    }
                identity.avatar == false && currentBytes != null ->
                    prefs.remove(Keys.avatarBytes)
                // The identity says an avatar exists but the fetch failed:
                // the cached bytes stay until a fetch succeeds.
                else -> Unit
            }
        }
    }
}
