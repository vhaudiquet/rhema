package rs.rhema.phone.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
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
import androidx.compose.ui.unit.dp
import rs.rhema.phone.R
import rs.rhema.phone.chat.ChatController
import rs.rhema.phone.di.AppContainer
import kotlinx.coroutines.launch

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
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
        ) {
            IconButton(onClick = onBack) {
                Icon(
                    Icons.AutoMirrored.Filled.ArrowBack,
                    contentDescription = stringResource(R.string.chat_back),
                )
            }
            Column(modifier = Modifier.weight(1f)) {
                Text(sessionKey, style = MaterialTheme.typography.titleMedium, maxLines = 1)
                ConnectionLabel(controller.connection)
            }
        }
        HorizontalDivider()

        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .padding(horizontal = 16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(controller.items, key = { it.id }) { item -> TranscriptRow(item) }
        }
        LaunchedEffect(controller.items.size) {
            if (controller.items.isNotEmpty()) {
                listState.animateScrollToItem(controller.items.size - 1)
            }
        }

        controller.sendError?.let { err ->
            Text(
                stringResource(R.string.chat_send_failed, err),
                color = MaterialTheme.colorScheme.error,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
            )
        }

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(8.dp),
            verticalAlignment = Alignment.Bottom,
        ) {
            OutlinedTextField(
                value = draft,
                onValueChange = { draft = it },
                modifier = Modifier.weight(1f),
                placeholder = { Text(stringResource(R.string.chat_message_hint)) },
                maxLines = 4,
            )
            Spacer(Modifier.width(8.dp))
            Button(
                enabled = !controller.sending && draft.isNotBlank(),
                onClick = {
                    val text = draft
                    draft = ""
                    scope.launch {
                        if (!controller.send(text)) draft = text
                    }
                },
            ) {
                Text(stringResource(R.string.chat_send))
            }
        }
    }
}

@Composable
private fun ConnectionLabel(conn: ChatController.Conn) {
    val text = when (conn) {
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
    Text(
        text,
        style = MaterialTheme.typography.labelSmall,
        color = if (conn is ChatController.Conn.Failed) {
            MaterialTheme.colorScheme.error
        } else {
            MaterialTheme.colorScheme.onSurfaceVariant
        },
    )
}

@Composable
private fun TranscriptRow(item: ChatController.Item) {
    when (item) {
        is ChatController.UserMessage -> Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.End,
        ) {
            Surface(
                color = MaterialTheme.colorScheme.primaryContainer,
                shape = MaterialTheme.shapes.medium,
            ) {
                Text(item.text, modifier = Modifier.padding(10.dp))
            }
        }
        is ChatController.Thinking -> {
            // Dim and collapsible: it is the agent's scratch, not its answer.
            var expanded by remember(item.id) { mutableStateOf(false) }
            Text(
                if (expanded || item.text.length <= 120) item.text else "${item.text.take(120)}…",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { expanded = !expanded },
            )
        }
        is ChatController.ToolCall -> Column(modifier = Modifier.fillMaxWidth()) {
            Text(
                if (item.running) {
                    stringResource(R.string.chat_tool_running, item.name)
                } else {
                    stringResource(R.string.chat_tool_done, item.name)
                },
                style = MaterialTheme.typography.labelMedium,
                color = if (item.isError) {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.primary
                },
            )
            item.summary?.takeIf { it.isNotBlank() }?.let { summary ->
                Text(
                    summary,
                    style = MaterialTheme.typography.bodySmall,
                    color = if (item.isError) {
                        MaterialTheme.colorScheme.error
                    } else {
                        MaterialTheme.colorScheme.onSurfaceVariant
                    },
                )
            }
        }
        is ChatController.Reply -> Surface(
            color = MaterialTheme.colorScheme.surfaceVariant,
            shape = MaterialTheme.shapes.medium,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(
                item.text + if (item.streaming) "…" else "",
                modifier = Modifier.padding(10.dp),
            )
        }
    }
}
