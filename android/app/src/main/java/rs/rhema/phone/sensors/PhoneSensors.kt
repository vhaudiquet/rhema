package rs.rhema.phone.sensors

import android.Manifest
import android.app.AlarmManager
import android.app.AppOpsManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Process
import android.location.Location
import android.location.LocationManager
import androidx.core.content.ContextCompat
import androidx.core.location.LocationManagerCompat
import androidx.core.os.CancellationSignal
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull

/** One location observation: where, how precisely, and how old. */
data class LocationFix(
    val lat: Double,
    val lon: Double,
    val accuracyM: Double?,
    val provider: String?,
    val ageS: Long,
)

/** The app the phone last brought to the foreground, and how long ago. */
data class ForegroundApp(
    val pkg: String,
    val label: String?,
    val ageS: Long,
)

/**
 * The status sensors the phone reports, one function each. Every reader
 * is nullable by contract: a sensor this phone cannot or may not answer
 * (a permission the human has not granted, a provider switched off,
 * nothing to report) produces no value, and no value never produces an
 * ingest post. The status worker calls these on its run; the settings
 * screen calls the two grant checks to explain what is switched off.
 */
object PhoneSensors {

    /** The age beyond which a location fix is too stale to report. */
    private const val MAX_FIX_AGE_MS = 30 * 60 * 1000L

    /** How long a fresh-fix request may take before the fallback wins. */
    private const val FIX_TIMEOUT_MS = 20_000L

    /** The look-back window for "the app currently in the foreground". */
    private const val USAGE_LOOKBACK_MS = 24 * 60 * 60 * 1000L

    // The foreground transition's event type: ACTIVITY_RESUMED renamed
    // MOVE_TO_FOREGROUND at API 29, same value, so the inlined constant
    // reads correctly on every supported level.
    @Suppress("InlinedApi")
    private val FOREGROUND_EVENT = UsageEvents.Event.ACTIVITY_RESUMED

