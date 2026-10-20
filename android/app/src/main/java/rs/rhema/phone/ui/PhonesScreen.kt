package rs.rhema.phone.ui

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.ui.Modifier
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import rs.rhema.phone.R
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.net.PhoneInfo
import rs.rhema.phone.sensors.PhoneSensors
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull

/**
 * The fleet management card: every device the claw has heard from,
 * this one marked, every other forgettable. The claw's
 * `GET`/`DELETE /v1/phones` are the whole story — the app holds no
 * private view of the fleet, so what this shows is what any client sees.
 */
@Composable
fun PhonesCard(container: AppContainer) {
    val scope = rememberCoroutineScope()
    var phones by remember { mutableStateOf<List<PhoneInfo>?>(null) }
    var loadError by remember { mutableStateOf<String?>(null) }
    var forgetCandidate by remember { mutableStateOf<PhoneInfo?>(null) }
    var forgotName by remember { mutableStateOf<String?>(null) }
    val deviceId by container.settings.deviceIdFlow.collectAsState(initial = "")
    // The last run that landed every post. The fleet is read from the claw,
    // so this device's last-seen only moves when a report has actually
    // reached it; reloading on this flow keeps the phone from calling
    // itself hours stale while it is in hand.
    val lastReportAt by container.status.lastReportAt.collectAsState(initial = 0L)

    fun load() {
        scope.launch {
            when (val result = container.api.phones()) {
                is ClawResult.Ok -> {
                    phones = result.value.phones
                    loadError = null
                }
                is ClawResult.HttpError ->
                    loadError = "HTTP ${result.code}" + (result.body?.let { " — $it" } ?: "")
                is ClawResult.Network -> loadError = result.message
            }
        }
    }

    // First paint loads the fleet as-is; every landed status run reloads it,
    // so the row this phone reads about itself carries the new last-seen.
    LaunchedEffect(lastReportAt) { load() }

    RhemaCard(title = stringResource(R.string.card_phones), meta = stringResource(R.string.phones_hint)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            val currentError = loadError
            val list = phones
            when {
                currentError != null -> AppCallout(
                    tone = CalloutTone.Error,
                    text = stringResource(R.string.providers_error, currentError),
                )
                list == null -> Text(
                    stringResource(R.string.phones_loading),
                    style = MaterialTheme.typography.bodySmall,
                    color = JadeiteText3,
                )
                list.isEmpty() -> Text(
                    stringResource(R.string.phones_empty),
                    style = MaterialTheme.typography.bodySmall,
                    color = JadeiteText3,
                )
                else -> {
                    forgotName?.let { name ->
                        AppCallout(tone = CalloutTone.Ok, text = stringResource(R.string.phones_forgot_fmt, name))
                    }
                    list.forEachIndexed { index, phone ->
                        val mine = phone.device == deviceId
                        CardRow(
                            title = phone.label ?: shortDeviceId(phone.device),
                            description = fleetRowDescription(phone, mine),
                            first = index == 0 && forgotName == null,
                        ) {
                            if (mine) {
                                AppBadge(stringResource(R.string.phones_this_device))
                            } else {
                                OutlinedButton(
                                    onClick = { forgetCandidate = phone },
                                ) {
                                    Text(stringResource(R.string.phones_forget))
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    forgetCandidate?.let { candidate ->
        val name = candidate.label ?: shortDeviceId(candidate.device)
        AlertDialog(
            onDismissRequest = { forgetCandidate = null },
            title = { Text(stringResource(R.string.phones_forget_title, name)) },
            text = { Text(stringResource(R.string.phones_forget_text)) },
            confirmButton = {
                Button(
                    onClick = {
                        forgetCandidate = null
                        scope.launch {
                            when (val result = container.api.forgetPhone(candidate.device)) {
                                is ClawResult.Ok -> {
                                    forgotName = name
                                    load()
                                }
                                is ClawResult.HttpError ->
                                    loadError = "HTTP ${result.code}" + (result.body?.let { " — $it" } ?: "")
                                is ClawResult.Network -> loadError = result.message
                            }
                        }
                    },
                ) { Text(stringResource(R.string.phones_forget)) }
            },
            dismissButton = {
                TextButton(onClick = { forgetCandidate = null }) {
                    Text(stringResource(R.string.providers_discard))
                }
            },
        )
    }
}

/**
 * What this phone reports and what it cannot until the human flips a
 * grant: location (a runtime permission) and the two special-app
 * toggles Android requires for usage stats and DND control. The state
 * re-reads on every ON_RESUME, so coming back from the settings app
 * flips the row without a restart.
 */
@Composable
fun ReportingCard() {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    var resumeKey by remember { mutableIntStateOf(0) }
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) resumeKey++
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    val locationOn = remember(resumeKey) { PhoneSensors.hasLocation(context) }
    val usageOn = remember(resumeKey) { PhoneSensors.hasUsageAccess(context) }
    val dndOn = remember(resumeKey) {
        context.getSystemService(NotificationManager::class.java)
            ?.isNotificationPolicyAccessGranted == true
    }

    RhemaCard(
        title = stringResource(R.string.card_reporting),
        meta = stringResource(R.string.reporting_hint),
    ) {
        CardRow(
            title = stringResource(R.string.reporting_location_title),
            description = stringResource(R.string.reporting_location_desc),
            first = true,
        ) {
            GrantControl(locationOn) { openAppSettings(context) }
        }
        CardRow(
            title = stringResource(R.string.reporting_usage_title),
            description = stringResource(R.string.reporting_usage_desc),
        ) {
            GrantControl(usageOn) { openUsageAccessSettings(context) }
        }
        CardRow(
            title = stringResource(R.string.reporting_dnd_title),
            description = stringResource(R.string.reporting_dnd_desc),
        ) {
            GrantControl(dndOn) {
                context.startActivity(Intent(Settings.ACTION_NOTIFICATION_POLICY_ACCESS_SETTINGS))
            }
        }
    }
}

/** On reads as a quiet badge; off as a button into the system settings. */
@Composable
private fun GrantControl(on: Boolean, onAllow: () -> Unit) {
    if (on) {
        AppBadge(stringResource(R.string.reporting_on), tint = JadeiteOk)
    } else {
        OutlinedButton(onClick = onAllow) {
            Text(stringResource(R.string.reporting_allow))
        }
    }
}

private fun openAppSettings(context: Context) {
    context.startActivity(
        Intent(
            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
            Uri.fromParts("package", context.packageName, null),
        ),
    )
}

private fun openUsageAccessSettings(context: Context) {
    context.startActivity(Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS))
}

/**
 * A fleet row's one-line state: battery, network, current app, and how
 * long ago any of it landed.
 */
@Composable
private fun fleetRowDescription(phone: PhoneInfo, mine: Boolean): String {
    val parts = mutableListOf<String>()

    (phone.topics["battery"]?.field("value"))?.contentOrNull?.let { level ->
        val charging = phone.topics["battery"]?.field("charging")?.booleanOrNull == true
        parts += if (charging) "$level% charging" else "$level%"
    }
    phone.topics["network"]?.field("value")?.contentOrNull?.let { parts += it }
    phone.topics["app"]?.field("label")?.contentOrNull?.let { parts += it }
        ?: phone.topics["app"]?.field("value")?.contentOrNull
            // No label on the payload: the package's last segment is the
            // closest readable name ("maps" out of com.google.android.maps).
            ?.let { pkg -> pkg.substringAfterLast('.').ifEmpty { pkg } }
            ?.let { parts += it }

    val seen = when {
        phone.lastSeen <= 0L -> stringResource(R.string.phones_never_reported)
        System.currentTimeMillis() - phone.lastSeen < 60_000 ->
            stringResource(R.string.phones_seen_just_now)
        else -> stringResource(R.string.phones_seen_fmt, relativeTime(phone.lastSeen))
    }
    if (mine && parts.isEmpty()) parts += stringResource(R.string.phones_never_reported)

    val summary = parts.joinToString(" · ")
    return if (summary.isEmpty()) seen else "$summary · $seen"
}

private fun JsonObject.field(name: String): JsonPrimitive? = this[name] as? JsonPrimitive

/** A UUID is noise; its first block is enough to tell rows apart. */
private fun shortDeviceId(device: String): String = device.take(8)

private fun relativeTime(thenMillis: Long): String {
    val minutes = ((System.currentTimeMillis() - thenMillis) / 60000L).coerceAtLeast(0)
    return when {
        minutes < 60 -> "${minutes}m"
        minutes < 60 * 24 -> "${minutes / 60}h"
        else -> "${minutes / (60 * 24)}d"
    }
}
