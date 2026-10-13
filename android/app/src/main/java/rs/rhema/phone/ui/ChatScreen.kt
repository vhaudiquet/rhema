package rs.rhema.phone.ui

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.graphics.BitmapFactory
import android.net.Uri
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import rs.rhema.phone.R
import rs.rhema.phone.chat.ChatController
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.net.ClawResult
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException

// The transcript screen, the design's chat panel as a phone screen:
// avatar rows per turn (the tool cards with their one-sided accent
// border, dim collapsible thinking, the outcome tag), and a composer
// with the round accent send button. The claw owns the conversation —
// this renders what the event stream delivered, nothing more.
@Composable
fun ChatScreen(container: AppContainer, sessionKey: String, onBack: () -> Unit) {
    val scope = rememberCoroutineScope()
    val controller = remember(sessionKey) { ChatController(container.api, scope, sessionKey) }
    // The SSE connection lives exactly as long as this screen is composed.
    DisposableEffect(sessionKey) {
        controller.start()
        onDispose { controller.stop() }
    }
    BackHandler { onBack() }

    var draft by rememberSaveable { mutableStateOf("") }

    Column(modifier = Modifier.fillMaxSize()) {
        ChatHeader(sessionKey, controller.connection, onBack)
        HorizontalDivider(color = JadeiteBorder)

        // Reversed layout: index 0 is the newest turn, pinned to the
        // bottom edge. A streaming reply grows against that anchor, so
        // the newest text stays on screen without scroll bookkeeping,
        // and a reader scrolled into history is never yanked back.
        LazyColumn(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth(),
            reverseLayout = true,
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            items(turnGroups(controller.items).asReversed(), key = { it.key }) { group ->
                TurnGroupRow(group, container)
            }
        }

        controller.sendError?.let { err ->
            Text(
                stringResource(R.string.chat_send_failed, err),
                color = JadeiteError,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
            )
        }

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 10.dp),
            verticalAlignment = Alignment.Bottom,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = draft,
                onValueChange = { draft = it },
                modifier = Modifier.weight(1f),
                placeholder = { Text(stringResource(R.string.chat_message_hint)) },
                maxLines = 4,
                shape = MaterialTheme.shapes.small,
                colors = OutlinedTextFieldDefaults.colors(
                    focusedBorderColor = JadeiteAccent,
                    unfocusedBorderColor = JadeiteBorder,
                    focusedContainerColor = JadeiteSunken,
                    unfocusedContainerColor = JadeiteSunken,
                    cursorColor = JadeiteAccentText,
                    focusedTextColor = JadeiteText,
                    unfocusedTextColor = JadeiteText,
                ),
            )
            SendButton(
                enabled = !controller.sending && draft.isNotBlank(),
            ) {
                val text = draft
                draft = ""
                scope.launch {
                    if (!controller.send(text)) draft = text
                }
            }
        }
    }
}

@Composable
private fun ChatHeader(sessionKey: String, conn: ChatController.Conn, onBack: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        modifier = Modifier.padding(horizontal = 8.dp, vertical = 8.dp),
    ) {
        IconButton(onClick = onBack) {
            Icon(
                Icons.AutoMirrored.Filled.ArrowBack,
                contentDescription = stringResource(R.string.chat_back),
                tint = JadeiteText2,
            )
        }
        AppAvatar("R", accent = true, size = 34.dp)
        Column(modifier = Modifier.weight(1f)) {
            Text(
                sessionKey.removePrefix("interactive:"),
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                StatusDot(tone = connTone(conn))
                Text(
                    connText(conn),
                    style = MaterialTheme.typography.labelSmall,
                    color = if (connTone(conn) == DotTone.Error) JadeiteError else JadeiteText3,
                )
            }
        }
    }
}

private fun connTone(conn: ChatController.Conn): DotTone = when (conn) {
    ChatController.Conn.Connecting -> DotTone.Warn
    ChatController.Conn.Live -> DotTone.Ok
    ChatController.Conn.Reconnecting -> DotTone.Error
    is ChatController.Conn.Failed -> DotTone.Error
}

