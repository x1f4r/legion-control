package com.x1f4r.legion

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import android.util.Log

/**
 * Receives commit results from Android's [PackageInstaller] session opened by [AppUpdaterPlugin].
 *
 * When Android requires explicit user confirmation (`STATUS_PENDING_USER_ACTION`), this receiver
 * launches the system confirmation intent; all terminal statuses are forwarded back to
 * [AppUpdaterPlugin] so the Rust updater can update state cleanly.
 */
class UpdateInstallReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    val sessionId = intent.getIntExtra(PackageInstaller.EXTRA_SESSION_ID, -1)
    val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
    val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE).orEmpty()

    Log.i(
      TAG,
      "PackageInstaller callback: session=$sessionId status=$status message=$message",
    )

    when (status) {
      PackageInstaller.STATUS_PENDING_USER_ACTION -> {
        val confirmIntent: Intent? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
          intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
        } else {
          @Suppress("DEPRECATION")
          intent.getParcelableExtra(Intent.EXTRA_INTENT)
        }
        AppUpdaterPlugin.onPendingUserAction(sessionId)
        if (confirmIntent != null) {
          confirmIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
          try {
            context.startActivity(confirmIntent)
          } catch (e: Exception) {
            Log.e(TAG, "Could not launch PackageInstaller confirmation activity", e)
            AppUpdaterPlugin.onInstallFinished(
              sessionId = sessionId,
              status = "failed",
              message = "Could not open the Android installer confirmation: ${e.message}",
            )
          }
        } else {
          AppUpdaterPlugin.onInstallFinished(
            sessionId = sessionId,
            status = "failed",
            message = "Android requested confirmation without providing an installer intent.",
          )
        }
      }

      PackageInstaller.STATUS_SUCCESS -> {
        AppUpdaterPlugin.onInstallFinished(
          sessionId = sessionId,
          status = "installed",
          message = "",
        )
      }

      PackageInstaller.STATUS_FAILURE_ABORTED -> {
        AppUpdaterPlugin.onInstallFinished(
          sessionId = sessionId,
          status = "cancelled",
          message = message.ifBlank { "The update was cancelled." },
        )
      }

      else -> {
        val detail = message.ifBlank { "Android package installation failed (status $status)." }
        AppUpdaterPlugin.onInstallFinished(
          sessionId = sessionId,
          status = "failed",
          message = detail,
        )
      }
    }
  }

  companion object {
    private const val TAG = "legion-updater"
  }
}
