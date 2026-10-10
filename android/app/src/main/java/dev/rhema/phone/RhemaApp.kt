package dev.rhema.phone

import android.app.Application
import android.content.Intent
import android.content.IntentFilter
import androidx.core.content.ContextCompat
import dev.rhema.phone.di.AppContainer
import dev.rhema.phone.work.PowerEventReceiver

class RhemaApp : Application() {

    lateinit var container: AppContainer
        private set

    // Held in a field so the registered receiver cannot be collected while
    // the process lives.
    private var powerReceiver: PowerEventReceiver? = null

    override fun onCreate() {
        super.onCreate()
        container = AppContainer(this)
        container.notifier.ensureChannel()
        container.schedulePeriodicWork()

        // ACTION_BATTERY_CHANGED and the power actions are implicit broadcasts:
        // manifest registration for them is refused, so this receiver lives for
        // the process lifetime instead. It stays dumb: it only enqueues work.
        val receiver = PowerEventReceiver()
        powerReceiver = receiver
        ContextCompat.registerReceiver(
            this,
            receiver,
            IntentFilter().apply {
                addAction(Intent.ACTION_POWER_CONNECTED)
                addAction(Intent.ACTION_POWER_DISCONNECTED)
                addAction(Intent.ACTION_BATTERY_CHANGED)
            },
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
    }
}
