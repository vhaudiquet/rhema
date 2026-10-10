package dev.rhema.phone.work

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

// WorkManager already re-enqueues its own periodic work after reboot; this
// receiver is the explicit boot kick. It is idempotent (KEEP policy).
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) return
        schedulePeriodicWork(context.applicationContext)
    }
}
