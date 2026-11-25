package com.x1f4r.legion

import android.app.Activity
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.FileInputStream
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

@InvokeArg
class InstallApkArgs {
  var path: String? = null
}

/**
 * Native Android self-update plugin backed by [PackageInstaller].
 *
 * Only installs verified APK files located inside this app's private data directory. Declares
 * `USER_ACTION_NOT_REQUIRED` on Android 12+ so that when Android permits a prompt-free self-update
 * (same installer package + `UPDATE_PACKAGES_WITHOUT_USER_ACTION`) the install applies without
 * interrupting the user, and falls back to Android's standard confirmation dialog otherwise.
 */
@TauriPlugin
class AppUpdaterPlugin(private val activity: Activity) : Plugin(activity) {
  private val ioExecutor = Executors.newSingleThreadExecutor()

  private fun currentInstallerPackage(): String? {
    return try {
      val pm = activity.packageManager
      val pkg = activity.packageName
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        pm.getInstallSourceInfo(pkg).installingPackageName
      } else {
        @Suppress("DEPRECATION")
        pm.getInstallerPackageName(pkg)
      }
    } catch (e: Exception) {
      Log.w(TAG, "Could not inspect install source info: ${e.message}")
      null
    }
  }

  private fun currentUpdateOwnerPackage(): String? {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      return null
    }
    return try {
      activity.packageManager.getInstallSourceInfo(activity.packageName).updateOwnerPackageName
    } catch (e: Exception) {
      Log.w(TAG, "Could not inspect update owner info: ${e.message}")
      null
    }
  }

  private fun capabilitiesPayload(): JSObject {
    val canInstall = activity.packageManager.canRequestPackageInstalls()
    val installer = currentInstallerPackage()
    val updateOwner = currentUpdateOwnerPackage()
    val selfInstaller = installer == activity.packageName
    // Android 12+ (API 31+) allows prompt-free self-updates when UPDATE_PACKAGES_WITHOUT_USER_ACTION
    // is declared, Install unknown apps is granted, and no competing update owner holds the package
    // (on API 34+, Android checks updateOwnerPackageName; on API 31-33, installerPackageName).
    val ownerAllowsSilent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      updateOwner == null || updateOwner == activity.packageName
    } else {
      selfInstaller || installer == null
    }
    val supportsSilent = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && canInstall && ownerAllowsSilent
    return JSObject().apply {
      put("canRequestPackageInstalls", canInstall)
      put("isSelfInstaller", selfInstaller)
      put("installerPackage", installer ?: "")
      put("apiLevel", Build.VERSION.SDK_INT)
      put("supportsSilentSelfUpdate", supportsSilent)
    }
  }

  @Command
  fun capabilities(invoke: Invoke) {
    val payload = capabilitiesPayload()
    Log.i(TAG, "capabilities: $payload")
    invoke.resolve(payload)
  }

  @Command
  fun requestInstallPermission(invoke: Invoke) {
    if (activity.packageManager.canRequestPackageInstalls()) {
      val out = capabilitiesPayload()
      out.put("granted", true)
      invoke.resolve(out)
      return
    }
    val intent = Intent(
      Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
      Uri.parse("package:${activity.packageName}"),
    )
    startActivityForResult(invoke, intent, "onUnknownSourcesResult")
  }

  @ActivityCallback
  fun onUnknownSourcesResult(invoke: Invoke, _result: ActivityResult) {
    val granted = activity.packageManager.canRequestPackageInstalls()
    val out = capabilitiesPayload()
    out.put("granted", granted)
    Log.i(TAG, "onUnknownSourcesResult: granted=$granted")
    invoke.resolve(out)
  }

  @Command
  fun installApk(invoke: Invoke) {
    val args = invoke.parseArgs(InstallApkArgs::class.java)
    val rawPath = args.path?.trim().orEmpty()
    if (rawPath.isEmpty()) {
      invoke.reject("APK path is required.")
      return
    }

    if (!activity.packageManager.canRequestPackageInstalls()) {
      val out = capabilitiesPayload()
      out.put("status", "needs_permission")
      out.put("prompted", false)
      out.put("message", "Allow Legion to install updates in Android settings.")
      invoke.resolve(out)
      return
    }

    val apkFile = File(rawPath).canonicalFile
    val allowedRoot = File(activity.dataDir, "updates").canonicalFile
    val allowedRootPath = allowedRoot.path.trimEnd(File.separatorChar) + File.separatorChar
    if (!apkFile.path.startsWith(allowedRootPath) || !apkFile.isFile) {
      invoke.reject("Refusing to install APK outside the app's private updates directory.")
      return
    }

    val installerPkg = currentInstallerPackage()
    Log.i(
      TAG,
      "Starting PackageInstaller session: apk=${apkFile.name} bytes=${apkFile.length()} api=${Build.VERSION.SDK_INT} installer=$installerPkg",
    )

    ioExecutor.execute {
      var session: PackageInstaller.Session? = null
      var sessionId = -1
      try {
        val installer = activity.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
        params.setAppPackageName(activity.packageName)
        params.setSize(apkFile.length())
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
          params.setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
        }

        sessionId = installer.createSession(params)
        pendingInstalls[sessionId] = PendingInstall(invoke)
        session = installer.openSession(sessionId)

        FileInputStream(apkFile).use { input ->
          session.openWrite("base.apk", 0, apkFile.length()).use { output ->
            val buf = ByteArray(64 * 1024)
            while (true) {
              val read = input.read(buf)
              if (read <= 0) break
              output.write(buf, 0, read)
            }
            session.fsync(output)
          }
        }

        val callbackIntent = Intent(activity, UpdateInstallReceiver::class.java)
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
          PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
        } else {
          PendingIntent.FLAG_UPDATE_CURRENT
        }
        val pendingIntent = PendingIntent.getBroadcast(
          activity,
          sessionId,
          callbackIntent,
          flags,
        )

        Log.i(TAG, "Committing PackageInstaller session $sessionId")
        session.commit(pendingIntent.intentSender)
      } catch (e: Exception) {
        Log.e(TAG, "Failed to commit PackageInstaller session $sessionId", e)
        if (sessionId >= 0) {
          pendingInstalls.remove(sessionId)
        }
        runCatching { session?.abandon() }
        invoke.reject("Could not start Android package install: ${e.message}")
      } finally {
        runCatching { session?.close() }
      }
    }
  }

  private class PendingInstall(val invoke: Invoke) {
    val prompted = AtomicBoolean(false)
  }

  companion object {
    private const val TAG = "legion-updater"
    private val pendingInstalls = ConcurrentHashMap<Int, PendingInstall>()

    fun onPendingUserAction(sessionId: Int) {
      val entry = pendingInstalls[sessionId]
      entry?.prompted?.set(true)
      Log.i(TAG, "Session $sessionId required user confirmation prompt")
    }

    fun onInstallFinished(sessionId: Int, status: String, message: String) {
      val entry = pendingInstalls.remove(sessionId) ?: return
      val payload = JSObject().apply {
        put("status", status)
        put("prompted", entry.prompted.get())
        put("message", message)
      }
      Log.i(TAG, "Session $sessionId finished: $payload")
      entry.invoke.resolve(payload)
    }
  }
}
