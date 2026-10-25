package rs.rhema.phone.work

import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import rs.rhema.phone.RhemaApp
import rs.rhema.phone.diagnostics.diagText
import rs.rhema.phone.net.ClawResult
import kotlinx.coroutines.flow.first

// Periodic notification poll. Latency is bounded by the WorkManager interval
// (15 minutes, the OS floor); without push infrastructure that is the deal.
// The chat screen is always live via SSE regardless.
class NotificationsWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {

    override suspend fun doWork(): Result {
        val container = (applicationContext as RhemaApp).container
        val deviceId = container.settings.deviceId()
        val since = container.status.notificationCursor.first()

        val notifications = when (val result = container.api.notifications(deviceId, since)) {
            is ClawResult.Ok -> {
                container.diag.log(
                    if (result.value.isEmpty()) "notify: idle" else "notify: ${result.value.size} delivered",
                )
                result.value
            }
            is ClawResult.HttpError -> {
                container.diag.log("notify: ${result.diagText()}")
                return if (result.code >= 500) Result.retry() else Result.failure()
            }
            is ClawResult.Network -> {
                container.diag.log("notify: ${result.diagText()}")
                return Result.retry()
            }
        }

        for (notification in notifications) {
            container.notifier.post(
                notification.id.hashCode(),
                notification.title ?: container.notifier.fallbackTitle(),
                notification.body.orEmpty(),
            )
            // Acks are best-effort: the cursor means "seen", not "acked", so a
            // failed ack does not re-notify on the next poll.
            container.api.ackNotification(notification.id.toString())
            container.status.setNotificationCursor(notification.id.toString())
        }
        return Result.success()
    }
}
