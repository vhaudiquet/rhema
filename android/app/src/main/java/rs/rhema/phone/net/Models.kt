package rs.rhema.phone.net

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject

// GET /v1/sessions
@Serializable
data class SessionInfo(
    val key: String,
    val state: String? = null,
    val revision: Long? = null,
    @SerialName("inbox_depth") val inboxDepth: Int? = null,
    // The claw serializes the outcome with serde's default externally
    // tagged enum form: unit variants as bare strings ("Handled",
    // "Superseded"), Failed as {"Failed":"reason"} — or null when the
    // key never released. Kept as raw JSON; the UI shows the variant
    // name.
    @SerialName("last_outcome") val lastOutcome: JsonElement? = null,
)

// POST /v1/sessions/{key}/messages
@Serializable
data class MessageRequest(val text: String)

// GET /v1/providers — the backend instances and the active pair. Each
// row is one instance, named by its id: the kind unless a login named
// it, so two instances of one kind coexist under two ids.
@Serializable
data class ProviderInfo(
    val id: String,
    val kind: String,
    @SerialName("default_model") val defaultModel: String,
)

@Serializable
data class ActivePair(val provider: String, val model: String)

@Serializable
data class ProvidersState(val active: ActivePair, val providers: List<ProviderInfo>)

// GET /v1/providers/{id}/models — an instance's live listing; 404 when none.
@Serializable
data class ModelCard(
    val id: String,
    @SerialName("display_name") val displayName: String? = null,
    val description: String? = null,
)

@Serializable
data class ModelListing(val models: List<ModelCard> = emptyList())

// PUT /v1/providers/active — answers the new pair only (provider is the
// instance id); the list is not echoed and the caller carries it forward.
@Serializable
data class ActiveRequest(val provider: String, val model: String? = null)

@Serializable
data class ActiveBackend(val active: ActivePair)

// POST /v1/ingest
@Serializable
data class IngestRequest(val source: String, val key: String, val payload: JsonObject)

// GET /v1/notifications — the claw wraps the list in an object.
@Serializable
data class NotificationPage(val notifications: List<RhemaNotification> = emptyList())

// One queued notification: ids and timestamps are numeric (SQLite row id
// and epoch millis), the ack path and the `since` cursor carry the id.
@Serializable
data class RhemaNotification(
    val id: Long,
    val device: String? = null,
    val title: String,
    val body: String,
    @SerialName("created_at") val createdAt: Long,
)

// Frames of GET /v1/sessions/{key}/events; "type" is the discriminator.
// The stream opens with the session's recorded turns replayed before the
// live feed, so a screen (re)builds its transcript from the replay.
// Unknown types fail the per-frame parse and are skipped, so the API can grow
// without breaking older clients.
@Serializable
sealed interface ClawEvent {
    @Serializable
    @SerialName("user_message")
    data class UserMessage(val text: String = "") : ClawEvent

    @Serializable
    @SerialName("thinking_delta")
    data class ThinkingDelta(val text: String = "") : ClawEvent

    @Serializable
    @SerialName("text_delta")
    data class TextDelta(val text: String = "") : ClawEvent

    @Serializable
    @SerialName("tool_start")
    data class ToolStart(val name: String = "") : ClawEvent

    @Serializable
    @SerialName("tool_result")
    data class ToolResult(
        val name: String = "",
        @SerialName("is_error") val isError: Boolean = false,
        val summary: String? = null,
        // Command tools answer with a process exit code and the shell
        // line; both ride only on the bash-like tools.
        val code: Long? = null,
        val command: String? = null,
    ) : ClawEvent

    // The reply is null when the turn ended without producing text —
    // a failed turn, mostly. Nullable on purpose: a dropped frame would
    // hide exactly the turns a viewer needs the outcome tag for.
    @Serializable
    @SerialName("turn_done")
    data class TurnDone(val reply: String? = null, val outcome: String? = null) : ClawEvent

    // A file the agent produced. The bytes are not on the stream; they
    // live at GET /v1/attachments/{id} behind the same bearer token.
    @Serializable
    @SerialName("attachment")
    data class Attachment(
        val id: String,
        val name: String,
        val media: String = "file",
        val size: Long = 0,
        val caption: String? = null,
    ) : ClawEvent
}
