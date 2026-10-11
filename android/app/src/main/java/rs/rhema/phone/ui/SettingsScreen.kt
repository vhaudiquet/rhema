package rs.rhema.phone.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
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
import rs.rhema.phone.R
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.net.ActivePair
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.net.ModelCard
import rs.rhema.phone.net.ProvidersState
import kotlinx.coroutines.launch

// Settings in the design language: the connection card (first launch
// lands here and stays until GET /healthz succeeds), then — once the
// claw is reachable — the Model providers card, the same draft → save
// flow the web UI runs over the public /v1/providers routes.
@Composable
fun SettingsScreen(container: AppContainer, onConnected: () -> Unit = {}) {
    val scope = rememberCoroutineScope()
    var baseUrl by rememberSaveable { mutableStateOf("") }
    var token by rememberSaveable { mutableStateOf("") }
    var busy by rememberSaveable { mutableStateOf(false) }
    var status by remember { mutableStateOf<UiMessage?>(null) }
    val deviceId by container.settings.deviceIdFlow.collectAsState(initial = "…")
    val configured by container.settings.configured.collectAsState(initial = null)

    LaunchedEffect(Unit) {
        val saved = container.settings.current()
        if (saved.baseUrl.isNotBlank()) baseUrl = saved.baseUrl
        if (saved.token.isNotBlank()) token = saved.token
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
    ) {
        PageHeader(title = stringResource(R.string.settings_page_title))

        ConnectionCard(
            baseUrl = baseUrl,
            onBaseUrl = { baseUrl = it },
            token = token,
            onToken = { token = it },
            busy = busy,
            onConnect = {
                busy = true
                status = null
                scope.launch {
                    container.settings.setCredentials(baseUrl.trim(), token.trim())
                    when (val result = container.api.health()) {
                        is ClawResult.Ok -> onConnected()
                        is ClawResult.HttpError -> status = UiMessage.Http(result.code, result.body)
                        is ClawResult.Network -> status = UiMessage.Network(result.message)
                    }
                    busy = false
                }
            },
            status = status,
            deviceId = deviceId,
        )

        // The providers card needs a reachable claw; hidden on first launch.
        if (configured == true) {
            Spacer(Modifier.height(20.dp))
            ProvidersCard(container)
        }
    }
}

@Composable
private fun ConnectionCard(
    baseUrl: String,
    onBaseUrl: (String) -> Unit,
    token: String,
    onToken: (String) -> Unit,
    busy: Boolean,
    onConnect: () -> Unit,
    status: UiMessage?,
    deviceId: String,
) {
    RhemaCard(title = stringResource(R.string.card_connection)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            OutlinedTextField(
                value = baseUrl,
                onValueChange = onBaseUrl,
                label = { Text(stringResource(R.string.field_base_url)) },
                singleLine = true,
                enabled = !busy,
                modifier = Modifier.fillMaxWidth(),
                shape = MaterialTheme.shapes.small,
                colors = sunkenFieldColors(),
            )
            OutlinedTextField(
                value = token,
                onValueChange = onToken,
                label = { Text(stringResource(R.string.field_token)) },
                visualTransformation = PasswordVisualTransformation(),
                singleLine = true,
                enabled = !busy,
                modifier = Modifier.fillMaxWidth(),
                shape = MaterialTheme.shapes.small,
                colors = sunkenFieldColors(),
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(enabled = !busy && baseUrl.isNotBlank(), onClick = onConnect) {
                    Text(stringResource(R.string.save_and_connect))
                }
            }
            val currentStatus = status
            if (currentStatus != null) {
                val text: String = when (currentStatus) {
                    is UiMessage.Http -> stringResource(R.string.settings_health_http, currentStatus.code) +
                        (currentErrorDetail(currentStatus) ?: "")
                    is UiMessage.Network -> stringResource(R.string.settings_health_network, currentStatus.message)
                }
                AppCallout(tone = CalloutTone.Error, text = text)
            }
            Text(
                stringResource(R.string.device_id_fmt, deviceId),
                style = MaterialTheme.typography.bodySmall,
                fontFamily = JadeiteMono,
                color = JadeiteText3,
            )
            Text(
                stringResource(R.string.settings_hint),
                style = MaterialTheme.typography.bodySmall,
                color = JadeiteText3,
            )
        }
    }
}

private fun currentErrorDetail(status: UiMessage.Http): String? =
    status.detail?.let { " — $it" }

