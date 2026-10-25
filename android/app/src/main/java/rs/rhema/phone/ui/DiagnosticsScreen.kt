package rs.rhema.phone.ui

import android.app.ActivityManager
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.work.WorkInfo
import androidx.work.WorkManager
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import rs.rhema.phone.R
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.work.PHONE_COMMANDS
import rs.rhema.phone.work.PHONE_STATUS
import rs.rhema.phone.work.NOTIFICATIONS
import rs.rhema.phone.work.enqueuePhoneStatusNow

/**
 * The background-work health page: what the three scheduled jobs are
 * doing, whether this phone's reports are actually landing on the claw
 * (the two clocks' disagreement is the diagnosis between "the worker is
 * dead", "the claw is down", and "the claw is up but not storing"), the
 * system checks whose wrong setting silences the workers, and the
 * workers' own log. Checks re-read on every ON_RESUME so returning from
 * the settings app flips the row without a restart.
 */
@Composable
fun DiagnosticsContent(container: AppContainer) {
    val lifecycleOwner = LocalLifecycleOwner.current
    var resumeKey by remember { mutableIntStateOf(0) }
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) resumeKey++
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    // Four cards where every other settings page has one: the column owns
    // the spacing the single-card pages never needed.
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(bottom = 24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        WorkCard(container)
        ClocksCard(container, resumeKey)
        ChecksCard(resumeKey)
        LogCard(container)
    }
}

// ---------------------------------------------------------------- work

/**
 * The three periodic jobs' live state, straight from WorkManager. An
 * empty info list is the loud answer: the schedule is gone (cancelled,
 * force-stopped, never enqueued) — the exact "silent for days" state the
 * rest of the app cannot show.
 */
@Composable
private fun WorkCard(container: AppContainer) {
    val context = LocalContext.current
    val wm = remember { WorkManager.getInstance(context) }
    val statusInfos by wm.getWorkInfosForUniqueWorkFlow(PHONE_STATUS).collectAsState(initial = null)
    val notifyInfos by wm.getWorkInfosForUniqueWorkFlow(NOTIFICATIONS).collectAsState(initial = null)
    val commandInfos by wm.getWorkInfosForUniqueWorkFlow(PHONE_COMMANDS).collectAsState(initial = null)

    RhemaCard(title = stringResource(R.string.card_diag_work), meta = stringResource(R.string.diag_work_hint)) {
        WorkRow(stringResource(R.string.diag_work_status), statusInfos, first = true)
        WorkRow(stringResource(R.string.diag_work_notify), notifyInfos)
        WorkRow(stringResource(R.string.diag_work_commands), commandInfos)
        CardRow(
            title = stringResource(R.string.diag_report_now_title),
            description = stringResource(R.string.diag_report_now_desc),
        ) {
            Button(onClick = { enqueuePhoneStatusNow(context) }) {
                Text(stringResource(R.string.diag_report_now))
            }
        }
    }
}

@Composable
private fun WorkRow(label: String, infos: List<WorkInfo>?, first: Boolean = false) {
    // Periodic work holds one info; take the first for both the state and
    // the next window. An empty list means the schedule itself is gone.
    val info = infos?.firstOrNull()
    val stateLabel = when {
        infos == null -> stringResource(R.string.diag_state_checking)
        info == null -> stringResource(R.string.diag_state_not_scheduled)
        else -> when (info.state) {
            WorkInfo.State.ENQUEUED -> stringResource(R.string.diag_state_enqueued)
            WorkInfo.State.RUNNING -> stringResource(R.string.diag_state_running)
            WorkInfo.State.SUCCEEDED -> stringResource(R.string.diag_state_enqueued)
            WorkInfo.State.FAILED -> stringResource(R.string.diag_state_failed)
            WorkInfo.State.BLOCKED -> stringResource(R.string.diag_state_blocked)
            WorkInfo.State.CANCELLED -> stringResource(R.string.diag_state_cancelled)
        }
    }
    val tint = when (info?.state) {
        WorkInfo.State.RUNNING -> JadeiteOk
        WorkInfo.State.FAILED, WorkInfo.State.CANCELLED -> JadeiteError
        WorkInfo.State.BLOCKED -> JadeiteWarn
        null -> JadeiteWarn
        else -> JadeiteText3
    }
    val detail = when {
        infos == null -> null
        info == null -> null
        else -> listOfNotNull(
            info.nextScheduleTimeMillis
                .takeIf { it > 0 && it != Long.MAX_VALUE }
                ?.let { stringResource(R.string.diag_work_next_fmt, inMinutes(it)) },
            info.runAttemptCount
                .takeIf { it > 0 }
                ?.let { stringResource(R.string.diag_work_attempts_fmt, it) },
        ).joinToString(" · ").ifEmpty { null }
    }
    CardRow(title = label, description = detail, first = first) {
        AppBadge(stateLabel, tint = tint)
    }
}

