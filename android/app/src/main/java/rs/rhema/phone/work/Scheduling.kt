package rs.rhema.phone.work

import android.content.Context
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import java.time.Duration

// The unique names double as the diagnostics screen's work rows: it reads
// the live WorkInfo for each, so the spellings live here and nowhere else.
internal const val PHONE_STATUS = "phone_status"
internal const val NOTIFICATIONS = "notifications"
internal const val PHONE_COMMANDS = "phone_commands"
private const val PHONE_STATUS_KICK = "phone_status_kick"
private const val PHONE_COMMANDS_KICK = "phone_commands_kick"

private fun networked() = Constraints.Builder()
    .setRequiredNetworkType(NetworkType.CONNECTED)
    .build()

// KEEP everywhere: re-running the schedule (app start, boot) must not reset
// the 15-minute cadence. 15 minutes is the WorkManager periodic floor.
fun schedulePeriodicWork(context: Context) {
    val wm = WorkManager.getInstance(context)
    wm.enqueueUniquePeriodicWork(
        PHONE_STATUS,
        ExistingPeriodicWorkPolicy.KEEP,
        PeriodicWorkRequestBuilder<PhoneStatusWorker>(Duration.ofMinutes(15))
            .setConstraints(networked())
            .build(),
    )
    wm.enqueueUniquePeriodicWork(
        NOTIFICATIONS,
        ExistingPeriodicWorkPolicy.KEEP,
        PeriodicWorkRequestBuilder<NotificationsWorker>(Duration.ofMinutes(15))
            .setConstraints(networked())
            .build(),
    )
    wm.enqueueUniquePeriodicWork(
        PHONE_COMMANDS,
        ExistingPeriodicWorkPolicy.KEEP,
        PeriodicWorkRequestBuilder<PhoneCommandWorker>(Duration.ofMinutes(15))
            .setConstraints(networked())
            .build(),
    )
}

// Receiver-side kick for charging flips; KEEP coalesces bursts into one run.
fun enqueuePhoneStatusNow(context: Context) {
    WorkManager.getInstance(context).enqueueUniqueWork(
        PHONE_STATUS_KICK,
        ExistingWorkPolicy.KEEP,
        OneTimeWorkRequestBuilder<PhoneStatusWorker>()
            .setConstraints(networked())
            .build(),
    )
}

// Foreground kick for the control queue: the app opening is when the human
// is around to see an alert or answer a question. KEEP coalesces resume
// bursts into one run.
fun enqueuePhoneCommandsNow(context: Context) {
    WorkManager.getInstance(context).enqueueUniqueWork(
        PHONE_COMMANDS_KICK,
        ExistingWorkPolicy.KEEP,
        OneTimeWorkRequestBuilder<PhoneCommandWorker>()
            .setConstraints(networked())
            .build(),
    )
}