/** The Model providers card: the staged pair, its listing, and Save/Discard. */
@Composable
private fun ProvidersCard(container: AppContainer) {
    val scope = rememberCoroutineScope()
    var state by remember { mutableStateOf<ProvidersState?>(null) }
    var loadError by remember { mutableStateOf<String?>(null) }
    var listing by remember { mutableStateOf<ListingState>(ListingState.Idle) }
    var draftProvider by remember { mutableStateOf<String?>(null) }
    var draftModel by remember { mutableStateOf(DEFAULT_MODEL) }
    var saving by remember { mutableStateOf(false) }
    var savedPair by remember { mutableStateOf<ActivePair?>(null) }
    var saveError by remember { mutableStateOf<String?>(null) }

    fun defaultOf(id: String?): String =
        state?.providers?.firstOrNull { it.id == id }?.defaultModel ?: ""

    fun loadModels(id: String) {
        listing = ListingState.Loading
        scope.launch {
            when (val result = container.api.providerModels(id)) {
                is ClawResult.Ok -> listing = ListingState.Available(result.value.models)
                is ClawResult.HttpError ->
                    // A 404 is the documented "no live listing", not a
                    // failure: the instance still switches on its default model.
                    listing = ListingState.Unavailable("HTTP ${result.code}")
                is ClawResult.Network -> listing = ListingState.Unavailable(result.message)
            }
        }
    }

    LaunchedEffect(Unit) {
        when (val result = container.api.providers()) {
            is ClawResult.Ok -> {
                state = result.value
                val active = result.value.active
                draftProvider = active.provider
                draftModel = if (active.model == defaultOf(active.provider)) DEFAULT_MODEL else active.model
                loadModels(active.provider)
            }
            is ClawResult.HttpError -> loadError = "HTTP ${result.code}" + (result.body?.let { " — $it" } ?: "")
            is ClawResult.Network -> loadError = result.message
        }
    }

    val active = state?.active
    val dirty = active != null && draftProvider != null && (
        draftProvider != active.provider ||
            (draftModel.ifBlank { defaultOf(draftProvider) }) != active.model
        )

    RhemaCard(
        title = stringResource(R.string.card_providers),
        meta = stringResource(R.string.providers_hint),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            val currentLoadError = loadError
            val currentState = state
            when {
                currentLoadError != null -> AppCallout(
                    tone = CalloutTone.Error,
                    text = stringResource(R.string.providers_error, currentLoadError),
                )
                currentState == null -> Text(
                    stringResource(R.string.providers_loading),
                    style = MaterialTheme.typography.bodySmall,
                    color = JadeiteText3,
                )
                else -> {
                    Text(
                        stringResource(R.string.providers_field_provider),
                        style = MaterialTheme.typography.labelMedium,
                        color = JadeiteText3,
                    )
                    RhemaSelect(
                        selected = draftProvider,
                        options = currentState.providers.map { it.id to it.id },
                        enabled = !saving,
                        onSelect = { id ->
                            draftProvider = id
                            draftModel = DEFAULT_MODEL
                            loadModels(id)
                        },
                    )

                    Text(
                        stringResource(R.string.providers_field_model),
                        style = MaterialTheme.typography.labelMedium,
                        color = JadeiteText3,
                    )
                    RhemaSelect(
                        selected = draftModel,
                        options = when (val l = listing) {
                            is ListingState.Available ->
                                listOf(DEFAULT_MODEL to stringResource(R.string.providers_default_option)) +
                                    l.models.map { it.id to it.id }
                            else -> listOf(DEFAULT_MODEL to stringResource(R.string.providers_default_option))
                        },
                        enabled = !saving,
                        onSelect = { draftModel = it },
                    )
                    if (listing is ListingState.Unavailable) {
                        Text(
                            stringResource(R.string.providers_none),
                            style = MaterialTheme.typography.bodySmall,
                            color = JadeiteText3,
                        )
                    }

                    if (dirty) {
                        AppCallout(
                            tone = CalloutTone.Warn,
                            title = stringResource(R.string.providers_staged),
                            text = stringResource(R.string.providers_staged_detail),
                        )
                    }
                    savedPair?.let { pair ->
                        AppCallout(
                            tone = CalloutTone.Ok,
                            text = stringResource(R.string.providers_saved, pair.provider, pair.model),
                        )
                    }
                    saveError?.let { err ->
                        AppCallout(
                            tone = CalloutTone.Error,
                            text = stringResource(R.string.providers_error, err),
                        )
                    }

                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(
                            enabled = dirty && !saving,
                            onClick = {
                                val activeNow = state?.active ?: return@OutlinedButton
                                draftProvider = activeNow.provider
                                draftModel =
                                    if (activeNow.model == defaultOf(activeNow.provider)) DEFAULT_MODEL else activeNow.model
                                savedPair = null
                                saveError = null
                            },
                        ) {
                            Text(stringResource(R.string.providers_discard))
                        }
                        Button(
                            enabled = dirty && !saving && draftProvider != null,
                            onClick = {
                                val provider = draftProvider ?: return@Button
                                saving = true
                                savedPair = null
                                saveError = null
                                scope.launch {
                                    when (
                                        val result = container.api.setActiveProvider(
                                            provider,
                                            draftModel.ifBlank { null },
                                        )
                                    ) {
                                        is ClawResult.Ok -> {
                                            // The route answers the new pair only;
                                            // carry the provider list forward.
                                            val current = state
                                            state = if (current != null) {
                                                current.copy(active = result.value.active)
                                            } else {
                                                ProvidersState(result.value.active, emptyList())
                                            }
                                            savedPair = result.value.active
                                            draftProvider = result.value.active.provider
                                            draftModel = if (
                                                result.value.active.model == defaultOf(result.value.active.provider)
                                            ) {
                                                DEFAULT_MODEL
                                            } else {
                                                result.value.active.model
                                            }
                                        }
                                        is ClawResult.HttpError ->
                                            saveError = "HTTP ${result.code}" + (result.body?.let { " — $it" } ?: "")
                                        is ClawResult.Network -> saveError = result.message
                                    }
                                    saving = false
                                }
                            },
                        ) {
                            Text(stringResource(R.string.providers_save))
                        }
                    }
                }
            }
        }
    }
}

private sealed interface ListingState {
    data object Idle : ListingState
    data object Loading : ListingState
    data class Available(val models: List<ModelCard>) : ListingState
    data class Unavailable(val reason: String) : ListingState
}

private const val DEFAULT_MODEL = ""

/** The design's input voice: sunken ground, hairline border, accent focus. */
@Composable
private fun sunkenFieldColors() = OutlinedTextFieldDefaults.colors(
    focusedBorderColor = JadeiteAccent,
    unfocusedBorderColor = JadeiteBorder,
    focusedContainerColor = JadeiteSunken,
    unfocusedContainerColor = JadeiteSunken,
    cursorColor = JadeiteAccentText,
    focusedTextColor = JadeiteText,
    unfocusedTextColor = JadeiteText,
)
