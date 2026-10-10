package dev.rhema.phone.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import dev.rhema.phone.R
import dev.rhema.phone.di.AppContainer
import dev.rhema.phone.net.ClawResult
import kotlinx.coroutines.launch

// First launch lands here and stays until GET /healthz succeeds against the
// saved base URL. Credentials are saved before the check so a retry is just
// the button, not a re-type.
@Composable
fun SettingsScreen(container: AppContainer, onConnected: () -> Unit = {}) {
    val scope = rememberCoroutineScope()
    var baseUrl by rememberSaveable { mutableStateOf("") }
    var token by rememberSaveable { mutableStateOf("") }
    var busy by rememberSaveable { mutableStateOf(false) }
    var status by remember { mutableStateOf<UiMessage?>(null) }
    val deviceId by container.settings.deviceIdFlow.collectAsState(initial = "…")

    LaunchedEffect(Unit) {
        val saved = container.settings.current()
        if (saved.baseUrl.isNotBlank()) baseUrl = saved.baseUrl
        if (saved.token.isNotBlank()) token = saved.token
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            stringResource(R.string.settings_title),
            style = MaterialTheme.typography.headlineSmall,
        )
        OutlinedTextField(
            value = baseUrl,
            onValueChange = { baseUrl = it },
            label = { Text(stringResource(R.string.field_base_url)) },
            singleLine = true,
            enabled = !busy,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = token,
            onValueChange = { token = it },
            label = { Text(stringResource(R.string.field_token)) },
            visualTransformation = PasswordVisualTransformation(),
            singleLine = true,
            enabled = !busy,
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            enabled = !busy && baseUrl.isNotBlank(),
            onClick = {
                busy = true
                status = null
                scope.launch {
                    container.settings.setCredentials(baseUrl.trim(), token.trim())
                    when (val result = container.api.health()) {
                        is ClawResult.Ok -> onConnected()
                        is ClawResult.HttpError -> status = UiMessage.Http(result.code)
                        is ClawResult.Network -> status = UiMessage.Network(result.message)
                    }
                    busy = false
                }
            },
        ) {
            Text(stringResource(R.string.save_and_connect))
        }
        val currentStatus = status
        if (currentStatus != null) {
            val text: String = when (currentStatus) {
                is UiMessage.Http -> stringResource(R.string.settings_health_http, currentStatus.code)
                is UiMessage.Network -> stringResource(R.string.settings_health_network, currentStatus.message)
            }
            Text(text, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium)
        }
        Text(
            stringResource(R.string.device_id_fmt, deviceId),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            stringResource(R.string.settings_hint),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