@Composable
private fun connText(conn: ChatController.Conn): String = when (conn) {
    ChatController.Conn.Connecting -> stringResource(R.string.chat_status_connecting)
    ChatController.Conn.Live -> stringResource(R.string.chat_status_live)
    ChatController.Conn.Reconnecting -> stringResource(R.string.chat_status_reconnecting)
    is ChatController.Conn.Failed ->
        if (conn.code == 401) {
            stringResource(R.string.chat_status_unauthorized)
        } else {
            stringResource(R.string.chat_status_failed, conn.message)
        }
}

@Composable
private fun SendButton(enabled: Boolean, onClick: () -> Unit) {
    Surface(
        shape = CircleShape,
        color = if (enabled) JadeiteAccent else JadeiteElevated,
        modifier = Modifier
            .size(44.dp)
            .clickable(enabled = enabled, onClick = onClick),
    ) {
        Box(contentAlignment = Alignment.Center) {
            Icon(
                Icons.AutoMirrored.Filled.Send,
                contentDescription = stringResource(R.string.chat_send),
                tint = if (enabled) JadeiteOnAccent else JadeiteText3,
                modifier = Modifier.size(20.dp),
            )
        }
    }
}

// One turn as rendered: a user message, or the agent's stack of
// thinking / tool cards / reply / outcome under one avatar. The flat
// item list folds here — consecutive agent items belong to one turn.
private sealed interface TurnGroup {
    val key: String

    data class User(val id: Long, val text: String) : TurnGroup {
        override val key: String = "u$id"
    }

    data class Agent(val items: MutableList<ChatController.Item>) : TurnGroup {
        override val key: String = "a${items.first().id}"
    }
}

private fun turnGroups(items: List<ChatController.Item>): List<TurnGroup> {
    val groups = mutableListOf<TurnGroup>()
    for (item in items) {
        when (item) {
            is ChatController.UserMessage -> groups += TurnGroup.User(item.id, item.text)
            else -> {
                val last = groups.lastOrNull()
                if (last is TurnGroup.Agent) last.items += item else groups += TurnGroup.Agent(mutableListOf(item))
            }
        }
    }
    return groups
}

@Composable
private fun TurnGroupRow(group: TurnGroup, container: AppContainer) {
    when (group) {
        is TurnGroup.User -> Row(
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            AppAvatar("·", accent = false)
            Column {
                Text(
                    stringResource(R.string.chat_you),
                    style = MaterialTheme.typography.labelMedium,
                    fontWeight = FontWeight.Bold,
                    color = JadeiteText2,
                )
                // Embedded pastes render as chips, not payloads; the text
                // around them stays verbatim.
                var chip = 0
                pasteParts(group.text).forEach { part ->
                    when (part) {
                        is PastePart.Text -> Text(part.text, style = MaterialTheme.typography.bodyLarge)
                        is PastePart.Paste -> {
                            chip += 1
                            PasteChip(chipLabel(part.payload, chip))
                        }
                    }
                }
            }
        }
        is TurnGroup.Agent -> Row(
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            AppAvatar("R", accent = true)
            Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    stringResource(R.string.chat_agent),
                    style = MaterialTheme.typography.labelMedium,
                    fontWeight = FontWeight.Bold,
                    color = if (group.items.any { it is ChatController.Reply && it.streaming }) {
                        JadeiteAccentText
                    } else {
                        JadeiteText2
                    },
                )
                // The design stacks a turn as thinking, tool cards, then the
                // reply and its outcome — stream order interleaves them, the
                // rendering does not.
                group.items.sortedBy { turnPriority(it) }.forEach { item ->
                    when (item) {
                        is ChatController.Thinking -> ThinkingBlock(item)
                        is ChatController.ToolCall ->
                            if (isShellTool(item.name)) ShellRow(item) else ToolCard(item)
                        is ChatController.Attachment -> AttachmentCard(item, container)
                        is ChatController.Reply -> ReplyBlock(item)
                        is ChatController.Outcome -> OutcomeTag(item.outcome)
                        is ChatController.UserMessage -> Unit // never grouped here
                    }
                }
            }
        }
    }
}

