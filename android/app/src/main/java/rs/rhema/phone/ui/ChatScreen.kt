package rs.rhema.phone.ui

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.graphics.BitmapFactory
import android.net.Uri
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
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
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import androidx.core.net.toUri
import rs.rhema.phone.R
import rs.rhema.phone.chat.ChatController
import rs.rhema.phone.chat.MAX_IMAGE_BYTES
import rs.rhema.phone.chat.MAX_STAGED_IMAGES
import rs.rhema.phone.data.AgentIdentityCache
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.net.ClawResult
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException
import java.util.Locale
import android.provider.OpenableColumns

// The transcript screen, the design's chat panel as a phone screen:
// avatar rows per turn (the tool cards with their one-sided accent
// border, dim collapsible thinking, the outcome tag), and a composer
// with the round accent send button. The claw owns the conversation —
// this renders what the event stream delivered, nothing more.
@Composable
fun ChatScreen(container: AppContainer, sessionKey: String, onBack: () -> Unit) {
    val scope = rememberCoroutineScope()
    val controller = remember(sessionKey) { ChatController(container.api, container.sessionCache, scope, sessionKey) }
    // The SSE connection lives exactly as long as this screen is composed.
    DisposableEffect(sessionKey) {
        controller.start()
        onDispose { controller.stop() }
    }
    BackHandler { onBack() }

    var draft by rememberSaveable { mutableStateOf("") }

    // Images staged for the next send, held as files in the app cache:
    // the picker's read grant dies with the activity result, while the
    // send reads the staged file whenever the message leaves.
    var staged by remember { mutableStateOf<List<StagedChip>>(emptyList()) }
    var stageError by remember { mutableStateOf<ImageStageError?>(null) }
    var attachOpen by remember { mutableStateOf(false) }
    // The camera's content:// target, remembered so the result callback
    // still knows what was written even after a configuration change.
    var cameraTarget by rememberSaveable { mutableStateOf<String?>(null) }
    val context = LocalContext.current
    // Labels resolve in composition, not at callback time: a lookup
    // through LocalContext.current freezes the first composition's
    // locale (the same rule the attachment card's templates follow).
    val cameraLabel = stringResource(R.string.chat_attach_camera)
    val cameraShotName = stringResource(R.string.chat_camera_shot_name)

    // Stage one image for the next send: the budget and the type are
    // checked here — loudly, never a silent drop — the bytes copied into
    // the cache's staged dir, and a downscaled chip preview decoded off
    // the main thread.
    fun stageImage(uri: Uri, name: String, mediaType: String?) {
        stageError = null
        if (staged.size >= MAX_STAGED_IMAGES) {
            stageError = ImageStageError.TooMany(name, MAX_STAGED_IMAGES)
            return
        }
        val type = mediaType?.takeIf { it in IMAGE_MEDIA_TYPES }
            ?: MIME_BY_EXTENSION[name.substringAfterLast('.', "").lowercase()]
        if (type == null) {
            stageError = ImageStageError.BadType(name)
            return
        }
        scope.launch {
            when (val outcome = stageImageTask(context, uri, name, type)) {
                is StageOutcome.Staged -> staged = staged + outcome.chip
                is StageOutcome.Failed -> stageError = outcome.error
            }
        }
    }

    val galleryLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.PickVisualMedia(),
    ) { uri ->
        if (uri != null) {
            val type = context.contentResolver.getType(uri)
            val name = queryDisplayName(context, uri) ?: "photo." + typeFallbackExtension(type)
            stageImage(uri, name, type)
        }
    }

    val cameraLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.TakePicture(),
    ) { shot ->
        val target = cameraTarget
        if (shot && target != null) {
            stageImage(target.toUri(), cameraShotName, "image/jpeg")
        }
    }

    // The claw's presentation identity: the avatar picture (when one is
    // set), the initials glyph otherwise, and the agent's name on every
    // turn — the same face the web UI shows.
    val face = rememberAgentFace(container)

    Column(modifier = Modifier.fillMaxSize()) {
        ChatHeader(sessionKey, controller.connection, face, onBack)
        HorizontalDivider(color = JadeiteBorder)

        // Reversed layout: index 0 is the newest turn, pinned to the
        // bottom edge. A streaming reply grows against that anchor, so
        // the newest text stays on screen without scroll bookkeeping,
        // and a reader scrolled into history is never yanked back.
        val listState = rememberLazyListState()
        // Older turns are fetched, not cached: the claw holds the record
        // and the screen opens on its newest turn, so the walk back
        // starts when the reader reaches the top (higher indices are
        // older here) or when the transcript is too short to fill the
        // screen — which reads as "the last visible item is the last
        // item" either way.
        LaunchedEffect(listState, controller) {
            snapshotFlow {
                val info = listState.layoutInfo
                info.visibleItemsInfo.lastOrNull()?.index to info.totalItemsCount
            }
                .distinctUntilChanged()
                .collect { (last, total) ->
                    if (last != null && total > 0 && last >= total - 2) controller.loadOlder()
                }
        }
        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth(),
            reverseLayout = true,
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            items(turnGroups(controller.items).asReversed(), key = { it.key }) { group ->
                TurnGroupRow(group, container, controller, face)
            }
            // The last declared item sits at the top of a reversed list.
            if (controller.loadingOlder) {
                item(key = "older") {
                    Text(
                        stringResource(R.string.chat_loading_earlier),
                        style = MaterialTheme.typography.labelSmall,
                        fontFamily = JadeiteMono,
                        color = JadeiteText3,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
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

        // The staged chips sit above the input row like the web composer's
        // paste-chip row: thumbnail, ▣N name size, and a drop affordance.
        if (staged.isNotEmpty()) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp)
                    .horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.Bottom,
            ) {
                staged.forEachIndexed { index, chip ->
                    StagedChipView(chip, index + 1) { staged = staged - chip }
                }
            }
        }

        stageError?.let { error ->
            ImageErrorRow(error) { stageError = null }
        }

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 10.dp),
            verticalAlignment = Alignment.Bottom,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            IconButton(
                onClick = { attachOpen = true },
                enabled = !controller.sending,
            ) {
                Icon(
                    Icons.Filled.Add,
                    contentDescription = stringResource(R.string.chat_attach),
                    tint = JadeiteText3,
                )
            }
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
                val sending = staged
                draft = ""
                staged = emptyList()
                stageError = null
                scope.launch {
                    if (!controller.send(text, sending.map { it.image })) {
                        draft = text
                        staged = sending
                    }
                }
            }
        }

        // The attach door: gallery pick or camera shot, the only two ways
        // images enter the composer.
        if (attachOpen) {
            AlertDialog(
                onDismissRequest = { attachOpen = false },
                title = { Text(stringResource(R.string.chat_attach)) },
                text = {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(
                            onClick = {
                                attachOpen = false
                                galleryLauncher.launch(
                                    PickVisualMediaRequest(
                                        ActivityResultContracts.PickVisualMedia.ImageOnly,
                                    ),
                                )
                            },
                            modifier = Modifier.weight(1f),
                        ) {
                            Text(stringResource(R.string.chat_attach_gallery))
                        }
                        OutlinedButton(
                            onClick = {
                                attachOpen = false
                                newCameraTarget(context)?.let { target ->
                                    cameraTarget = target.toString()
                                    cameraLauncher.launch(target)
                                } ?: run {
                                    stageError = ImageStageError.ReadFailed(cameraLabel)
                                }
                            },
                            modifier = Modifier.weight(1f),
                        ) {
                            Text(stringResource(R.string.chat_attach_camera))
                        }
                    }
                },
                confirmButton = {},
            )
        }
    }
}