// -------------------------------------------------------------- clocks

/**
 * The two ends of the reporting pipeline, one clock each: when this
 * phone last landed a full report (its own record), and when the claw
 * last stamped this device (the fleet row's `last_seen`, re-read live).
 * The callout names the disagreement, because that disagreement is the
 * whole diagnosis.
 */
@Composable
private fun ClocksCard(container: AppContainer, resumeKey: Int) {
    val lastReportAt by container.status.lastReportAt.collectAsState(initial = 0L)
    // null = unchecked, -1 = checked and the claw has no row for us.
    var clawSeen by remember { mutableStateOf<Long?>(null) }
    var clawError by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(resumeKey, lastReportAt) {
        when (val result = container.api.phones()) {
            is ClawResult.Ok -> {
                val deviceId = container.settings.deviceId()
                clawSeen = result.value.phones
                    .firstOrNull { it.device == deviceId }
                    ?.lastSeen ?: -1L
                clawError = null
            }
            is ClawResult.HttpError -> clawError = "HTTP ${result.code}"
            is ClawResult.Network -> clawError = result.message
        }
    }

    RhemaCard(title = stringResource(R.string.card_diag_clocks)) {
        CardRow(
            title = stringResource(R.string.diag_clock_phone),
            description = stringResource(R.string.diag_clock_phone_desc),
            first = true,
        ) {
            AppBadge(if (lastReportAt > 0) clock(lastReportAt) else stringResource(R.string.diag_clock_never))
        }
        CardRow(
            title = stringResource(R.string.diag_clock_claw),
            description = stringResource(R.string.diag_clock_claw_desc),
        ) {
            val seen = clawSeen
            AppBadge(
                when {
                    clawError != null -> stringResource(R.string.diag_clock_unreachable)
                    seen == null -> stringResource(R.string.diag_state_checking)
                    seen < 0 -> stringResource(R.string.diag_clock_never)
                    else -> clock(seen)
                },
            )
        }

        val currentError = clawError
        val seen = clawSeen
        when {
            currentError != null -> AppCallout(
                tone = CalloutTone.Error,
                text = stringResource(R.string.diag_callout_unreachable, currentError),
            )
            // The claw is behind the phone by more than one cadence: posts
            // are being sent and not stored. One cadence of drift is the
            // schedule's normal jitter; anything more is a lie one of the
            // two ends is telling.
            seen != null && seen > 0 && lastReportAt > seen + NOT_LANDED_MS ->
                AppCallout(
                    tone = CalloutTone.Warn,
                    text = stringResource(R.string.diag_callout_not_landing),
                )
            lastReportAt == 0L -> AppCallout(
                tone = CalloutTone.Info,
                text = stringResource(R.string.diag_callout_never_landed),
            )
        }
    }
}

// -------------------------------------------------------------- checks

/**
 * The system settings that silence background work, each auto-detected
 * and each answered with a jump into the exact settings page when the
 * setting is wrong. Nothing OS buries these three; finding them by hand
 * is the part nobody does.
 */