// Render order inside one agent turn: thinking, tools, attachments,
// reply, outcome.
private fun turnPriority(item: ChatController.Item): Int = when (item) {
    is ChatController.Thinking -> 0
    is ChatController.ToolCall -> 1
    is ChatController.Attachment -> 2
    is ChatController.Reply -> 3
    is ChatController.Outcome -> 4
    is ChatController.UserMessage -> 5
}

@Composable
private fun ThinkingBlock(item: ChatController.Thinking) {
    // Dim and collapsible: it is the agent's scratch, not its answer.
    var expanded by remember(item.id) { mutableStateOf(false) }
    val text = if (expanded || item.text.length <= 120) item.text else "${item.text.take(120)}…"
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { expanded = !expanded }
            .background(JadeiteSurface, MaterialTheme.shapes.extraSmall)
            .padding(8.dp),
    ) {
        Text(
            "thinking",
            style = MaterialTheme.typography.labelSmall,
            color = JadeiteText3,
        )
        Text(
            text,
            style = MaterialTheme.typography.bodySmall,
            fontStyle = FontStyle.Italic,
            color = JadeiteText3,
        )
    }
}

@Composable
private fun ToolCard(item: ChatController.ToolCall) {
    val tint = when {
        item.running -> JadeiteText3
        item.isError -> JadeiteError
        else -> JadeiteOk
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 32.dp)
            .height(IntrinsicSize.Min)
            .background(JadeiteSurface, MaterialTheme.shapes.extraSmall),
    ) {
        AccentBar(tint)
        Row(
            modifier = Modifier
                .padding(horizontal = 10.dp, vertical = 6.dp)
                .weight(1f),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(
                if (item.running) "…" else if (item.isError) "✗" else "✓",
                style = MaterialTheme.typography.labelMedium,
                color = tint,
            )
            Text(
                item.name,
                style = MaterialTheme.typography.labelMedium,
                fontFamily = JadeiteMono,
                color = JadeiteText,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            item.summary?.takeIf { it.isNotBlank() }?.let { summary ->
                Text(
                    summary,
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = JadeiteMono,
                    color = JadeiteText3,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

/** Command rows render as shell lines, not glyph rows (the desktop's
 * shell_line); run_command is the bash tool's other name. */
private fun isShellTool(name: String): Boolean = name == "bash" || name == "run_command"

@Composable
private fun ShellRow(item: ChatController.ToolCall) {
    val tint = when {
        item.running -> JadeiteText3
        item.exitCode == 0L && !item.isError -> JadeiteOk
        else -> JadeiteError
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 32.dp)
            .height(IntrinsicSize.Min)
            .background(JadeiteSurface, MaterialTheme.shapes.extraSmall),
    ) {
        AccentBar(tint)
        Row(
            modifier = Modifier
                .padding(horizontal = 10.dp, vertical = 6.dp)
                .weight(1f),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            // A shell line reads as the command it ran: dim $ prefix, the
            // command, the exit code at its end.
            Text(
                "$",
                style = MaterialTheme.typography.labelMedium,
                fontFamily = JadeiteMono,
                color = JadeiteText3,
            )
            Text(
                item.command ?: "…",
                style = MaterialTheme.typography.labelMedium,
                fontFamily = JadeiteMono,
                color = JadeiteText,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            when {
                item.running -> Unit
                item.exitCode != null -> Text(
                    "[${item.exitCode}]",
                    style = MaterialTheme.typography.labelMedium,
                    fontFamily = JadeiteMono,
                    color = if (item.exitCode == 0L) JadeiteOk else JadeiteError,
                )
                item.isError -> Text(
                    "✗",
                    style = MaterialTheme.typography.labelMedium,
                    color = JadeiteError,
                )
            }
        }
    }
}

@Composable
private fun ReplyBlock(item: ChatController.Reply) {
    AgentMarkdown(
        text = item.text,
        streaming = item.streaming,
    )
}

@Composable
private fun OutcomeTag(outcome: String) {
    Text(
        outcome,
        style = MaterialTheme.typography.labelSmall,
        fontFamily = JadeiteMono,
        color = if (outcome.equals("Failed", ignoreCase = true)) JadeiteError else JadeiteText3,
    )
}

@Composable
private fun PasteChip(label: String) {
    Text(
        label,
        style = MaterialTheme.typography.labelSmall,
        fontFamily = JadeiteMono,
        color = JadeiteText3,
        modifier = Modifier
            .background(JadeiteSunken, MaterialTheme.shapes.extraSmall)
            .padding(horizontal = 8.dp, vertical = 2.dp),
    )
}

// --- embedded pastes --------------------------------------------------
// The wire form a long paste travels in: MARK, payload, MARK — one
// logical unit inside otherwise plain text (rhema-core's paste module
// is the definition; these are its client-side shape). A mark with no
// partner is literal text.

private const val PASTE_MARK = '\uFFFC'

/** One scanned piece of embedded-form text. */
private sealed interface PastePart {
    data class Text(val text: String) : PastePart
    data class Paste(val payload: String) : PastePart
}

/** Splits embedded-form text into its parts, left to right. A mark with
 * no partner is literal text; malformed input never throws. */
private fun pasteParts(text: String): List<PastePart> {
    val parts = mutableListOf<PastePart>()
    var rest = text
    while (true) {
        val start = rest.indexOf(PASTE_MARK)
        if (start < 0) break
        val after = rest.substring(start + 1)
        val end = after.indexOf(PASTE_MARK)
        if (end < 0) {
            parts += PastePart.Text(rest.substring(0, start + 1))
            rest = after
            continue
        }
        if (start > 0) parts += PastePart.Text(rest.substring(0, start))
        parts += PastePart.Paste(after.substring(0, end))
        rest = after.substring(end + 1)
    }
    if (rest.isNotEmpty()) parts += PastePart.Text(rest)
    return parts
}

/** Rust `str::lines`: \n-split, and a trailing newline ends the last
 * line instead of opening an empty one. */
private fun lineCount(text: String): Int {
    if (text.isEmpty()) return 0
    val lines = text.split('\n')
    return if (lines.last().isEmpty()) lines.size - 1 else lines.size
}

/** A chip's label: `▣1 5L` — the anchor's one-based id and the
 * payload's line count. */
private fun chipLabel(paste: String, chip: Int): String = "▣$chip ${lineCount(paste)}L"

// --- attachments ------------------------------------------------------

// Only small images preview inline: there is no image loader in the
// app, so the preview is a BitmapFactory decode of the fetched bytes.
private const val INLINE_IMAGE_MAX_BYTES = 2L * 1024 * 1024

private sealed interface AttachState {
    data object Idle : AttachState
    data object Loading : AttachState
    data class Ready(val uri: Uri, val preview: ImageBitmap?, val note: String? = null) : AttachState
    data class Failed(val message: String) : AttachState
}

@Composable
private fun AttachmentCard(item: ChatController.Attachment, container: AppContainer) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var state by remember(item.attachmentId) { mutableStateOf<AttachState>(AttachState.Idle) }

    // One fetch: bytes → cache file → shareable uri; small images also
    // decode to the inline preview. open hands the uri to ACTION_VIEW.
    suspend fun download(open: Boolean) {
        state = AttachState.Loading
        when (val result = container.api.attachment(item.attachmentId)) {
            is ClawResult.Ok -> {
                val prepared = withContext(Dispatchers.IO) {
                    val uri = saveAttachment(context, item.attachmentId, item.name, result.value)
                    when {
                        uri == null -> AttachState.Failed(
                            context.getString(R.string.chat_attachment_failed, "save failed"),
                        )
                        item.media == "image" && result.value.size <= INLINE_IMAGE_MAX_BYTES -> {
                            val preview = BitmapFactory.decodeByteArray(
                                result.value,
                                0,
                                result.value.size,
                            )?.asImageBitmap()
                            AttachState.Ready(uri, preview)
                        }
                        else -> AttachState.Ready(uri, preview = null)
                    }
                }
                state = prepared
                if (prepared is AttachState.Ready && open && !openAttachment(context, prepared.uri)) {
                    state = prepared.copy(note = context.getString(R.string.chat_attachment_open_failed))
                }
            }
            is ClawResult.HttpError ->
                state = AttachState.Failed(context.getString(R.string.chat_attachment_failed, "HTTP ${result.code}"))
            is ClawResult.Network ->
                state = AttachState.Failed(context.getString(R.string.chat_attachment_failed, result.message))
        }
    }

    // Small images fetch themselves on first composition; everything else
    // waits for the download row.
    LaunchedEffect(item.attachmentId) {
        if (item.media == "image" && item.size in 1L..INLINE_IMAGE_MAX_BYTES && state == AttachState.Idle) {
            download(open = false)
        }
    }

    val current = state
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 32.dp)
            .height(IntrinsicSize.Min)
            .background(JadeiteSurface, MaterialTheme.shapes.extraSmall),
    ) {
        AccentBar(
            when (current) {
                AttachState.Loading -> JadeiteText3
                is AttachState.Failed -> JadeiteError
                else -> JadeiteOk
            },
        )
        Column(
            modifier = Modifier
                .padding(horizontal = 10.dp, vertical = 6.dp)
                .weight(1f),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text(
                    item.name,
                    style = MaterialTheme.typography.labelMedium,
                    fontFamily = JadeiteMono,
                    color = JadeiteText,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                Text(
                    formatBytes(item.size),
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = JadeiteMono,
                    color = JadeiteText3,
                )
            }
            item.caption?.takeIf { it.isNotBlank() }?.let { caption ->
                Text(caption, style = MaterialTheme.typography.bodySmall, color = JadeiteText2)
            }
            if (current is AttachState.Ready) {
                current.preview?.let { preview ->
                    Image(
                        bitmap = preview,
                        contentDescription = item.name,
                        contentScale = ContentScale.Fit,
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 240.dp),
                    )
                }
                current.note?.let { note ->
                    Text(note, style = MaterialTheme.typography.bodySmall, color = JadeiteError)
                }
            }
            if (current is AttachState.Failed) {
                Text(current.message, style = MaterialTheme.typography.bodySmall, color = JadeiteError)
            }
            if (current is AttachState.Loading) {
                Text(
                    stringResource(R.string.chat_attachment_downloading),
                    style = MaterialTheme.typography.labelMedium,
                    color = JadeiteText3,
                )
            } else {
                Text(
                    when (current) {
                        is AttachState.Ready -> stringResource(R.string.chat_attachment_open)
                        else -> stringResource(R.string.chat_attachment_download)
                    },
                    style = MaterialTheme.typography.labelMedium,
                    color = JadeiteAccentText,
                    modifier = Modifier.clickable {
                        when (val s = state) {
                            is AttachState.Ready -> {
                                if (!openAttachment(context, s.uri)) {
                                    state = s.copy(note = context.getString(R.string.chat_attachment_open_failed))
                                }
                            }
                            else -> scope.launch { download(open = true) }
                        }
                    },
                )
            }
        }
    }
}

/** Saves the bytes under cache/attachments/ and answers the shareable
 * content:// uri; other apps can read the file only through the
 * FileProvider mapping, so the name (its extension drives the mime)
 * must survive the save. Null when the write failed. */
private fun saveAttachment(context: Context, id: String, name: String, bytes: ByteArray): Uri? {
    val dir = File(context.cacheDir, "attachments")
    if (!dir.exists() && !dir.mkdirs()) return null
    val safe = name.substringAfterLast('/').substringAfterLast('\\').trim().ifBlank { "attachment" }
    return try {
        val file = File(dir, "$id-$safe")
        file.writeBytes(bytes)
        FileProvider.getUriForFile(context, context.packageName + ".fileprovider", file)
    } catch (e: IOException) {
        null
    }
}

/** Hands a saved attachment to another app. False when nothing on the
 * phone opens it — the card says so, and the file stays for a retry. */
private fun openAttachment(context: Context, uri: Uri): Boolean = try {
    val mime = context.contentResolver.getType(uri) ?: "application/octet-stream"
    context.startActivity(
        Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, mime)
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK),
    )
    true
} catch (e: ActivityNotFoundException) {
    false
}

/** B/KB/MB, integer — the mono token beside the file name. */
private fun formatBytes(size: Long): String = when {
    size < 1024 -> "$size B"
    size < 1024 * 1024 -> "${size / 1024} KB"
    else -> "${size / (1024 * 1024)} MB"
}
