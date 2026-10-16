package rs.rhema.phone.notif

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import rs.rhema.phone.MainActivity
import rs.rhema.phone.R

class Notifier(private val context: Context) {

    fun fallbackTitle(): String = context.getString(R.string.notif_default_title)

    fun ensureChannel() =
        ensureChannel(CHANNEL_ID, R.string.channel_name, NotificationManager.IMPORTANCE_DEFAULT)

    // The claw's notification payload carries no session key, so the tap opens
    // the app root (session list) rather than a specific chat. id = hash of the
    // claw id: stable per notification, so re-shows collapse instead of stack.
    fun post(notificationId: Int, title: String, body: String) {
        notifyOn(CHANNEL_ID, notificationId, title, body)
    }

    // The alert path (claw phone commands): heads-up and sound on, against
    // the poll channel's quiet delivery. Answers false when the runtime
    // notification grant is missing, so the caller reports that back
    // instead of pretending delivery.
    fun postAlert(notificationId: Int, title: String, body: String): Boolean =
        notifyOn(ALERT_CHANNEL_ID, notificationId, title, body)

    // False when POST_NOTIFICATIONS is ungranted (API 33+): nothing can be
    // posted without it, and callers still advance their own state (see
    // the workers).
    private fun notifyOn(channel: String, notificationId: Int, title: String, body: String): Boolean {
        if (Build.VERSION.SDK_INT >= 33 &&
            context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return false
        }
        if (channel == ALERT_CHANNEL_ID) {
            ensureChannel(
                ALERT_CHANNEL_ID,
                R.string.alert_channel_name,
                NotificationManager.IMPORTANCE_HIGH,
            )
        } else {
            ensureChannel()
        }
        val tap = PendingIntent.getActivity(
            context,
            0,
            Intent(context, MainActivity::class.java)
                .setFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_rhema)
            .setContentTitle(title)
            .setContentText(body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .build()
        context.getSystemService(NotificationManager::class.java)
            ?.notify(notificationId, notification)
        return true
    }

    private fun ensureChannel(id: String, nameRes: Int, importance: Int) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        manager.createNotificationChannel(NotificationChannel(id, context.getString(nameRes), importance))
    }

    companion object {
        const val CHANNEL_ID = "rhema"

        // The alert channel exists for immediacy: high importance posts
        // heads-up with sound, where the default channel stays quiet.
        const val ALERT_CHANNEL_ID = "rhema-alert"
    }
}
