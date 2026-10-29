package com.x1f4r.legioncontrol.ui

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/**
 * Keeps the process running while a change somebody asked for is being sent or followed.
 *
 * It does none of the work. The work is the operation center's, on the application's scope, and it
 * always was; what it lacked was a process that went on running once the phone was locked. A
 * foreground service is what tells Android that this process is doing something the person is
 * waiting for, so it is neither frozen nor cut off from the network halfway through an update.
 *
 * Started by the first hold on [KeepAlive] after none, which is a press and so a moment the app is
 * in front, and stopped by itself once it reads nothing held. Never stopped from outside: Android
 * ends a process whose foreground service is stopped before it has gone into the foreground, and
 * only this service knows when it has.
 *
 * The foreground keeps the process from being frozen, not the phone from sleeping. A locked phone
 * on battery suspends its CPU, and while it is suspended no delay runs out: the readiness polls after
 * a wake packet, the pause before reading an operation again and the looks at a machine whose agent
 * keeps no records all wait for the screen, and only a reply already on its way moves anything. So
 * the service also keeps the CPU running, screen off, with a partial wake lock that ends with it.
 * The lock is timed besides, a little past one watch and armed again whenever what is held moves,
 * so a hold that is never given back cannot keep the phone awake for good.
 *
 * `dataSync`, because that is what this is: commands sent over the network and replies waited for.
 * It needs nothing the user is asked for. Android 15 caps it at six hours a day, and a change is
 * held for little more than one half-hour watch, so the cap is a backstop rather than a limit
 * anybody meets; when it is met, the work goes on unheld, as it did before there was a service.
 */
class ChangeService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate + uncaughtFailureHandler("ChangeService"))
    private var watching: Job? = null

    /** The newest start. A stop for an older one is ignored by Android, which is what makes it safe. */
    private var latestStart = 0

    /**
     * Keeps the CPU running while this service does. Not reference counted: it is armed again with
     * every start and every move of the count, and released once, when the service ends.
     */
    private val awake: PowerManager.WakeLock? by lazy {
        getSystemService<PowerManager>()
            ?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG)
            ?.apply { setReferenceCounted(false) }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        latestStart = startId
        val holds = (application as LegionApplication).keepAlive.holds
        // Into the foreground first, every time: Android requires it of every start, and stopping
        // before it is the one way to stop this service that ends the process.
        if (!enterForeground() || holds.value == 0) {
            stopSelf(startId)
            return START_NOT_STICKY
        }
        stayAwake()
        if (watching == null) {
            watching = scope.launch {
                holds.collect { held -> if (held == 0) stopSelf(latestStart) else stayAwake() }
            }
        }
        // Not restarted after the process dies. A new process starts with no change in flight, and
        // finds the ones that were by their records when the app is next opened.
        return START_NOT_STICKY
    }

    /** Android 15's daily allowance for this kind of service ran out. The work goes on unheld. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        stopSelf()
    }

    /** Every way this service ends comes through here, a stop for the daily allowance included. */
    override fun onDestroy() {
        scope.cancel()
        awake?.takeIf { it.isHeld }?.release()
        super.onDestroy()
    }

    /** Keeps the CPU running for [WAKE_LOCK_MS] from now, however long it already had. */
    private fun stayAwake() {
        awake?.acquire(WAKE_LOCK_MS)
    }

    private fun enterForeground(): Boolean = try {
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        true
    } catch (refused: RuntimeException) {
        // Refused by Android rather than failed here: started from the background after all, or the
        // daily allowance is gone. The change itself is not affected.
        Log.w(TAG, "Could not keep the app running for a change in progress.", refused)
        false
    }

    /**
     * What Android shows while this runs. It has to show something; this says what, and a tap opens
     * the app, where the change itself says how far it has got.
     */
    private fun notification() = NotificationCompat.Builder(this, ensureChannel())
        .setSmallIcon(android.R.drawable.stat_notify_sync)
        .setContentTitle("Seeing a change through")
        .setContentText("Until the machine says how it ended.")
        .setCategory(NotificationCompat.CATEGORY_PROGRESS)
        .setPriority(NotificationCompat.PRIORITY_LOW)
        .setOngoing(true)
        .setContentIntent(
            PendingIntent.getActivity(
                this,
                0,
                Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            ),
        )
        .build()

    private fun ensureChannel(): String {
        val manager = getSystemService<NotificationManager>() ?: return CHANNEL
        if (manager.getNotificationChannel(CHANNEL) == null) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL,
                    "Changes in progress",
                    // Low: no sound, no peek. It is there because Android requires it, and it should
                    // be no louder than that.
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = "Shown while an update, restart, boot or action you asked for is still going."
                    setShowBadge(false)
                },
            )
        }
        return CHANNEL
    }

    companion object {
        private const val TAG = "ChangeService"
        private const val CHANNEL = "legion-changes-in-progress"
        private const val NOTIFICATION_ID = 1

        /** In the `app:component` form Android's own tools read a wake lock's owner from. */
        private const val WAKE_LOCK_TAG = "LegionControl:ChangeService"

        /**
         * How long the CPU is kept running after the newest start or move of the count: one watch
         * and a few minutes, for the send or the read that comes before it.
         */
        private const val WAKE_LOCK_MS = OperationCenter.WATCH_BUDGET_MS + 5 * 60_000L

        /**
         * Asks for the service. A refusal is logged and nothing more: Android 12 and later refuse a
         * foreground service started from the background, which is when a change is first followed
         * from a poll rather than a press, and the change runs all the same.
         */
        fun start(context: Context) {
            try {
                ContextCompat.startForegroundService(context, Intent(context, ChangeService::class.java))
            } catch (refused: IllegalStateException) {
                Log.w(TAG, "Android would not start the service for a change in progress.", refused)
            } catch (refused: SecurityException) {
                Log.w(TAG, "Android would not start the service for a change in progress.", refused)
            }
        }
    }
}
