package rs.rhema.phone.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Settings
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonPrimitive
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import rs.rhema.phone.R
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.net.SessionInfo
import kotlinx.coroutines.launch

@Composable
fun SessionsScreen(
    container: AppContainer,
    onOpenSession: (String) -> Unit,
    onOpenSettings: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    var sessions by remember { mutableStateOf<List<SessionInfo>?>(null) }
    var error by remember { mutableStateOf<UiMessage?>(null) }
    var loading by remember { mutableStateOf(false) }

    val refresh: () -> Unit = {
        scope.launch {
            loading = true
            error = null
            when (val result = container.api.sessions()) {
                is ClawResult.Ok ->
                    // Interactive sessions first; key order is stable within a group.
                    sessions = result.value.sortedWith(
                        compareBy({ it.state != "interactive" }, { it.key }),
                    )
                is ClawResult.HttpError -> error = UiMessage.Http(result.code, result.body)
                is ClawResult.Network -> error = UiMessage.Network(result.message)
            }
            loading = false
        }
    }

    LaunchedEffect(Unit) { refresh() }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                stringResource(R.string.sessions_title),
                style = MaterialTheme.typography.headlineSmall,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onOpenSettings) {
                Icon(
                    Icons.Filled.Settings,
                    contentDescription = stringResource(R.string.sessions_open_settings),
                )
            }
            IconButton(onClick = refresh, enabled = !loading) {
                Icon(
                    Icons.Filled.Refresh,
                    contentDescription = stringResource(R.string.sessions_refresh),
                )
            }
        }

        val currentError = error
        when {
            currentError != null -> {
                val text = when (currentError) {
                    is UiMessage.Http -> stringResource(R.string.sessions_http_error, currentError.code) +
                    (currentError.detail?.let { " — $it" } ?: "")
                    is UiMessage.Network -> stringResource(R.string.sessions_network_error, currentError.message)
                }
                Text(
                    text,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
                if (currentError is UiMessage.Http && currentError.code == 401) {
                    Text(
                        stringResource(R.string.sessions_unauthorized_hint),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Button(onClick = onOpenSettings) {
                        Text(stringResource(R.string.sessions_open_settings))
                    }
                }
            }
            loading && sessions == null -> Box(
                modifier = Modifier.fillMaxSize(),
                contentAlignment = Alignment.Center,
            ) { CircularProgressIndicator() }
            sessions?.isEmpty() == true -> Text(
                stringResource(R.string.sessions_empty),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            sessions != null -> LazyColumn(
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(sessions ?: emptyList(), key = { it.key }) { session ->
                    SessionRow(session) { onOpenSession(session.key) }
                }
            }
        }
    }
}

@Composable
private fun SessionRow(session: SessionInfo, onClick: () -> Unit) {
    ElevatedCard(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Text(session.key, style = MaterialTheme.typography.titleMedium)
            Text(
                listOfNotNull(
                    session.state,
                    session.inboxDepth?.let { "inbox: $it" },
                ).joinToString(" · "),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            session.lastOutcome?.let { outcome ->
                val text = outcome.entries.firstOrNull()?.let { (variant, reason) ->
                    reason.takeIf { it is JsonPrimitive && it.isString }
                        ?.let { primitive ->
                            val text = primitive.jsonPrimitive.content
                            text.takeIf { it.isNotBlank() }?.let { "$variant: $text" } ?: variant
                        } ?: variant
                }
                text?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}
