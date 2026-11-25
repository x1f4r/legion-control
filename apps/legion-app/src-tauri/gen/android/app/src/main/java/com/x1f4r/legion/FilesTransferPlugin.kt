package com.x1f4r.legion

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.OpenableColumns
import android.util.Log
import androidx.activity.result.ActivityResult
import androidx.core.content.ContextCompat
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class ConnectBridgeArgs {
  var socketPath: String? = null
  var token: String? = null
}

/**
 * Foreground Android Tauri plugin for Files transfers:
 * - Connects the process-scoped [TransferBridge] to Rust's private `LocalSocket`.
 * - Prompts for `POST_NOTIFICATIONS` lazily on first transfer with a one-sentence rationale.
 * - Launches the system document picker (`ACTION_OPEN_DOCUMENT`) with persistable read grants.
 */
@TauriPlugin
class FilesTransferPlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun connectBridge(invoke: Invoke) {
    val args = invoke.parseArgs(ConnectBridgeArgs::class.java)
    val socketPath = args.socketPath?.trim().orEmpty()
    val token = args.token?.trim().orEmpty()
    if (socketPath.isEmpty() || token.isEmpty()) {
      invoke.reject("Bridge socketPath and token are required.")
      return
    }
    TransferBridge.connect(activity.applicationContext, socketPath, token)
    invoke.resolve(JSObject().put("ok", true))
  }

  @Command
  fun ensureNotificationPermission(invoke: Invoke) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
      invoke.resolve(JSObject().put("granted", true))
      return
    }
    val hostActivity = TransferBridge.currentActivity() ?: (activity as? MainActivity)
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
        .setTitle("Transfer notifications")
        .setMessage("Legion uses a notification to keep downloads and uploads running while you use other apps.")
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

  @Command
  fun pickUploadFiles(invoke: Invoke) {
    val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
      addCategory(Intent.CATEGORY_OPENABLE)
      type = "*/*"
      putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
      addFlags(
        Intent.FLAG_GRANT_READ_URI_PERMISSION or
          Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION,
      )
    }
    startActivityForResult(invoke, intent, "onPickUploadFilesResult")
  }

  @ActivityCallback
  fun onPickUploadFilesResult(invoke: Invoke, result: ActivityResult) {
    val data = result.data
    if (result.resultCode != Activity.RESULT_OK || data == null) {
      invoke.resolve(JSObject().apply {
        put("cancelled", true)
        put("files", JSArray())
      })
      return
    }

    val uris = mutableListOf<Uri>()
    val clip = data.clipData
    if (clip != null && clip.itemCount > 0) {
      for (i in 0 until clip.itemCount) {
        clip.getItemAt(i)?.uri?.let { uris.add(it) }
      }
    } else {
      data.data?.let { uris.add(it) }
    }

    if (uris.isEmpty()) {
      invoke.resolve(JSObject().apply {
        put("cancelled", true)
        put("files", JSArray())
      })
      return
    }

    val resolver = activity.applicationContext.contentResolver
    val filesArray = JSArray()
    for (uri in uris) {
      runCatching {
        resolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
      }.onFailure { e ->
        Log.w(TAG, "Could not persist read permission for $uri: ${e.message}")
      }

      var displayName = uri.lastPathSegment?.substringAfterLast('/') ?: "file"
      var size = 0L
      runCatching {
        resolver.query(
          uri,
          arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE),
          null,
          null,
          null,
        )?.use { cursor ->
          if (cursor.moveToFirst()) {
            val nameIdx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (nameIdx >= 0 && !cursor.isNull(nameIdx)) {
              val queried = cursor.getString(nameIdx)?.trim().orEmpty()
              if (queried.isNotEmpty()) displayName = queried
            }
            val sizeIdx = cursor.getColumnIndex(OpenableColumns.SIZE)
            if (sizeIdx >= 0 && !cursor.isNull(sizeIdx)) {
              size = cursor.getLong(sizeIdx).coerceAtLeast(0L)
            }
          }
        }
      }

      val mime = runCatching { resolver.getType(uri) }.getOrNull()
      val entry = JSObject().apply {
        put("uri", uri.toString())
        put("name", displayName)
        put("size", size)
        if (!mime.isNullOrBlank()) {
          put("mime", mime)
        }
      }
      filesArray.put(entry)
    }

    invoke.resolve(JSObject().apply {
      put("cancelled", false)
      put("files", filesArray)
    })
  }

  companion object {
    private const val TAG = "legion-transfers"
  }
}
