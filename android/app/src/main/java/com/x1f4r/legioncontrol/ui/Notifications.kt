package com.x1f4r.legioncontrol.ui

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
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
 * screen is away, and it earns that by being rare. Nor does it speak while the screen is not away:
 * the page already says how the change ended, in front of the person reading it.
 *
 * Opt in, in both directions. Android will not deliver these without the runtime permission, and the
 * app will not ask for it until the user turns the setting on, because a permission prompt on first
 * launch for a feature nobody has asked for is how people learn to say no to everything.
 *
 * A tap opens the machine the change was on, which is where its outcome and its details are.
 */
class AndroidOperationNotifier(
    context: Context,
    private val enabled: () -> Boolean,
    /** Whether the app is in front of the person right now, in which case the page says it. */
    private val inFront: () -> Boolean,
) : OperationNotifier {
    private val app = context.applicationContext

    override fun finished(record: OperationRecord) {
        if (!announces(record, enabled = enabled(), inFront = inFront())) return

        // Checked here rather than through the helper below, because this is the check the tooling
        // can see: a permission test one call away is a permission test the compiler cannot connect
        // to the call it protects, and a wrong answer here is a crash rather than a missing line.
        if (ContextCompat.checkSelfPermission(app, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }

        ensureChannel()
        val title = when {
            record.succeeded == true -> "${record.kind.label} finished"
            record.succeeded == false -> "${record.kind.label} failed"
            record.outcome == com.x1f4r.legioncontrol.agent.Outcome.DEFERRED -> "${record.kind.label} did not run"
            else -> "${record.kind.label}: outcome not known"
        }
        val notification = NotificationCompat.Builder(app, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle("$title on ${record.machineName}")
            .setContentText(record.summary)
            .setStyle(NotificationCompat.BigTextStyle().bigText(record.summary))
            .setCategory(NotificationCompat.CATEGORY_STATUS)
            .setContentIntent(opening(record))
            .setOnlyAlertOnce(true)
            .setAutoCancel(true)
            .build()

        // The operation id decides the notification id, so an operation that is reported twice
        // replaces its own line rather than stacking.
        runCatching {
            NotificationManagerCompat.from(app).notify(record.id.hashCode(), notification)
        }
    }

    /**
     * Whether Android lets this app post at all. Below Android 13 there is no runtime permission and
     * androidx answers from whether the user left this app's notifications on, which is the same
     * question asked the only way those versions can be asked it.
     */
    fun permitted(): Boolean = ContextCompat.checkSelfPermission(
        app,
        Manifest.permission.POST_NOTIFICATIONS,
    ) == PackageManager.PERMISSION_GRANTED

    /**
     * The tap: this app, on the page of the machine the change was on.
     *
     * One pending intent per change, keyed like the notification itself, because Android tells
     * pending intents apart by request code and not by their extras: with one code for all of them,
     * every notification would open whichever machine was announced last.
     */
    private fun opening(record: OperationRecord): PendingIntent = PendingIntent.getActivity(
        app,
        record.id.hashCode(),
        Intent(app, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
            .putExtra(EXTRA_MACHINE, record.machineId),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

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

    companion object {
        private const val CHANNEL = "legion-operations"

        /** The machine id a tapped notification asks the app to show. */
        const val EXTRA_MACHINE = "com.x1f4r.legioncontrol.MACHINE"
    }
}

/**
 * Whether [record] ending is announced: a change that did or failed to do something, with the
 * setting on, while the app is not in front of the person who asked for it.
 */
internal fun announces(record: OperationRecord, enabled: Boolean, inFront: Boolean): Boolean =
    enabled && !inFront && record.isNoteworthyEnding
