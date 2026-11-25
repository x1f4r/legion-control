package com.x1f4r.legion

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.Uri
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Data-sync foreground service that keeps background downloads and uploads running while Legion
 * is in the background, the screen is locked, or the activity is swiped away from Recents.
 *
 * Respects Android 14+ / 15+ `dataSync` foreground service rules:
 * - Declares `foregroundServiceType="dataSync"` and starts foreground with
 *   `FOREGROUND_SERVICE_TYPE_DATA_SYNC`.
 * - Detaches from foreground state when all remaining transfers are paused so Android's 6-hour
 *   rolling `dataSync` quota stops ticking while the notification keeps Resume and Cancel buttons.
 * - Handles `onTimeout` cleanly by pausing active transfers in Rust, notifying the person, and
 *   stopping within Android's grace window.
 */
class TransferForegroundService : Service() {
  private val inForeground = AtomicBoolean(false)

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    ensureChannels(this)
    runningInstance = this
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    runningInstance = this
    ensureChannels(this)
    val summary = latestSummary
    if (summary == null) {
      promoteMinimalAndStop(startId)
      return START_NOT_STICKY
    }
    applySummary(summary, ensureForegroundFirst = true)
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    if (runningInstance === this) {
      runningInstance = null
    }
    inForeground.set(false)
    super.onDestroy()
  }

  override fun onTimeout(startId: Int) {
    handleSystemTimeout(startId)
  }

  override fun onTimeout(startId: Int, fgsType: Int) {
    handleSystemTimeout(startId)
  }

  private fun handleSystemTimeout(startId: Int) {
    Log.w(TAG, "Android dataSync foreground service timed out; pausing active transfers")
    TransferBridge.sendEvent(JSONObject().put("event", "fgs_timeout"))
    postTimeoutNotice(this)
    inForeground.set(false)
    runCatching {
      ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
    }
    stopSelf(startId)
  }

  private fun promoteMinimalAndStop(startId: Int) {
    val placeholder = NotificationCompat.Builder(this, CHANNEL_ONGOING)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("Legion transfers")
      .setContentText("Finishing up")
      .setSilent(true)
      .build()
    runCatching {
      ServiceCompat.startForeground(
        this,
        ONGOING_NOTIFICATION_ID,
        placeholder,
        ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
      )
      ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
    }
    stopSelf(startId)
  }

  internal fun applySummary(summary: JSONObject, ensureForegroundFirst: Boolean = false) {
    val activeCount = summary.optInt("activeCount", 0)
    val pausedCount = summary.optInt("pausedCount", 0)
    val nm = getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager

    if (activeCount > 0) {
      val notification = buildOngoingNotification(this, summary, isActive = true)
      if (!inForeground.get() || ensureForegroundFirst) {
        try {
          ServiceCompat.startForeground(
            this,
            ONGOING_NOTIFICATION_ID,
            notification,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
          )
          inForeground.set(true)
        } catch (e: Exception) {
          Log.w(TAG, "startForeground(dataSync) failed: ${e.message}")
          notifySafely(this, nm, ONGOING_NOTIFICATION_ID, notification)
        }
      } else {
        notifySafely(this, nm, ONGOING_NOTIFICATION_ID, notification)
      }
      return
    }

    if (pausedCount > 0) {
      val notification = buildOngoingNotification(this, summary, isActive = false)
      if (ensureForegroundFirst && !inForeground.get()) {
        runCatching {
          ServiceCompat.startForeground(
            this,
            ONGOING_NOTIFICATION_ID,
            notification,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
          )
        }
      }
      if (inForeground.getAndSet(false) || ensureForegroundFirst) {
        runCatching {
          ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_DETACH)
        }
      }
      notifySafely(this, nm, ONGOING_NOTIFICATION_ID, notification)
      stopSelf()
      return
    }

    // Queue empty.
    if (ensureForegroundFirst && !inForeground.get()) {
      promoteMinimalAndStop(0)
      return
    }
    if (inForeground.getAndSet(false)) {
      runCatching {
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
      }
    }
    nm?.cancel(ONGOING_NOTIFICATION_ID)
    stopSelf()
  }

  internal fun stopServiceCleanly() {
    val nm = getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
    if (inForeground.getAndSet(false)) {
      runCatching {
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
      }
    }
    nm?.cancel(ONGOING_NOTIFICATION_ID)
    stopSelf()
  }

  companion object {
    private const val TAG = "legion-transfers"
    const val CHANNEL_ONGOING = "legion_transfers_ongoing"
    const val CHANNEL_COMPLETE = "legion_transfers_complete"
    const val ONGOING_NOTIFICATION_ID = 4101
    private const val TIMEOUT_NOTIFICATION_ID = 4102
    private const val COMPLETE_NOTIFICATION_BASE = 4200

    @Volatile
    private var runningInstance: TransferForegroundService? = null

    @Volatile
    private var latestSummary: JSONObject? = null

    fun ensureChannels(context: Context) {
      val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        ?: return
      if (nm.getNotificationChannel(CHANNEL_ONGOING) == null) {
        val ongoing = NotificationChannel(
          CHANNEL_ONGOING,
          "Active transfers",
          NotificationManager.IMPORTANCE_LOW,
        ).apply {
          description = "Progress and controls for ongoing Legion downloads and uploads."
          setShowBadge(false)
        }
        nm.createNotificationChannel(ongoing)
      }
      if (nm.getNotificationChannel(CHANNEL_COMPLETE) == null) {
        val complete = NotificationChannel(
          CHANNEL_COMPLETE,
          "Finished transfers",
          NotificationManager.IMPORTANCE_DEFAULT,
        ).apply {
          description = "Notifications when Legion downloads or uploads finish."
        }
        nm.createNotificationChannel(complete)
      }
    }

    fun sync(context: Context, summary: JSONObject) {
      ensureChannels(context)
      latestSummary = summary
      val activeCount = summary.optInt("activeCount", 0)
      val pausedCount = summary.optInt("pausedCount", 0)
      val instance = runningInstance

      if (activeCount > 0) {
        if (instance != null) {
          instance.applySummary(summary, ensureForegroundFirst = false)
        } else {
          val intent = Intent(context, TransferForegroundService::class.java)
          try {
            ContextCompat.startForegroundService(context, intent)
          } catch (e: Exception) {
            Log.w(TAG, "Background foreground-service start disallowed: ${e.message}")
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
            val fallback = buildOngoingNotification(context, summary, isActive = false)
            notifySafely(context, nm, ONGOING_NOTIFICATION_ID, fallback)
          }
        }
        return
      }

      if (pausedCount > 0) {
        if (instance != null) {
          instance.applySummary(summary, ensureForegroundFirst = false)
        } else {
          val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
          val pausedNotification = buildOngoingNotification(context, summary, isActive = false)
          notifySafely(context, nm, ONGOING_NOTIFICATION_ID, pausedNotification)
        }
        return
      }

      if (instance != null) {
        instance.stopServiceCleanly()
      } else {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        nm?.cancel(ONGOING_NOTIFICATION_ID)
      }
    }

    fun notifyComplete(context: Context, params: JSONObject) {
      ensureChannels(context)
      val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        ?: return
      val transferId = params.optString("transferId", "done")
      val direction = params.optString("direction", "download")
      val name = params.optString("name", "File")
      val localUri = params.optString("localUri", "").trim()
      val mime = params.optString("mime", "application/octet-stream")
        .ifBlank { "application/octet-stream" }

      val title = if (direction == "upload") "Upload finished" else "Download finished"
      val tapIntent: Intent = if (direction == "download" && localUri.startsWith("content://")) {
        Intent(Intent.ACTION_VIEW).apply {
          setDataAndType(Uri.parse(localUri), mime)
          addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }
      } else {
        Intent(context, MainActivity::class.java).apply {
          addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        }
      }

      val requestCode = COMPLETE_NOTIFICATION_BASE + (transferId.hashCode() and 0x7FFF)
      val pending = PendingIntent.getActivity(
        context,
        requestCode,
        tapIntent,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
      )

      val notification = NotificationCompat.Builder(context, CHANNEL_COMPLETE)
        .setSmallIcon(R.mipmap.ic_launcher)
        .setContentTitle(title)
        .setContentText(name)
        .setGroup("legion_complete_$requestCode")
        .setAutoCancel(true)
        .setContentIntent(pending)
        .build()
      notifySafely(context, nm, requestCode, notification)
    }

    private fun postTimeoutNotice(context: Context) {
      ensureChannels(context)
      val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        ?: return
      val openApp = PendingIntent.getActivity(
        context,
        TIMEOUT_NOTIFICATION_ID,
        Intent(context, MainActivity::class.java).apply {
          addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        },
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
      )
      val notification = NotificationCompat.Builder(context, CHANNEL_COMPLETE)
        .setSmallIcon(R.mipmap.ic_launcher)
        .setContentTitle("Transfers paused by Android")
        .setContentText("Background transfer time limit reached. Open Legion to resume.")
        .setAutoCancel(true)
        .setContentIntent(openApp)
        .build()
      notifySafely(context, nm, TIMEOUT_NOTIFICATION_ID, notification)
    }

    private fun buildOngoingNotification(
      context: Context,
      summary: JSONObject,
      isActive: Boolean,
    ): Notification {
      val title = summary.optString("title", "Legion transfers")
      val detail = summary.optString("detail", "")
      val bytesDone = summary.optLong("bytesDone", 0L).coerceAtLeast(0L)
      val bytesTotal = summary.optLong("bytesTotal", 0L).coerceAtLeast(0L)
      val indeterminate = summary.optBoolean("indeterminate", true)
      val canPause = summary.optBoolean("canPause", false)
      val canResume = summary.optBoolean("canResume", false)
      val primaryId = summary.optString("primaryTransferId", "")

      val openApp = PendingIntent.getActivity(
        context,
        ONGOING_NOTIFICATION_ID,
        Intent(context, MainActivity::class.java).apply {
          addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        },
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
      )

      val builder = NotificationCompat.Builder(context, CHANNEL_ONGOING)
        .setSmallIcon(R.mipmap.ic_launcher)
        .setContentTitle(title)
        .setContentText(detail)
        .setOnlyAlertOnce(true)
        .setSilent(true)
        .setOngoing(isActive)
        .setContentIntent(openApp)
        .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)

      if (indeterminate || bytesTotal <= 0L) {
        builder.setProgress(0, 0, isActive)
      } else {
        val pct = ((bytesDone.toDouble() / bytesTotal.toDouble()) * 1000.0)
          .toInt()
          .coerceIn(0, 1000)
        builder.setProgress(1000, pct, false)
      }

      if (canPause) {
        builder.addAction(
          0,
          "Pause",
          actionPendingIntent(context, TransferActionReceiver.ACTION_PAUSE, primaryId, 1),
        )
      } else if (canResume) {
        builder.addAction(
          0,
          "Resume",
          actionPendingIntent(context, TransferActionReceiver.ACTION_RESUME, primaryId, 2),
        )
      }
      builder.addAction(
        0,
        "Cancel",
        actionPendingIntent(context, TransferActionReceiver.ACTION_CANCEL, primaryId, 3),
      )

      return builder.build()
    }

    private fun actionPendingIntent(
      context: Context,
      action: String,
      transferId: String,
      requestCode: Int,
    ): PendingIntent {
      val intent = Intent(context, TransferActionReceiver::class.java).apply {
        this.action = action
        putExtra(TransferActionReceiver.EXTRA_TRANSFER_ID, transferId)
      }
      return PendingIntent.getBroadcast(
        context,
        requestCode,
        intent,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
      )
    }

    private fun notifySafely(
      context: Context,
      nm: NotificationManager?,
      id: Int,
      notification: Notification,
    ) {
      if (nm == null) return
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        val granted = ContextCompat.checkSelfPermission(
          context,
          Manifest.permission.POST_NOTIFICATIONS,
        ) == PackageManager.PERMISSION_GRANTED
        if (!granted) return
      }
      runCatching { nm.notify(id, notification) }
    }
  }
}
