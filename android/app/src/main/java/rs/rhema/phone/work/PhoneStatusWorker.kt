package rs.rhema.phone.work

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.ConnectivityManager
import android.os.BatteryManager
import android.os.Build
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import kotlinx.coroutines.CancellationException
import rs.rhema.phone.RhemaApp
import rs.rhema.phone.data.PhoneSnapshot
import rs.rhema.phone.diagnostics.diagText
import rs.rhema.phone.net.ClawResult
import rs.rhema.phone.sensors.LocationFix
import rs.rhema.phone.sensors.ForegroundApp
import rs.rhema.phone.sensors.PhoneSensors
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

// Periodic phone-status source. Reads one snapshot, diffs it against the last
// one that was fully POSTed, and ingests only the transitions. If any post
// fails the snapshot is not saved, so the next run retries the transitions
// from the same reading — which is why a failed run still succeeds: the
// schedule is the retry, and `Result.retry()` would move the next run onto
// WorkManager's exponential backoff (up to hours), the exact silence the
// fleet reads as "last seen hours ago".
//
// Two kinds of topic leave this worker. Battery, network, and DND posts are
// conversation: the claw dispatches them and the agent hears the flip. The
// rest — location, app, device, wifi, alarm, heartbeat — are snapshot-only on
// the claw
// (its ingest door answers "observed" and never runs a turn), so their rules
// only bound write traffic, not model traffic.
class PhoneStatusWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {

    override suspend fun doWork(): Result = try {
        reportOnce()
    } catch (cancelled: CancellationException) {
        (applicationContext as RhemaApp).container.diag.log("status: cancelled")
        throw cancelled
    } catch (t: Throwable) {
        // A crash before the first log line used to be invisible: the row
        // just sat "waiting" while every attempt died silently.
        (applicationContext as RhemaApp).container.diag.log(
            "status: crashed: ${t.javaClass.simpleName}: ${t.message ?: "no message"}" +
                t.stackTrace.firstOrNull()?.let { " at ${it.className}.${it.methodName}" }.orEmpty(),
        )
        Result.failure()
    }

    private suspend fun reportOnce(): Result {
        val container = (applicationContext as RhemaApp).container
        val deviceId = container.settings.deviceId()
        // Suspend: the location read may wait out a fresh fix.
        val reading = readSnapshot(applicationContext)
        val snapshot = reading.sensors
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

        // Location posts when the phone moved a meaningful distance since the
        // last posted fix (or never posted one). Stationary drift stays under
        // the threshold; a commute clears it per stop. Snapshot-only: the fix
        // lands in the claw's tables, no turn is run on it.
        val fix = reading.fix
        val metresMoved = fix?.let { PhoneSensors.metresBetween(last?.locationLat, last?.locationLon, it) }
        if (fix != null && (metresMoved == null || metresMoved >= LOCATION_MOVE_M)) {
            transitions += "location" to buildJsonObject {
                put("topic", "location")
                put("lat", fix.lat)
                put("lon", fix.lon)
                put("accuracy_m", fix.accuracyM)
                put("provider", fix.provider)
                put("age_s", fix.ageS)
            }
        }

        // The foreground app posts on change; the age says how long it has
        // been current, which is what keeps "currently open" honest at a
        // 15-minute cadence. Snapshot-only.
        val app = snapshot.app
        if (app != null && app != last?.app) {
            transitions += "app" to buildJsonObject {
                put("topic", "app")
                put("value", app)
                put("label", reading.foreground?.label)
                put("age_s", reading.foreground?.ageS ?: 0)
            }
        }

        // The device identity posts once, and again whenever it changes (an
        // OS upgrade, an app update): the claw keeps it as the fleet label,
        // so this topic is what makes the settings screens human-readable.
        // Snapshot-only.
        if (snapshot.device != null && snapshot.device != last?.device) {
            transitions += "device" to buildJsonObject {
                put("topic", "device")
                put("value", PhoneSensors.deviceName())
                put("manufacturer", Build.MANUFACTURER)
                put("model", Build.MODEL)
                put("android", Build.VERSION.RELEASE)
                put("sdk", Build.VERSION.SDK_INT)
                put("app_version", PhoneSensors.appVersion(applicationContext))
            }
        }

        val wifi = snapshot.wifi
        if (wifi != null && wifi != last?.wifi) {
            transitions += "wifi" to buildJsonObject {
                put("topic", "wifi")
                put("value", wifi)
            }
        }

        val alarm = snapshot.alarm
        if (alarm != null && alarm != last?.alarm) {
            transitions += "alarm" to buildJsonObject {
                put("topic", "alarm")
                put("value", alarm)
                put("in_min", (alarm - System.currentTimeMillis()) / 60000)
            }
        }

        // Nothing changed is not nothing to say: the claw's fleet
        // registry stamps "last seen" on whatever post lands, so a run
        // with no transitions posts a bare heartbeat to keep its row
        // fresh at the worker's cadence instead of the last flip. The
        // topic is snapshot-only claw-side; no turn runs on it.
        if (transitions.isEmpty()) {
            transitions += "heartbeat" to buildJsonObject {
                put("topic", "heartbeat")
                put("ts", System.currentTimeMillis())
            }
        }

        // The start line is the liveness beacon: the diagnostics screen
        // distinguishes "worker ran and failed" (this line, then a failure
        // line) from "worker never ran" (neither).
        container.diag.log(
            "status: run battery=${snapshot.batteryLevel ?: "?"} " +
                "charging=${snapshot.charging ?: "?"} network=${snapshot.network ?: "?"} " +
                "transitions=${transitions.size}",
        )

        for ((topic, payload) in transitions) {
            val result = container.api.ingest(SOURCE, "phone:$deviceId:$topic", payload)
            // The snapshot stays unsaved, so the next scheduled run re-derives
            // this whole transition set and posts it again. Retrying here
            // instead would push that run out on WorkManager's backoff, and a
            // phone in a pocket would go quiet for hours.
            if (result !is ClawResult.Ok) {
                container.diag.log("status: $topic failed: ${result.diagText()}")
                return Result.success()
            }
        }
        container.status.setSnapshot(snapshot)
        container.diag.log("status: posted ${transitions.joinToString { it.first }}")
        return Result.success()
    }

    companion object {
        const val SOURCE = "phone"

        // A fix counts as a move at this distance; below it, the phone has
        // not gone anywhere worth a post.
        const val LOCATION_MOVE_M = 300f

        fun bucket(level: Int): Int = level / 10

        private suspend fun readSnapshot(context: Context): Reading {
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

            // A fresh fix beats the last-known cache: on a stationary
            // phone the cache is minutes-to-hours old and reports where
            // the phone was, not where it is.
            val fix = PhoneSensors.currentFix(context)
            val foreground = PhoneSensors.foregroundApp(context)
            val sensors = PhoneSnapshot(
                charging = charging,
                batteryLevel = level,
                network = network,
                dnd = dnd,
                locationLat = fix?.lat,
                locationLon = fix?.lon,
                app = foreground?.pkg,
                device = PhoneSensors.deviceFingerprint(context),
                wifi = PhoneSensors.wifiSsid(context),
                alarm = PhoneSensors.nextAlarm(context),
            )
            return Reading(sensors, fix, foreground)
        }
    }
}

/**
 * One read of every sensor: the diffable [PhoneSnapshot] the run
 * persists on success, plus the rich readings whose payload fields (a
 * fix's accuracy, an app's label) never need persisting.
 */
private data class Reading(
    val sensors: PhoneSnapshot,
    val fix: LocationFix?,
    val foreground: ForegroundApp?,
)
