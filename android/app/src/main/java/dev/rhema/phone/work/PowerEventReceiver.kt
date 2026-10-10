package dev.rhema.phone.work

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.BatteryManager
import dev.rhema.phone.RhemaApp

// Registered dynamically for the process lifetime (see RhemaApp): the OS
// refuses manifest registration for these implicit broadcasts. Dumb on
// purpose — it only enqueues work; the worker does all reading and posting.
class PowerEventReceiver : BroadcastReceiver() {

    // In-memory only: coalesces ACTION_BATTERY_CHANGED (which fires on every
    // percent point) down to charging flips seen by this process. Process
    // death simply resets the baseline; the periodic worker covers the gap.
    private var lastCharging: Boolean? = null

    override fun onReceive(context: Context, intent: Intent) {
        val app = context.applicationContext as? RhemaApp ?: return
        when (intent.action) {
            Intent.ACTION_POWER_CONNECTED,
            Intent.ACTION_POWER_DISCONNECTED,
            -> enqueuePhoneStatusNow(app)

            Intent.ACTION_BATTERY_CHANGED -> {
                val status = intent.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
                val charging = status == BatteryManager.BATTERY_STATUS_CHARGING ||
                    status == BatteryManager.BATTERY_STATUS_FULL
                if (lastCharging != null && charging != lastCharging) {
                    enqueuePhoneStatusNow(app)
                }
                lastCharging = charging
            }
        }
    }
}
