package dev.rhema.phone.net

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
    @SerialName("last_outcome") val lastOutcome: String? = null,
)

// POST /v1/sessions/{key}/messages
@Serializable
data class MessageRequest(val text: String)

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
