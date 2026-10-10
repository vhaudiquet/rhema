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

    fun ensureChannel() {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                context.getString(R.string.channel_name),
                NotificationManager.IMPORTANCE_DEFAULT,
            ),
        )
    }

    // The claw's notification payload carries no session key, so the tap opens
    // the app root (session list) rather than a specific chat. id = hash of the
    // claw id: stable per notification, so re-shows collapse instead of stack.
    fun post(notificationId: Int, title: String, body: String) {
        if (Build.VERSION.SDK_INT >= 33 &&
            context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            // Nothing can be posted without the runtime grant; the poll still
            // advances its cursor (see NotificationsWorker).
            return
        }
        ensureChannel()
        val tap = PendingIntent.getActivity(
            context,
            0,
            Intent(context, MainActivity::class.java)
                .setFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_rhema)
            .setContentTitle(title)
            .setContentText(body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .build()
        context.getSystemService(NotificationManager::class.java)
            ?.notify(notificationId, notification)
    }

    companion object {
        const val CHANNEL_ID = "rhema"
    }
}