@Composable
private fun ChecksCard(resumeKey: Int) {
    val context = LocalContext.current
    // Re-read on resume: the point of the fix buttons is coming back from
    // the settings app, so the row must flip without a restart.
    val powerOk = remember(resumeKey) {
        context.getSystemService(PowerManager::class.java)
            ?.isIgnoringBatteryOptimizations(context.packageName) == true
    }
    val backgroundOk = remember(resumeKey) {
        // Background restriction is API 28+; below it the answer is "no
        // such switch", which reads as fine.
        Build.VERSION.SDK_INT < 28 ||
            context.getSystemService(ActivityManager::class.java)?.isBackgroundRestricted != true
    }
    val notificationsOk = remember(resumeKey) {
        context.getSystemService(NotificationManager::class.java)?.areNotificationsEnabled() == true
    }

    RhemaCard(title = stringResource(R.string.card_diag_checks)) {
        CardRow(
            title = stringResource(R.string.diag_check_battery),
            description = stringResource(R.string.diag_check_battery_desc),
            first = true,
        ) {
            if (powerOk) {
                AppBadge(stringResource(R.string.diag_check_ok), tint = JadeiteOk)
            } else {
                OutlinedButton(onClick = {
                    context.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
                }) {
                    Text(stringResource(R.string.diag_check_battery_fix))
                }
            }
        }
        CardRow(
            title = stringResource(R.string.diag_check_background),
            description = stringResource(R.string.diag_check_background_desc),
        ) {
            if (backgroundOk) {
                AppBadge(stringResource(R.string.diag_check_ok), tint = JadeiteOk)
            } else {
                OutlinedButton(onClick = { openAppDetails(context) }) {
                    Text(stringResource(R.string.diag_check_background_fix))
                }
            }
        }
        CardRow(
            title = stringResource(R.string.diag_check_notifications),
            description = stringResource(R.string.diag_check_notifications_desc),
        ) {
            if (notificationsOk) {
                AppBadge(stringResource(R.string.diag_check_ok), tint = JadeiteOk)
            } else {
                OutlinedButton(onClick = {
                    context.startActivity(
                        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                            .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName),
                    )
                }) {
                    Text(stringResource(R.string.diag_check_notifications_fix))
                }
            }
        }
    }
}

// ----------------------------------------------------------------- log

/** The workers' flight recorder, newest first, with a copy-out button. */
@Composable
private fun LogCard(container: AppContainer) {
    var entries by remember { mutableStateOf(container.diag.snapshot()) }
    // The ring only grows while workers run; a light poll keeps the tail
    // live without wiring a flow through the recorder.
    LaunchedEffect(Unit) {
        while (isActive) {
            entries = container.diag.snapshot()
            delay(2_000)
        }
    }
    val clipboard = LocalClipboardManager.current

    RhemaCard(title = stringResource(R.string.card_diag_log), meta = stringResource(R.string.diag_log_hint)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            if (entries.isEmpty()) {
                Text(
                    stringResource(R.string.diag_log_empty),
                    style = MaterialTheme.typography.bodySmall,
                    color = JadeiteText3,
                )
            } else {
                entries.reversed().take(60).forEach { entry ->
                    Text(
                        clock(entry.at) + "  " + entry.line,
                        style = MaterialTheme.typography.bodySmall,
                        fontFamily = JadeiteMono,
                        color = JadeiteText2,
                    )
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (entries.isNotEmpty()) {
                    OutlinedButton(onClick = {
                        clipboard.setText(
                            AnnotatedString(entries.joinToString("\n") { "${clock(it.at)} ${it.line}" }),
                        )
                    }) {
                        Text(stringResource(R.string.diag_log_copy))
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------ helpers

/** One cadence plus margin: more drift than this and the claw is lying. */
private const val NOT_LANDED_MS = 45 * 60_000L

/** The absolute stamp the diagnostics vocabulary speaks: local wall time. */
private fun clock(millis: Long): String =
    SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.getDefault()).format(Date(millis))

/** "in 7m"-style relative label, floor zero. */
private fun inMinutes(thenMillis: Long): String {
    val minutes = ((thenMillis - System.currentTimeMillis()) / 60_000L).coerceAtLeast(0)
    return "${minutes}m"
}

private fun openAppDetails(context: Context) {
    context.startActivity(
        Intent(
            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
            android.net.Uri.fromParts("package", context.packageName, null),
        ),
    )
}