@Composable
private fun ChatHeader(
    sessionKey: String,
    conn: ChatController.Conn,
    face: AgentFace,
    onBack: () -> Unit,
) {
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
        AppAvatar(face.glyph, accent = face.image == null, size = 34.dp, image = face.image)
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

    data class User(
        val id: Long,
        val text: String,
        val images: List<ChatController.SentImage>,
    ) : TurnGroup {
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
            is ChatController.UserMessage -> groups += TurnGroup.User(item.id, item.text, item.images)
            else -> {
                val last = groups.lastOrNull()
                if (last is TurnGroup.Agent) last.items += item else groups += TurnGroup.Agent(mutableListOf(item))
            }
        }
    }
    return groups
}

@Composable
private fun TurnGroupRow(
    group: TurnGroup,
    container: AppContainer,
    controller: ChatController,
    face: AgentFace,
) {
    when (group) {
        is TurnGroup.User -> Row(
            horizontalArrangement = Arrangement.End,
            modifier = Modifier.fillMaxWidth(),
        ) {
            // The reader's own words: a right-anchored bubble, elevated
            // one step and marked on its trailing edge — no label or
            // avatar, the side says whose it is.
            Row(
                modifier = Modifier
                    .weight(1f, fill = false)
                    .height(IntrinsicSize.Min)
                    .background(JadeiteElevated, MaterialTheme.shapes.small),
            ) {
                Column(
                    modifier = Modifier
                        .padding(horizontal = 10.dp, vertical = 8.dp)
                        .weight(1f, fill = false),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    // The message's images sit above its text, the web
                    // sent row's layout: a locally sent row previews its
                    // own staged files, a replayed one fetches by id.
                    if (group.images.isNotEmpty()) {
                        Row(
                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                            modifier = Modifier.horizontalScroll(rememberScrollState()),
                        ) {
                            group.images.forEach { image ->
                                SentImageThumb(image, container)
                            }
                        }
                    }
                    // Embedded pastes render as chips, not payloads; the
                    // text around them stays verbatim.
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
                AccentBar(JadeiteAccent)
            }
        }
        is TurnGroup.Agent -> Row(
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            AppAvatar(face.glyph, accent = face.image == null, image = face.image)
            Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    face.name ?: stringResource(R.string.chat_agent),
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
                        is ChatController.Consent -> ConsentCard(item, controller)
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
// questions, reply, outcome.
private fun turnPriority(item: ChatController.Item): Int = when (item) {
    is ChatController.Thinking -> 0
    is ChatController.ToolCall -> 1
    is ChatController.Attachment -> 2
    is ChatController.Consent -> 3
    is ChatController.Reply -> 4
    is ChatController.Outcome -> 5
    is ChatController.UserMessage -> 6
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
    // A steered turn was cut short by the next message, not failed: the
    // reply that replaced it streams right below, so the tag reads as an
    // accent, not an error — the same distinction the web UI draws.
    val color = when {
        outcome.equals("Failed", ignoreCase = true) -> JadeiteError
        outcome.equals("Steered", ignoreCase = true) -> JadeiteAccentText
        else -> JadeiteText3
    }
    Text(
        outcome,
        style = MaterialTheme.typography.labelSmall,
        fontFamily = JadeiteMono,
        color = color,
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

// --- staged images ----------------------------------------------------

// One staged image as the composer holds it: the sendable part (file,
// wire type, decoded size) plus what only the UI needs — the label and
// the downscaled preview.
private data class StagedChip(
    val image: ChatController.StagedImage,
    val name: String,
    val thumb: ImageBitmap?,
)

// Why an image refused to stage — failure data, resolved to text at
// render (the UiMessage rule), never a silent drop.
private sealed interface ImageStageError {
    data class TooMany(val name: String, val max: Int) : ImageStageError
    data class TooLarge(val name: String) : ImageStageError
    data class BadType(val name: String) : ImageStageError
    data class ReadFailed(val name: String) : ImageStageError
}

private sealed interface StageOutcome {
    data class Staged(val chip: StagedChip) : StageOutcome
    data class Failed(val error: ImageStageError) : StageOutcome
}

// The claw accepts png, jpeg, gif, and webp only; anything else the
// picker offers (heic, raw camera types) refuses loudly at stage time.
private val IMAGE_MEDIA_TYPES = listOf("image/png", "image/jpeg", "image/gif", "image/webp")

private val MIME_BY_EXTENSION = mapOf(
    "png" to "image/png",
    "jpg" to "image/jpeg",
    "jpeg" to "image/jpeg",
    "gif" to "image/gif",
    "webp" to "image/webp",
)

// Chip previews at staging, transcript previews at render — both far
// under the decoded sizes they stand for.
private const val STAGE_THUMB_PX = 256
private const val TRANSCRIPT_THUMB_PX = 512

/** Copies a picked or shot image into the cache's staged dir, size-checked
 * against the claw's budget, with a downscaled chip preview decoded off
 * the main thread. Every refusal answers a failure the composer shows. */
private suspend fun stageImageTask(
    context: Context,
    uri: Uri,
    name: String,
    mediaType: String,
): StageOutcome = withContext(Dispatchers.IO) {
    // The read can fail on a dead grant (SecurityException) or a gone
    // source (IOException); both stage as unreadable, both visible.
    val bytes = try {
        context.contentResolver.openInputStream(uri)?.use { stream -> stream.readBytes() }
    } catch (e: IOException) {
        null
    } catch (e: SecurityException) {
        null
    }
    when {
        bytes == null -> StageOutcome.Failed(ImageStageError.ReadFailed(name))
        bytes.size > MAX_IMAGE_BYTES -> StageOutcome.Failed(ImageStageError.TooLarge(name))
        else -> {
            val dir = File(context.cacheDir, "staged")
            when {
                !dir.exists() && !dir.mkdirs() ->
                    StageOutcome.Failed(ImageStageError.ReadFailed(name))
                else -> try {
                    // Synthetic on-disk name, never shown: the chip and
                    // the sent row label carry the pick's display name.
                    val file = File(
                        dir,
                        "staged-${System.currentTimeMillis()}.${stagedExtension(mediaType)}",
                    )
                    file.writeBytes(bytes)
                    StageOutcome.Staged(
                        StagedChip(
                            image = ChatController.StagedImage(file, mediaType, bytes.size.toLong()),
                            name = name,
                            thumb = decodeThumbnail(bytes, STAGE_THUMB_PX),
                        ),
                    )
                } catch (e: IOException) {
                    StageOutcome.Failed(ImageStageError.ReadFailed(name))
                }
            }
        }
    }
}

/** The staged file's extension, from its accepted media type. */
private fun stagedExtension(mediaType: String): String =
    if (mediaType == "image/jpeg") "jpg" else mediaType.removePrefix("image/")

/** A label's extension when the provider named no type: the image
 * subtype when usable, png otherwise. */
private fun typeFallbackExtension(mediaType: String?): String =
    when (mediaType?.removePrefix("image/")) {
        "gif" -> "gif"
        "webp" -> "webp"
        "jpeg" -> "jpg"
        else -> "png"
    }

/** The pick's display name from the media provider; null when it names
 * nothing — the caller falls back to a type-derived one. */
private fun queryDisplayName(context: Context, uri: Uri): String? = runCatching {
    context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
        ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }
}.getOrNull()

/** A fresh cache file for a camera shot, mapped through the manifest's
 * FileProvider: TakePicture writes straight into the content:// uri, and
 * it needs no CAMERA permission. Null when the cache directory cannot
 * be made — the composer surfaces that, it never drops silently. */
private fun newCameraTarget(context: Context): Uri? {
    val dir = File(context.cacheDir, "camera")
    if (!dir.exists() && !dir.mkdirs()) return null
    val file = File(dir, "shot-${System.currentTimeMillis()}.jpg")
    return FileProvider.getUriForFile(context, context.packageName + ".fileprovider", file)
}

/** A downscaled decode: inSampleSize halves until the longer side is
 * near [maxDim], so a 12 MP camera shot never becomes a full bitmap in
 * memory. Null when the bytes decode to nothing. */
private fun decodeThumbnail(bytes: ByteArray, maxDim: Int): ImageBitmap? {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
    var sample = 1
    while (bounds.outWidth / (sample * 2) >= maxDim || bounds.outHeight / (sample * 2) >= maxDim) {
        sample *= 2
    }
    val options = BitmapFactory.Options().apply { inSampleSize = sample }
    return BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)?.asImageBitmap()
}

/** B/KB/MB with one decimal past 1 KB — the chip label's size, the web
 * composer's format. */
private fun formatSize(size: Long): String = when {
    size < 1024 -> "$size B"
    size < 1024 * 1024 -> String.format(Locale.US, "%.1f KB", size / 1024.0)
    else -> String.format(Locale.US, "%.1f MB", size / (1024.0 * 1024.0))
}

// A staged image's chip, the paste chip's sibling: downscaled thumbnail,
// the ▣N name size label, and a drop affordance.
@Composable
private fun StagedChipView(chip: StagedChip, number: Int, onRemove: () -> Unit) {
    Surface(
        shape = MaterialTheme.shapes.extraSmall,
        color = JadeiteSunken,
        border = BorderStroke(1.dp, JadeiteBorder),
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            modifier = Modifier.padding(start = 6.dp, end = 6.dp, top = 4.dp, bottom = 4.dp),
        ) {
            chip.thumb?.let { thumb ->
                Image(
                    bitmap = thumb,
                    contentDescription = chip.name,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier
                        .size(28.dp)
                        .clip(MaterialTheme.shapes.extraSmall),
                )
            }
            Text(
                "▣$number ${chip.name} ${formatSize(chip.image.sizeBytes)}",
                style = MaterialTheme.typography.labelSmall,
                fontFamily = JadeiteMono,
                color = JadeiteText3,
            )
            Icon(
                Icons.Filled.Close,
                contentDescription = stringResource(R.string.chat_image_drop, chip.name),
                tint = JadeiteText3,
                modifier = Modifier
                    .size(14.dp)
                    .clickable(onClick = onRemove),
            )
        }
    }
}

// A refused image's inline error with its dismiss — the web composer's
// role=alert row.
@Composable
private fun ImageErrorRow(error: ImageStageError, onDismiss: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 2.dp),
    ) {
        Text(
            when (error) {
                is ImageStageError.TooMany ->
                    stringResource(R.string.chat_image_too_many, error.name, error.max)
                is ImageStageError.TooLarge ->
                    stringResource(R.string.chat_image_too_large, error.name)
                is ImageStageError.BadType ->
                    stringResource(R.string.chat_image_bad_type, error.name)
                is ImageStageError.ReadFailed ->
                    stringResource(R.string.chat_image_read_failed, error.name)
            },
            color = JadeiteError,
            style = MaterialTheme.typography.bodySmall,
            modifier = Modifier.weight(1f),
        )
        Icon(
            Icons.Filled.Close,
            contentDescription = stringResource(R.string.chat_image_dismiss),
            tint = JadeiteText3,
            modifier = Modifier
                .size(16.dp)
                .clickable(onClick = onDismiss),
        )
    }
}

// One image a sent row carries: the local file decodes in place; a
// replayed row fetches its bytes bearer-guarded once per row lifetime —
// the attachment card's own caching pattern. The preview is a downscaled
// decode, capped at the attachment card's 240 dp.
@Composable
private fun SentImageThumb(image: ChatController.SentImage, container: AppContainer) {
    var thumb by remember(image.attachmentId, image.file) { mutableStateOf<ImageBitmap?>(null) }
    LaunchedEffect(image.attachmentId, image.file) {
        thumb = withContext(Dispatchers.IO) {
            val bytes = when {
                image.file != null -> runCatching { image.file.readBytes() }.getOrNull()
                image.attachmentId != null -> when (
                    val result = container.api.attachment(image.attachmentId)
                ) {
                    is ClawResult.Ok -> result.value
                    else -> null
                }
                else -> null
            }
            bytes?.let { decodeThumbnail(it, TRANSCRIPT_THUMB_PX) }
        }
    }
    thumb?.let { bitmap ->
        Image(
            bitmap = bitmap,
            contentDescription = stringResource(R.string.chat_sent_image),
            contentScale = ContentScale.Fit,
            modifier = Modifier
                .widthIn(max = 240.dp)
                .heightIn(max = 240.dp)
                .clip(MaterialTheme.shapes.small)
                .border(1.dp, JadeiteBorder, MaterialTheme.shapes.small),
        )
    }
}

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
    // Templates resolve in composition, not at callback time: a lookup
    // through LocalContext.current freezes the locale of the first
    // composition.
    val failedTemplate = stringResource(R.string.chat_attachment_failed)
    val openFailed = stringResource(R.string.chat_attachment_open_failed)
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
                            failedTemplate.format("save failed"),
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
                    state = prepared.copy(note = openFailed)
                }
            }
            is ClawResult.HttpError ->
                state = AttachState.Failed(failedTemplate.format("HTTP ${result.code}"))
            is ClawResult.Network ->
                state = AttachState.Failed(failedTemplate.format(result.message))
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
                                    state = s.copy(note = openFailed)
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

