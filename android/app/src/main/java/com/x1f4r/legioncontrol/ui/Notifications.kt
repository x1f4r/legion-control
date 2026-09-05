package com.x1f4r.legioncontrol.ui

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
import com.x1f4r.legioncontrol.agent.OperationRecord

/**
 * Telling the user how something they asked for ended, when they were not looking.
 *
 * Only for changes a person explicitly asked for, and only for the ones that actually did or failed
 * to do something: a no-op update posts nothing, because "there was nothing to install" is not news
 * worth a buzz. Polling never notifies. This is the one thing in the app that speaks while the
 * screen is away, and it earns that by being rare.
 *
 * Opt in, in both directions. Android will not deliver these without the runtime permission, and the
 * app will not ask for it until the user turns the setting on, because a permission prompt on first
 * launch for a feature nobody has asked for is how people learn to say no to everything.
 */
class AndroidOperationNotifier(
    context: Context,
    private val enabled: () -> Boolean,
) : OperationNotifier {
    private val app = context.applicationContext

    override fun finished(record: OperationRecord) {
        if (!enabled()) return
        if (!record.isNoteworthyEnding) return

        // Checked here rather than through the helper below, because this is the check the tooling
        // can see: a permission test one call away is a permission test the compiler cannot connect
        // to the call it protects, and a wrong answer here is a crash rather than a missing line.
        if (ContextCompat.checkSelfPermission(app, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }

        ensureChannel()
        val title = when (record.succeeded) {
            true -> "${record.kind.label} finished"
            false -> "${record.kind.label} failed"
            null -> "${record.kind.label}: outcome not known"
        }
        val notification = NotificationCompat.Builder(app, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle("$title on ${record.machineName}")
            .setContentText(record.summary)
            .setStyle(NotificationCompat.BigTextStyle().bigText(record.summary))
            .setCategory(NotificationCompat.CATEGORY_STATUS)
            .setOnlyAlertOnce(true)
            .setAutoCancel(true)
            .build()

        // The operation id decides the notification id, so an operation that is reported twice
        // replaces its own line rather than stacking.
        runCatching {
            NotificationManagerCompat.from(app).notify(record.id.hashCode(), notification)
        }
    }

    fun permitted(): Boolean = ContextCompat.checkSelfPermission(
        app,
        Manifest.permission.POST_NOTIFICATIONS,
    ) == PackageManager.PERMISSION_GRANTED

    private fun ensureChannel() {
        val manager = app.getSystemService<NotificationManager>() ?: return
        if (manager.getNotificationChannel(CHANNEL) != null) return
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL,
                "Finished changes",
                // Not high: this is a report about something already asked for, not an alarm.
                NotificationManager.IMPORTANCE_DEFAULT,
            ).apply {
                description = "How an update, restart, boot or action you asked for ended."
                setShowBadge(false)
            },
        )
    }

    private companion object {
        const val CHANNEL = "legion-operations"
    }
}
