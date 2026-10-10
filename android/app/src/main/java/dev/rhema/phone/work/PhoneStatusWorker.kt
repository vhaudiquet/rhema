package dev.rhema.phone.work

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.ConnectivityManager
import android.os.BatteryManager
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import dev.rhema.phone.RhemaApp
import dev.rhema.phone.data.PhoneSnapshot
import dev.rhema.phone.net.ClawResult
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

// Periodic phone-status source. Reads one snapshot, diffs it against the last
// one that was fully POSTed, and ingests only the transitions. If any post
// fails the snapshot is not saved, so the next run retries the transitions.
class PhoneStatusWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {

    override suspend fun doWork(): Result {
        val container = (applicationContext as RhemaApp).container
        val deviceId = container.settings.deviceId()
        val snapshot = readSnapshot(applicationContext)
        val last = container.status.lastSnapshot()

        val transitions = mutableListOf<Pair<String, JsonObject>>()

        // Battery posts on charging flips and decile bucket changes only, so
        // slow drain does not ping on every run.
        val charging = snapshot.charging
        val level = snapshot.batteryLevel
        val batteryChanged = (charging != null && charging != last?.charging) ||
            (level != null && bucket(level) != last?.batteryLevel?.let { bucket(it) })
        if (batteryChanged) {
            transitions += "battery" to buildJsonObject {
                put("topic", "battery")
                put("value", level?.toString() ?: "unknown")
                put("charging", charging)
            }
        }

        val network = snapshot.network
        if (network != null && network != last?.network) {
            transitions += "network" to buildJsonObject {
                put("topic", "network")
                put("value", network)
            }
        }

        val dnd = snapshot.dnd
        if (dnd != null && dnd != last?.dnd) {
            transitions += "dnd" to buildJsonObject {
                put("topic", "dnd")
                put("value", dnd)
            }
        }

        if (transitions.isEmpty()) return Result.success()

        for ((topic, payload) in transitions) {
            val result = container.api.ingest(SOURCE, "phone:$deviceId:$topic", payload)
            if (result !is ClawResult.Ok) return Result.retry()
        }
        container.status.setSnapshot(snapshot)
        return Result.success()
    }

    companion object {
        const val SOURCE = "phone"

        fun bucket(level: Int): Int = level / 10

        private fun readSnapshot(context: Context): PhoneSnapshot {
            val battery = context.getSystemService(BatteryManager::class.java)
            val level = battery
                ?.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY)
                ?.takeIf { it in 1..100 }

            // Peek the sticky battery intent (null receiver, so no export flag
            // applies). This is the only reliable charging signal without
            // listening to the chatty ACTION_BATTERY_CHANGED stream.
            val sticky = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
            val charging = sticky?.let { intent ->
                val status = intent.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
                status == BatteryManager.BATTERY_STATUS_CHARGING ||
                    status == BatteryManager.BATTERY_STATUS_FULL ||
                    intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0
            }

            val connectivity = context.getSystemService(ConnectivityManager::class.java)
            val network = when {
                connectivity == null -> null
                connectivity.activeNetwork == null -> "offline"
                connectivity.isActiveNetworkMetered -> "metered"
                else -> "wifi"
            }

            val notifications = context.getSystemService(NotificationManager::class.java)
            val dnd = notifications?.currentInterruptionFilter
                ?.takeIf { it != NotificationManager.INTERRUPTION_FILTER_UNKNOWN }
                ?.let { if (it == NotificationManager.INTERRUPTION_FILTER_ALL) "off" else "on" }

            return PhoneSnapshot(charging, level, network, dnd)
        }
    }
}
