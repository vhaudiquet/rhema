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
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
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

// The session inventory, the design's sidebar as a screen: grouped
// interactive-first rows with an honest state dot and the inbox depth.
// Displayed names drop the `interactive:` prefix; the full key is what
// gets opened.
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
                is ClawResult.Ok -> sessions = result.value
                is ClawResult.HttpError -> error = UiMessage.Http(result.code, result.body)
                is ClawResult.Network -> error = UiMessage.Network(result.message)
            }
            loading = false
        }
    }

    LaunchedEffect(Unit) { refresh() }

    Column(modifier = Modifier.fillMaxSize()) {
        PageHeader(
            title = stringResource(R.string.sessions_title),
            actions = {
                GhostIcon(Icons.Filled.Settings, stringResource(R.string.sessions_open_settings), onClick = onOpenSettings)
                GhostIcon(Icons.Filled.Refresh, stringResource(R.string.sessions_refresh), enabled = !loading, onClick = refresh)
            },
        )

        val currentError = error
        val visible = sessions?.let(::displayable)
        when {
            currentError != null -> Column(
                modifier = Modifier.padding(horizontal = 16.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                AppCallout(
                    tone = CalloutTone.Error,
                    text = when (currentError) {
                        is UiMessage.Http -> stringResource(R.string.sessions_http_error, currentError.code) +
                            (currentError.detail?.let { " — $it" } ?: "")
                        is UiMessage.Network -> stringResource(R.string.sessions_network_error, currentError.message)
                    },
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
            visible != null && visible.isEmpty() -> Text(
                stringResource(R.string.sessions_empty),
                modifier = Modifier.padding(horizontal = 16.dp),
                style = MaterialTheme.typography.bodyMedium,
                color = JadeiteText3,
            )
            visible != null -> SessionsList(
                sessions = visible,
                onOpenSession = onOpenSession,
            )
        }
    }
}

private data class SessionGroup(val labelKey: String, val rows: List<SessionInfo>)

private const val GROUP_INTERACTIVE = "interactive"
private const val GROUP_OTHER = "other"

// `phone:<device>:<topic>` keys are this device's own status reports,
// not conversations — listed they read as broken empty sessions, so
// they never reach a group.
private fun displayable(sessions: List<SessionInfo>): List<SessionInfo> =
    sessions.filterNot { it.key.startsWith("phone:") }

// Interactive keys are the chat surface and lead; other dispatch keys
// read after. Key order is stable within a group.
private fun groupSessions(sessions: List<SessionInfo>): List<SessionGroup> {
    val byKey = compareBy<SessionInfo> { it.key }
    val visible = displayable(sessions)
    val interactive = visible.filter { it.key.startsWith("interactive:") }.sortedWith(byKey)
    val others = visible.filterNot { it.key.startsWith("interactive:") }.sortedWith(byKey)
    val groups = mutableListOf<SessionGroup>()
    if (interactive.isNotEmpty()) {
        groups += SessionGroup(GROUP_INTERACTIVE, interactive)
    }
    if (others.isNotEmpty()) {
        groups += SessionGroup(GROUP_OTHER, others)
    }
    return groups
}

@Composable
private fun SessionsList(sessions: List<SessionInfo>, onOpenSession: (String) -> Unit) {
    LazyColumn(modifier = Modifier.fillMaxSize()) {
        groupSessions(sessions).forEach { group ->
            item(key = group.labelKey) {
                SectionLabel(
                    stringResource(
                        if (group.labelKey == GROUP_INTERACTIVE) {
                            R.string.sessions_group_interactive
                        } else {
                            R.string.sessions_group_other
                        },
                    ),
                    modifier = Modifier.padding(start = 16.dp, top = 12.dp, bottom = 4.dp),
                )
            }
            items(group.rows, key = { it.key }) { session ->
                SessionRow(session) { onOpenSession(session.key) }
            }
        }
    }
}

@Composable
private fun SessionRow(session: SessionInfo, onClick: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        StatusDot(tone = stateTone(session.state))
        Column(modifier = Modifier.weight(1f)) {
            Text(
                session.key.removePrefix("interactive:"),
                style = MaterialTheme.typography.titleMedium,
                maxLines = 1,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
            )
            Text(
                listOfNotNull(
                    session.state,
                    session.revision?.let { "rev $it" },
                ).joinToString(" · "),
                style = MaterialTheme.typography.bodySmall,
                fontFamily = JadeiteMono,
                color = JadeiteText3,
            )
        }
        val depth = session.inboxDepth
        if (depth != null && depth > 0) {
            AppBadge(depth.toString(), tint = JadeiteInfo)
        }
    }
}

/** Leased keys are alive, reacquiring ones are troubled, the rest idle. */
private fun stateTone(state: String?): DotTone = when (state) {
    "leased" -> DotTone.Ok
    "reacquire" -> DotTone.Warn
    else -> DotTone.Idle
}
