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

// POST /v1/sessions/{key}/messages. The Json instance skips defaults,
// so an empty image list keeps the old {"text"} body byte-for-byte.
// Text stays required: the claw 422s an empty text even with images.
@Serializable
data class MessageRequest(val text: String, val images: List<OutboundImage> = emptyList())

// One image in the POST body: the full MIME type plus bare base64. The
// claw's budget — at most eight, 5 MiB decoded each, png/jpeg/gif/webp
// only — is enforced client-side at stage time (the composer).
@Serializable
data class OutboundImage(
    @SerialName("media_type") val mediaType: String,
    val data: String,
)

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
    // Whether the model takes image input, when the adapter knows:
    // absent means unknown — no verdict, never a false.
    @SerialName("supports_vision") val supportsVision: Boolean? = null,
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

// GET /v1/phones — the fleet: every device that ever reported, its
// self-reported label, both clocks, and its latest observation per
// topic. This device's row is the one whose `device` equals the local
// device id.
@Serializable
data class PhonesPage(val phones: List<PhoneInfo> = emptyList())

@Serializable
data class PhoneInfo(
    val device: String,
    val label: String? = null,
    @SerialName("first_seen") val firstSeen: Long = 0,
    @SerialName("last_seen") val lastSeen: Long = 0,
    // Topic name to its latest payload (battery, location, app, …),
    // verbatim from the ingest posts. Unknown topics land here too, so
    // an older build still lists a newer claw's data.
    val topics: Map<String, JsonObject> = emptyMap(),
)

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

// One image a user_message names: the attachment id the bytes live
// under (GET /v1/attachments/{id}, bearer-guarded) plus the MIME type —
// megabytes never ride the stream.
@Serializable
data class UserImage(val id: String, @SerialName("media_type") val mediaType: String)

// One event of GET /v1/sessions/{key}/history: the record's seq for it,
// beside the event itself. Seq order is the wire order.
@Serializable
data class HistoryEvent(val seq: Long = -1, val event: ClawEvent? = null)

// One page of whole turns older than the seq the screen holds oldest
// (`before`). `more` false means the page reached the session's first
// event, so the walk back can stop.
@Serializable
data class HistoryPage(val events: List<HistoryEvent> = emptyList(), val more: Boolean = false)

// Frames of GET /v1/sessions/{key}/events; "type" is the discriminator.
// Every frame also names the record's seq in its `id:` line, which is the
// stream's resume cursor. A screen with no cache opens on the newest turn
// and walks back through /history; one with a cache resumes past it.
// Unknown types fail the per-frame parse and are skipped, so the API can grow
// without breaking older clients.
@Serializable
sealed interface ClawEvent {
    @Serializable
    @SerialName("user_message")
    // Rows recorded before images rode the stream carry no field at
    // all; absent folds to none, never a guessed shape.
    data class UserMessage(
        val text: String = "",
        val images: List<UserImage> = emptyList(),
    ) : ClawEvent

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

    // The agent asking the human: kind=secret wants a secret string,
    // kind=phone is approve/deny of a phone control action.
    @Serializable
    @SerialName("consent_request")
    data class ConsentRequest(
        val id: String,
        val kind: String,
        val summary: String,
        val detail: String,
        @SerialName("created_at") val createdAt: Long = 0,
    ) : ClawEvent

    // A question settled — by this surface or another one; the card
    // flips on it whichever way the answer went.
    @Serializable
    @SerialName("consent_resolved")
    data class ConsentResolved(val id: String, val granted: Boolean) : ClawEvent
}

// GET /v1/consents — the pending questions, oldest first.
@Serializable
data class ConsentsPage(val consents: List<ConsentInfo> = emptyList())

@Serializable
data class ConsentInfo(
    val id: String,
    val kind: String,
    val summary: String,
    val detail: String,
    @SerialName("created_at") val createdAt: Long,
)

// POST /v1/consents/{id} — the kind decides the body.
@Serializable
data class SecretConsentAnswer(val secret: String)

@Serializable
data class PhoneConsentAnswer(val grant: Boolean)

// GET /v1/phone/commands — the control queue for this device, oldest
// first. The result POST doubles as the ack: until it lands the claw
// keeps the command queued and the worker repeats it.
@Serializable
data class CommandPage(val commands: List<PhoneCommand> = emptyList())

@Serializable
data class PhoneCommand(
    val id: String,
    val action: String,
    val args: JsonObject = JsonObject(emptyMap()),
    @SerialName("created_at") val createdAt: Long = 0,
)

// POST /v1/phone/commands/{id}/result
@Serializable
data class CommandResult(val ok: Boolean, val detail: String? = null)

// GET /v1/agent — the agent's presentation identity, shared with the web
// UI. Unset text fields are absent on the wire (the claw skips nulls), so
// every field defaults; the images report presence only, their bytes live
// at GET /v1/agent/{kind}.
@Serializable
data class AgentIdentity(
    val name: String? = null,
    val mark: String? = null,
    val initials: String? = null,
    val tagline: String? = null,
    val signoff: String? = null,
    val avatar: Boolean = false,
    val logo: Boolean = false,
)