    /** Location (fine or coarse) is a runtime permission: granted here, or no fixes. */
    fun hasLocation(context: Context): Boolean =
        ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_FINE_LOCATION) ==
            PackageManager.PERMISSION_GRANTED ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_COARSE_LOCATION) ==
            PackageManager.PERMISSION_GRANTED

    /** Usage access is a special-app grant (a settings toggle, like DND access). */
    fun hasUsageAccess(context: Context): Boolean {
        val appOps = context.getSystemService(AppOpsManager::class.java) ?: return false
        val mode = @Suppress("DEPRECATION") appOps.checkOpNoThrow(
            AppOpsManager.OPSTR_GET_USAGE_STATS,
            Process.myUid(),
            context.packageName,
        )
        return mode == AppOpsManager.MODE_ALLOWED
    }

    /**
     * A fresh fix from the best enabled provider, falling back to
     * [lastKnownFix] when no provider can answer inside [FIX_TIMEOUT_MS].
     * The snapshot pass prefers this over a last-known read: last-known
     * is whatever another app happened to leave behind, which on a
     * stationary phone goes stale within minutes and reports where the
     * phone was, not where it is.
     */
    suspend fun currentFix(context: Context): LocationFix? {
        if (!hasLocation(context)) return null
        val manager = context.getSystemService(LocationManager::class.java) ?: return null
        // Fused exists from API 31 and is the one provider that answers
        // indoors and out; below that, gps then network.
        val provider = listOf(
            if (Build.VERSION.SDK_INT >= 31) LocationManager.FUSED_PROVIDER else null,
            LocationManager.GPS_PROVIDER,
            LocationManager.NETWORK_PROVIDER,
        ).filterNotNull().firstOrNull { candidate ->
            runCatching { manager.isProviderEnabled(candidate) }.getOrDefault(false)
        } ?: return lastKnownFix(context)

        val signal = CancellationSignal()
        val location = withTimeoutOrNull(FIX_TIMEOUT_MS) {
            suspendCancellableCoroutine<Location?> { continuation ->
                val settled = AtomicBoolean(false)
                continuation.invokeOnCancellation {
                    settled.set(true)
                    signal.cancel()
                }
                val callback = androidx.core.util.Consumer<Location> { location ->
                    // The timeout abandons this coroutine but not the
                    // callback: a late answer must not resume it twice.
                    // A null answer is the provider's "no fix", which
                    // falls through to the last-known fallback.
                    if (settled.compareAndSet(false, true)) {
                        continuation.resume(location)
                    }
                }
                try {
                    LocationManagerCompat.getCurrentLocation(
                        manager,
                        provider,
                        signal,
                        ContextCompat.getMainExecutor(context),
                        callback,
                    )
                } catch (_: SecurityException) {
                    // The grant can be revoked between the check at the
                    // top of currentFix and this request; that read then
                    // belongs to the last-known fallback, not a crash.
                    if (settled.compareAndSet(false, true)) {
                        continuation.resume(null)
                    }
                }
            }
        }
        return location?.let { fix ->
            LocationFix(
                lat = fix.latitude,
                lon = fix.longitude,
                accuracyM = fix.accuracy.takeIf { it > 0f }?.toDouble(),
                provider = fix.provider,
                ageS = (System.currentTimeMillis() - fix.time) / 1000,
            )
        } ?: lastKnownFix(context)
    }

    /**
     * The most recent fix any enabled provider still holds, or null
     * when location is off or every fix is older than [MAX_FIX_AGE_MS].
     * The fallback read behind [currentFix], and the answer when no
     * provider can produce a fresh fix in time.
     */
    fun lastKnownFix(context: Context): LocationFix? {
        if (!hasLocation(context)) return null
        val manager = context.getSystemService(LocationManager::class.java) ?: return null
        val now = System.currentTimeMillis()
        val fix = try {
            manager.getProviders(true)
                .mapNotNull { provider -> manager.getLastKnownLocation(provider) }
                .filter { now - it.time <= MAX_FIX_AGE_MS }
                .maxByOrNull { it.time }
        } catch (_: SecurityException) {
            // The grant can be revoked between the check above and this read.
            null
        } ?: return null
        return LocationFix(
            lat = fix.latitude,
            lon = fix.longitude,
            accuracyM = fix.accuracy.takeIf { it > 0f }?.toDouble(),
            provider = fix.provider,
            ageS = (now - fix.time) / 1000,
        )
    }

    /**
     * The app last brought to the foreground, from the usage stream.
     * Needs the usage-access grant; without it this is null and the
     * app topic stays silent. This app's own activity is skipped: the
     * status run itself must not make Rhema the answer.
     */
    fun foregroundApp(context: Context): ForegroundApp? {
        if (!hasUsageAccess(context)) return null
        val usage = context.getSystemService(UsageStatsManager::class.java) ?: return null
        val now = System.currentTimeMillis()
        val events = usage.queryEvents(now - USAGE_LOOKBACK_MS, now)
        val event = UsageEvents.Event()
        var pkg: String? = null
        var lastResume = 0L
        while (events.hasNextEvent()) {
            events.getNextEvent(event)
            if (event.eventType == FOREGROUND_EVENT && event.packageName != context.packageName) {
                pkg = event.packageName
                lastResume = event.timeStamp
            }
        }
        pkg ?: return null
        val label = runCatching {
            val pm = context.packageManager
            pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
        }.getOrNull()
        return ForegroundApp(pkg, label, (now - lastResume) / 1000)
    }

    /** The connected wifi network's name, when the system will tell. */
    @Suppress("DEPRECATION")
    fun wifiSsid(context: Context): String? {
        // Reading an SSID is treated as location by the OS: without the
        // grant the answer is a placeholder, so do not post it.
        if (!hasLocation(context)) return null
        val wifi = context.getSystemService(WifiManager::class.java) ?: return null
        val info = wifi.connectionInfo ?: return null
        return info.ssid
            ?.removeSurrounding("\"")
            ?.takeIf { ssid ->
                ssid.isNotBlank() && !ssid.equals("<unknown ssid>", ignoreCase = true)
            }
    }

    /** The user's next alarm as set in the clock app, epoch millis. */
    fun nextAlarm(context: Context): Long? =
        context
            .getSystemService(AlarmManager::class.java)
            ?.nextAlarmClock
            ?.triggerTime
            ?.takeIf { it > 0 }

    /** The device's identity fingerprint: changes on upgrade or rename. */
    fun deviceFingerprint(context: Context): String {
        val version = runCatching { appVersion(context) }.getOrNull().orEmpty()
        return listOf(Build.MANUFACTURER, Build.MODEL, Build.VERSION.RELEASE, Build.VERSION.SDK_INT, version)
            .joinToString("|")
    }

    /** `<manufacturer> <model>`, the label the claw's fleet rows lead with. */
    fun deviceName(): String = "${Build.MANUFACTURER} ${Build.MODEL}".trim()

    /** This install's own version name, e.g. "0.1.0". */
    fun appVersion(context: Context): String {
        val pm = context.packageManager
        @Suppress("DEPRECATION")
        val info = pm.getPackageInfo(context.packageName, 0)
        return info.versionName ?: ""
    }

    /** Metres between a fix and a previously posted one, 0 when either is absent. */
    fun metresBetween(fromLat: Double?, fromLon: Double?, to: LocationFix): Float? {
        if (fromLat == null || fromLon == null) return null
        val results = FloatArray(2)
        Location.distanceBetween(fromLat, fromLon, to.lat, to.lon, results)
        return results[0]
    }
}
