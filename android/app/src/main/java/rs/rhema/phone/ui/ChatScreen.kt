package rs.rhema.phone.ui

import androidx.activity.compose.BackHandler
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
import androidx.compose.foundation.lazy.rememberLazyListState
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
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import rs.rhema.phone.R
import rs.rhema.phone.chat.ChatController
import rs.rhema.phone.di.AppContainer
import kotlinx.coroutines.launch

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
    val listState = rememberLazyListState()

    Column(modifier = Modifier.fillMaxSize()) {
        ChatHeader(sessionKey, controller.connection, onBack)
        HorizontalDivider(color = JadeiteBorder)

        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            items(turnGroups(controller.items), key = { it.key }) { group ->
                TurnGroupRow(group)
            }
        }
        LaunchedEffect(controller.items.size) {
            if (controller.items.isNotEmpty()) {
                listState.animateScrollToItem(controller.items.size - 1)
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
private fun TurnGroupRow(group: TurnGroup) {
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
                Text(group.text, style = MaterialTheme.typography.bodyLarge)
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
                        is ChatController.ToolCall -> ToolCard(item)
                        is ChatController.Reply -> ReplyBlock(item)
                        is ChatController.Outcome -> OutcomeTag(item.outcome)
                        is ChatController.UserMessage -> Unit // never grouped here
                    }
                }
            }
        }
    }
}

// Render order inside one agent turn: thinking, tools, reply, outcome.
private fun turnPriority(item: ChatController.Item): Int = when (item) {
    is ChatController.Thinking -> 0
    is ChatController.ToolCall -> 1
    is ChatController.Reply -> 2
    is ChatController.Outcome -> 3
    is ChatController.UserMessage -> 4
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
