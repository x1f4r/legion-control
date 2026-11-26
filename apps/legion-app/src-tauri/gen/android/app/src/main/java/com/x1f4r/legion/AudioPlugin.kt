package com.x1f4r.legion

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class ConnectAudioBridgeArgs {
  var socketPath: String? = null
  var token: String? = null
}

/**
 * Foreground Android Tauri plugin for background music playback:
 * - Connects the process-scoped [AudioBridge] to Rust's private `LocalSocket`.
 * - Prompts for `POST_NOTIFICATIONS` lazily once on first music playback with a single-sentence
 *   rationale; refusing never blocks playback.
 */
@TauriPlugin
class AudioPlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun connectAudioBridge(invoke: Invoke) {
    val args = invoke.parseArgs(ConnectAudioBridgeArgs::class.java)
    val socketPath = args.socketPath?.trim().orEmpty()
    val token = args.token?.trim().orEmpty()
    if (socketPath.isEmpty() || token.isEmpty()) {
      invoke.reject("Audio bridge socketPath and token are required.")
      return
    }
    AudioBridge.connect(activity.applicationContext, socketPath, token)
    invoke.resolve(JSObject().put("ok", true))
  }

  @Command
  fun ensureMusicNotificationPermission(invoke: Invoke) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
      invoke.resolve(JSObject().put("granted", true))
      return
    }
    val hostActivity = AudioBridge.currentActivity() ?: (activity as? MainActivity)
    if (hostActivity == null || hostActivity.isFinishing || hostActivity.isDestroyed) {
      invoke.resolve(JSObject().put("granted", false))
      return
    }
    val alreadyGranted = ContextCompat.checkSelfPermission(
      hostActivity,
      Manifest.permission.POST_NOTIFICATIONS,
    ) == PackageManager.PERMISSION_GRANTED
    if (alreadyGranted) {
      invoke.resolve(JSObject().put("granted", true))
      return
    }

    hostActivity.runOnUiThread {
      if (hostActivity.isFinishing || hostActivity.isDestroyed) {
        invoke.resolve(JSObject().put("granted", false))
        return@runOnUiThread
      }
      AlertDialog.Builder(hostActivity)
        .setTitle("Playback controls")
        .setMessage("Legion uses a notification to show playback controls and keep music playing while you use other apps.")
        .setCancelable(true)
        .setPositiveButton("Continue") { _, _ ->
          hostActivity.requestNotificationPermission { granted ->
            invoke.resolve(JSObject().put("granted", granted))
          }
        }
        .setNegativeButton("Not now") { _, _ ->
          invoke.resolve(JSObject().put("granted", false))
        }
        .setOnCancelListener {
          invoke.resolve(JSObject().put("granted", false))
        }
        .show()
    }
  }
}
