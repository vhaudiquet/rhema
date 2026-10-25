package rs.rhema.phone.work

import android.app.NotificationManager
import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import rs.rhema.phone.RhemaApp
import rs.rhema.phone.di.AppContainer
import rs.rhema.phone.diagnostics.diagText
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.net.PhoneCommand
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.coroutines.CancellationException

// Periodic control-plane poll: the claw queues commands for this device
// once their phone consent is granted; each is executed in order, then
// answered. The result POST is the ack, so a failed post leaves the
// command queued and the next run repeats it — at-least-once delivery.
class PhoneCommandWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {

    override suspend fun doWork(): Result = try {
        drainOnce()
    } catch (cancelled: CancellationException) {
        (applicationContext as RhemaApp).container.diag.log("commands: cancelled")
        throw cancelled
    } catch (t: Throwable) {
        (applicationContext as RhemaApp).container.diag.log(
            "commands: crashed: ${t.javaClass.simpleName}: ${t.message ?: "no message"}" +
                t.stackTrace.firstOrNull()?.let { " at ${it.className}.${it.methodName}" }.orEmpty(),
        )
        Result.failure()
    }

    private suspend fun drainOnce(): Result {
        val container = (applicationContext as RhemaApp).container
        val deviceId = container.settings.deviceId()

        val commands = when (val result = container.api.phoneCommands(deviceId)) {
            is ClawResult.Ok -> {
                container.diag.log(
                    if (result.value.isEmpty()) "commands: idle" else "commands: ${result.value.size} queued",
                )
                result.value
            }
            is ClawResult.HttpError -> {
                container.diag.log("commands: ${result.diagText()}")
                return if (result.code >= 500) Result.retry() else Result.failure()
            }
            is ClawResult.Network -> {
                container.diag.log("commands: ${result.diagText()}")
                return Result.retry()
            }
        }

        for (command in commands) {
            val (ok, detail) = execute(container, command)
            container.diag.log(
                "commands: ${command.action} -> " +
                    (if (ok) "ok" else "failed: ${detail ?: "no detail"}"),
            )
            // Stop at the first failed ack: the queue is ordered, later
            // commands must not jump ahead of an un-acked one.
            if (container.api.postPhoneCommandResult(command.id, ok, detail) !is ClawResult.Ok) {
                container.diag.log("commands: ack failed: ${command.id}, retry next run")
                return Result.retry()
            }
        }
        return Result.success()
    }

    // One command, locally. The answer pair rides to the claw verbatim;
    // the detail strings are wire content, not UI resources.
    private fun execute(container: AppContainer, command: PhoneCommand): Pair<Boolean, String?> {
        val notifications = applicationContext.getSystemService(NotificationManager::class.java)
        return when (command.action) {
            "dnd_on", "dnd_off" -> {
                // Policy access is a settings grant, not a permission: it
                // can only be given by the user in system settings, so a
                // missing grant is reported back, never prompted here.
                if (notifications?.isNotificationPolicyAccessGranted != true) {
                    false to DND_ACCESS_DETAIL
                } else {
                    notifications.setInterruptionFilter(
                        if (command.action == "dnd_on") {
                            NotificationManager.INTERRUPTION_FILTER_PRIORITY
                        } else {
                            NotificationManager.INTERRUPTION_FILTER_ALL
                        },
                    )
                    true to null
                }
            }
            "alert" -> {
                val posted = container.notifier.postAlert(
                    command.id.hashCode(),
                    command.args.text("title") ?: "claw",
                    command.args.text("body") ?: command.action,
                )
                if (posted) {
                    true to null
                } else {
                    true to "notification permission not granted"
                }
            }
            else -> false to "unknown action"
        }
    }

    private companion object {
        const val DND_ACCESS_DETAIL =
            "grant Do Not Disturb access to Rhema (Settings → Special app access)"
    }
}

// Only primitive strings count; anything else falls back to the
// caller's default.
private fun JsonObject.text(name: String): String? =
    (this[name] as? JsonPrimitive)?.contentOrNull
