package rs.rhema.phone.net

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonObject

// GET /v1/sessions
@Serializable
data class SessionInfo(
    val key: String,
    val state: String? = null,
    val revision: Long? = null,
    @SerialName("inbox_depth") val inboxDepth: Int? = null,
    // The claw serializes the outcome as an externally-tagged enum:
    // {"Handled":null}, {"Failed":"reason"}, {"Superseded":null} — or
    // null when the key never released. Kept as raw JSON; the UI shows
    // the variant name.
    @SerialName("last_outcome") val lastOutcome: JsonObject? = null,
)

// POST /v1/sessions/{key}/messages
@Serializable
data class MessageRequest(val text: String)

// GET /v1/providers — the constructible backends and the active pair.
@Serializable
data class ProviderInfo(
    val kind: String,
    @SerialName("default_model") val defaultModel: String,
)

@Serializable
data class ActivePair(val provider: String, val model: String)

@Serializable
data class ProvidersState(val active: ActivePair, val providers: List<ProviderInfo>)

// GET /v1/providers/{kind}/models — a kind's live listing; 404 when none.
@Serializable
data class ModelCard(
    val id: String,
    @SerialName("display_name") val displayName: String? = null,
    val description: String? = null,
)

@Serializable
data class ModelListing(val models: List<ModelCard> = emptyList())

// PUT /v1/providers/active — answers the new pair only; the provider
// list is not echoed and the caller carries it forward.
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
// Unknown types fail the per-frame parse and are skipped, so the API can grow
// without breaking older clients.
@Serializable
sealed interface ClawEvent {
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
    ) : ClawEvent

    @Serializable
    @SerialName("turn_done")
    data class TurnDone(val reply: String = "", val outcome: String? = null) : ClawEvent
}