@Composable
private fun ConsentCard(item: ChatController.Consent, controller: ChatController) {
    val scope = rememberCoroutineScope()
    var secret by remember(item.id) { mutableStateOf("") }
    var busy by remember(item.id) { mutableStateOf(false) }
    var failure by remember(item.id) { mutableStateOf<String?>(null) }

    // An open question glows warn; a settled card keeps only its tone.
    val tint = when (item.granted) {
        null -> JadeiteWarn
        true -> JadeiteOk
        false -> JadeiteText3
    }

    fun answer(call: suspend () -> ClawResult<Unit>) {
        busy = true
        scope.launch {
            failure = when (val result = call()) {
                is ClawResult.Ok -> null
                is ClawResult.HttpError -> "HTTP ${result.code}" + (result.body?.let { " — $it" } ?: "")
                is ClawResult.Network -> result.message
            }
            busy = false
        }
    }

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(IntrinsicSize.Min)
            .background(JadeiteSurface, MaterialTheme.shapes.extraSmall),
    ) {
        AccentBar(tint)
        Column(
            modifier = Modifier
                .padding(horizontal = 10.dp, vertical = 6.dp)
                .weight(1f),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                AppBadge(item.kind, tint = tint)
                Text(
                    item.summary,
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.weight(1f, fill = false),
                )
            }
            Text(item.detail, style = MaterialTheme.typography.bodySmall, color = JadeiteText2)
            when (item.granted) {
                true -> Text(
                    "✓ " + stringResource(R.string.consent_granted),
                    style = MaterialTheme.typography.labelMedium,
                    color = JadeiteOk,
                )
                false -> Text(
                    "✗ " + stringResource(R.string.consent_denied),
                    style = MaterialTheme.typography.labelMedium,
                    color = JadeiteText3,
                )
                null -> {
                    if (item.kind == "secret") {
                        OutlinedTextField(
                            value = secret,
                            onValueChange = { secret = it },
                            modifier = Modifier.fillMaxWidth(),
                            placeholder = { Text(stringResource(R.string.consent_secret_hint)) },
                            visualTransformation = PasswordVisualTransformation(),
                            singleLine = true,
                            enabled = !busy,
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
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (item.kind == "secret") {
                            Button(
                                enabled = !busy && secret.isNotBlank(),
                                onClick = { answer { controller.answerSecret(item.consentId, secret) } },
                            ) {
                                Text(stringResource(R.string.consent_submit))
                            }
                        } else {
                            Button(
                                enabled = !busy,
                                onClick = { answer { controller.grantPhone(item.consentId, true) } },
                            ) {
                                Text(stringResource(R.string.consent_approve))
                            }
                        }
                        OutlinedButton(
                            enabled = !busy,
                            onClick = { answer { controller.denyConsent(item.consentId) } },
                        ) {
                            Text(stringResource(R.string.consent_deny))
                        }
                    }
                }
            }
            failure?.let { fail ->
                Text(
                    stringResource(R.string.consent_answer_failed, fail),
                    style = MaterialTheme.typography.bodySmall,
                    color = JadeiteError,
                )
            }
        }
    }
}

// The agent's face as this screen renders it: the display name (null
// keeps the string resource's fallback), the initials glyph, and the
// decoded avatar image. The glyph precedence is the web's: the claw's
// `initials`, else the name's first letter, else "R".
private data class AgentFace(val name: String?, val glyph: String, val image: ImageBitmap?)

@Composable
private fun rememberAgentFace(container: AppContainer): AgentFace {
    val cache by container.identity.cached.collectAsState(AgentIdentityCache())
    var image by remember { mutableStateOf<ImageBitmap?>(null) }
    // Decoding off the main thread: an uploaded avatar can weigh the
    // claw's full 2 MB image cap.
    LaunchedEffect(cache.avatar) {
        image = cache.avatar?.let { bytes ->
            withContext(Dispatchers.Default) {
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size)?.asImageBitmap()
            }
        }
    }
    val identity = cache.identity
    val name = identity?.name?.trim()?.takeIf { it.isNotEmpty() }
    val glyph = identity?.initials?.trim()?.takeIf { it.isNotEmpty() }
        ?: name?.take(1)
        ?: "R"
    return AgentFace(name, glyph, image)
}
