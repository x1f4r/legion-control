package com.x1f4r.legion

import android.app.DownloadManager
import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Base64
import android.util.Log
import androidx.core.content.FileProvider
import org.json.JSONObject
import java.io.BufferedReader
import java.io.BufferedWriter
import java.io.File
import java.io.FileInputStream
import java.io.FileNotFoundException
import java.io.FileOutputStream
import java.io.InputStream
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.lang.ref.WeakReference
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/**
 * Process-scoped Android bridge between Rust's transfer queue and Android platform services.
 *
 * Holds `applicationContext` and connects over the private owner-only (`0o600`, session-token
 * authenticated) Unix domain socket `<dataDir>/transfers/bridge.sock` so the foreground service,
 * MediaStore downloads, `content://` uploads, and notification actions keep working even after
 * `MainActivity` is swiped away from Recents.
 */
object TransferBridge {
  private const val TAG = "legion-transfers"
  private const val MAX_CHUNK_BYTES = 64 * 1024

  private class NoHandlerException(message: String) : IllegalStateException(message)

  @Volatile
  private var appContext: Context? = null

  @Volatile
  private var activeActivityRef: WeakReference<MainActivity>? = null

  @Volatile
  private var writer: BufferedWriter? = null

  @Volatile
  private var activeSocket: LocalSocket? = null

  @Volatile
  private var lastUnmeteredWifi: Boolean = true

  private val writerLock = Any()
  private val networkRegistered = AtomicBoolean(false)
  private val workerPool = Executors.newCachedThreadPool()
  private val uriReaders = ConcurrentHashMap<Long, InputStream>()
  private val nextHandle = AtomicLong(1L)

  fun setActiveActivity(activity: MainActivity?) {
    activeActivityRef = activity?.let { WeakReference(it) }
    if (activity != null && appContext == null) {
      appContext = activity.applicationContext
    }
  }

  fun currentActivity(): MainActivity? {
    val act = activeActivityRef?.get() ?: return null
    return if (act.isFinishing || act.isDestroyed) null else act
  }

  fun connect(context: Context, socketPath: String, token: String) {
    val ctx = context.applicationContext
    appContext = ctx
    registerNetworkMonitor(ctx)

    workerPool.execute {
      runCatching {
        TransferPathPolicy.pruneCacheDirectory(File(ctx.cacheDir, "open-externally"))
      }
      connectLoop(ctx, socketPath, token)
    }
  }

  fun sendEvent(payload: JSONObject) {
    val line = payload.toString() + "\n"
    synchronized(writerLock) {
      try {
        writer?.write(line)
        writer?.flush()
      } catch (e: Exception) {
        if (BuildConfig.DEBUG) {
          Log.w(TAG, "Could not push event to Rust transfer bridge: ${e.message}")
        } else {
          Log.w(TAG, "Could not push event to Rust transfer bridge")
        }
      }
    }
  }

  fun isUnmeteredWifi(context: Context): Boolean {
    return try {
      val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
        ?: return true
      val active = cm.activeNetwork ?: return false
      val caps = cm.getNetworkCapabilities(active) ?: return false
      caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
    } catch (e: Exception) {
      if (BuildConfig.DEBUG) {
        Log.w(TAG, "Could not inspect active network meter state: ${e.message}")
      } else {
        Log.w(TAG, "Could not inspect active network meter state")
      }
      true
    }
  }

  private fun registerNetworkMonitor(context: Context) {
    if (!networkRegistered.compareAndSet(false, true)) return
    val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return
    lastUnmeteredWifi = isUnmeteredWifi(context)
    try {
      cm.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
          pushNetworkState(context)
        }

        override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
          pushNetworkState(context, capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED))
        }

        override fun onLost(network: Network) {
          pushNetworkState(context, false)
        }
      })
    } catch (e: Exception) {
      if (BuildConfig.DEBUG) {
        Log.w(TAG, "Could not register default network callback: ${e.message}")
      } else {
        Log.w(TAG, "Could not register default network callback")
      }
    }
  }

  private fun pushNetworkState(context: Context, explicit: Boolean? = null) {
    val unmetered = explicit ?: isUnmeteredWifi(context)
    if (unmetered == lastUnmeteredWifi) return
    lastUnmeteredWifi = unmetered
    sendEvent(JSONObject().apply {
      put("event", "network_changed")
      put("unmeteredWifi", unmetered)
    })
  }

  private fun connectLoop(context: Context, socketPath: String, token: String) {
    // Close any previous bridge socket cleanly when reconnecting.
    synchronized(writerLock) {
      runCatching { activeSocket?.close() }
      activeSocket = null
      writer = null
    }

    var socket: LocalSocket? = null
    for (attempt in 1..20) {
      try {
        val candidate = LocalSocket()
        candidate.connect(LocalSocketAddress(socketPath, LocalSocketAddress.Namespace.FILESYSTEM))
        socket = candidate
        break
      } catch (e: Exception) {
        if (attempt == 20) {
          if (BuildConfig.DEBUG) {
            Log.e(TAG, "Failed to connect to Rust transfer bridge socket: ${e.message}")
          } else {
            Log.e(TAG, "Failed to connect to Rust transfer bridge socket")
          }
          return
        }
        try {
          Thread.sleep(100L)
        } catch (_: InterruptedException) {
          return
        }
      }
    }

    val liveSocket = socket ?: return
    val outWriter = BufferedWriter(OutputStreamWriter(liveSocket.outputStream, Charsets.UTF_8))
    val inReader = BufferedReader(InputStreamReader(liveSocket.inputStream, Charsets.UTF_8))

    synchronized(writerLock) {
      activeSocket = liveSocket
      writer = outWriter
    }

    val unmetered = isUnmeteredWifi(context)
    lastUnmeteredWifi = unmetered
    sendRaw(JSONObject().apply {
      put("type", "hello")
      put("token", token)
      put("unmeteredWifi", unmetered)
    })
    Log.i(TAG, "Connected to Rust transfer bridge (unmeteredWifi=$unmetered)")

    try {
      while (true) {
        val line = inReader.readLine() ?: break
        if (line.isBlank()) continue
        val req = runCatching { JSONObject(line) }.getOrNull() ?: continue
        val id = req.optLong("id", -1L)
        val method = req.optString("method", "")
        val params = req.optJSONObject("params") ?: JSONObject()
        if (id < 0L || method.isEmpty()) continue
        workerPool.execute {
          handleRequest(context, id, method, params)
        }
      }
    } catch (e: Exception) {
      if (BuildConfig.DEBUG) {
        Log.i(TAG, "Rust transfer bridge reader closed: ${e.message}")
      } else {
        Log.i(TAG, "Rust transfer bridge reader closed")
      }
    } finally {
      closeAllUriReaders()
      synchronized(writerLock) {
        if (activeSocket === liveSocket) {
          activeSocket = null
          writer = null
        }
      }
      runCatching { liveSocket.close() }
    }
  }

  private fun sendRaw(obj: JSONObject) {
    val line = obj.toString() + "\n"
    synchronized(writerLock) {
      try {
        writer?.write(line)
        writer?.flush()
      } catch (e: Exception) {
        if (BuildConfig.DEBUG) {
          Log.w(TAG, "Failed writing to Rust transfer bridge: ${e.message}")
        } else {
          Log.w(TAG, "Failed writing to Rust transfer bridge")
        }
      }
    }
  }

  private fun handleRequest(context: Context, id: Long, method: String, params: JSONObject) {
    try {
      val result: JSONObject = when (method) {
        "syncService" -> {
          TransferForegroundService.sync(context, params)
          JSONObject().put("ok", true)
        }
        "notifyComplete" -> {
          TransferForegroundService.notifyComplete(context, params)
          JSONObject().put("ok", true)
        }
        "saveToDownloads" -> saveToDownloads(context, params)
        "stageOpenExternally" -> stageOpenExternally(context, params)
        "checkCacheFile" -> checkCacheFile(context, params)
        "deleteSavedDownload" -> deleteSavedDownload(context, params)
        "openTransferFile" -> openTransferFile(context, params)
        "showInFolder" -> showInFolder(context)
        "openUriRead" -> openUriRead(context, params)
        "readUriChunk" -> readUriChunk(params)
        "skipUriBytes" -> skipUriBytes(params)
        "closeUriRead" -> closeUriRead(params)
        "releaseUriPermission" -> releaseUriPermission(context, params)
        else -> throw IllegalArgumentException("Unknown bridge method: $method")
      }
      sendRaw(JSONObject().apply {
        put("id", id)
        put("result", result)
      })
    } catch (e: Exception) {
      if (BuildConfig.DEBUG) {
        Log.w(TAG, "Bridge method $method failed: ${e.message}")
      } else {
        Log.w(TAG, "Bridge method $method failed (${e.javaClass.simpleName})")
      }
      val errorCode = when (e) {
        is NoHandlerException -> "no_handler"
        is SecurityException -> "denied"
        is FileNotFoundException -> "not_found"
        else -> null
      }
      sendRaw(JSONObject().apply {
        put("id", id)
        put("error", e.message ?: "Android transfer operation failed.")
        if (errorCode != null) {
          put("errorCode", errorCode)
        }
      })
    }
  }

  private fun verifyPartFile(context: Context, rawPath: String): File {
    val allowedRoot = File(context.dataDir, "transfers/parts")
    return TransferPathPolicy.resolveContainedChild(allowedRoot, File(rawPath), requireExistingFile = true)
  }

  private fun saveToDownloads(context: Context, params: JSONObject): JSONObject {
    val tempPath = params.optString("tempPath", "")
    val fileName = TransferPathPolicy.sanitizeFileName(params.optString("fileName", "download"))
    // Always derive MIME on this device from the sanitized file extension.
    val mime = TransferPathPolicy.mimeForFileName(fileName)
    val partFile = verifyPartFile(context, tempPath)

    val resolver = context.contentResolver
    val values = ContentValues().apply {
      put(MediaStore.Downloads.DISPLAY_NAME, fileName)
      put(MediaStore.Downloads.MIME_TYPE, mime)
      put(MediaStore.Downloads.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/Legion")
      put(MediaStore.Downloads.IS_PENDING, 1)
    }

    val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
      ?: throw IllegalStateException("Android MediaStore refused to create Downloads/Legion/$fileName.")

    try {
      resolver.openOutputStream(uri)?.use { out ->
        FileInputStream(partFile).use { input ->
          val buf = ByteArray(MAX_CHUNK_BYTES)
          while (true) {
            val read = input.read(buf)
            if (read <= 0) break
            out.write(buf, 0, read)
          }
          out.flush()
        }
      } ?: throw IllegalStateException("Could not open MediaStore output stream.")

      if (BuildConfig.DEBUG) {
        val delayFile = File(context.dataDir, "debug-delay-save-ms")
        if (delayFile.isFile) {
          val delayMs = runCatching { delayFile.readText().trim().toLong() }.getOrDefault(0L)
            .coerceIn(0L, 5000L)
          if (delayMs > 0L) {
            Thread.sleep(delayMs)
          }
        }
      }

      val publish = ContentValues().apply {
        put(MediaStore.Downloads.IS_PENDING, 0)
      }
      resolver.update(uri, publish, null, null)
    } catch (e: Exception) {
      runCatching { resolver.delete(uri, null, null) }
      throw e
    }

    runCatching { partFile.delete() }
    if (BuildConfig.DEBUG) {
      Log.i(TAG, "Saved download to MediaStore Downloads/Legion/$fileName ($uri)")
    }
    return JSONObject().put("uri", uri.toString())
  }

  private fun stageOpenExternally(context: Context, params: JSONObject): JSONObject {
    val transferId = TransferPathPolicy.sanitizeFileName(params.optString("transferId", "item"))
    val tempPath = params.optString("tempPath", "")
    val fileName = TransferPathPolicy.sanitizeFileName(params.optString("fileName", "file"))
    if (!TransferPathPolicy.canOpenFileName(fileName)) {
      throw SecurityException("Refusing to stage disallowed file type for direct opening.")
    }
    val partFile = verifyPartFile(context, tempPath)

    val cacheDir = File(context.cacheDir, "open-externally")
    if (!cacheDir.exists() && !cacheDir.mkdirs()) {
      throw IllegalStateException("Could not create open-externally cache directory.")
    }
    TransferPathPolicy.pruneCacheDirectory(cacheDir)

    val destFile = TransferPathPolicy.createUniqueCacheFile(cacheDir, transferId, fileName)
    try {
      FileInputStream(partFile).use { input ->
        FileOutputStream(destFile).use { out ->
          val buf = ByteArray(MAX_CHUNK_BYTES)
          while (true) {
            val read = input.read(buf)
            if (read <= 0) break
            out.write(buf, 0, read)
          }
          out.flush()
        }
      }
    } catch (e: Exception) {
      runCatching { destFile.delete() }
      throw e
    }
    runCatching { partFile.delete() }

    val authority = "${context.packageName}.fileprovider"
    val uri = FileProvider.getUriForFile(context, authority, destFile)
    return JSONObject().put("uri", uri.toString())
  }

  private fun checkCacheFile(context: Context, params: JSONObject): JSONObject {
    val uriOrPath = params.optString("uriOrPath", "").trim()
    val expectedSize = params.optLong("expectedSize", -1L)
    if (uriOrPath.isEmpty()) {
      return JSONObject().put("exists", false)
    }
    val cacheDir = File(context.cacheDir, "open-externally")
    val candidate = runCatching {
      if (uriOrPath.startsWith("content://")) {
        val uri = Uri.parse(uriOrPath)
        if (uri.authority != "${context.packageName}.fileprovider") {
          return@runCatching null
        }
        val seg = uri.lastPathSegment?.substringAfterLast('/')
        if (seg.isNullOrBlank()) return@runCatching null
        TransferPathPolicy.resolveContainedChild(
          cacheDir,
          File(cacheDir, seg),
          requireExistingFile = true,
        )
      } else {
        TransferPathPolicy.resolveContainedChild(
          cacheDir,
          File(uriOrPath),
          requireExistingFile = true,
        )
      }
    }.getOrNull()
    val exists = candidate != null &&
      candidate.isFile &&
      (expectedSize < 0L || candidate.length() == expectedSize)
    return JSONObject().put("exists", exists)
  }

  private fun deleteSavedDownload(context: Context, params: JSONObject): JSONObject {
    val uriOrPath = params.optString("uriOrPath", "").trim()
    if (uriOrPath.isEmpty()) {
      return JSONObject().put("ok", true).put("deleted", false)
    }

    var deleted = false
    if (uriOrPath.startsWith("content://")) {
      val uri = Uri.parse(uriOrPath)
      if (!TransferPathPolicy.isDeletableSavedDownloadUri(uri.authority, context.packageName)) {
        throw SecurityException("Refusing to delete content URI from disallowed authority.")
      }
      val rows = try {
        context.contentResolver.delete(uri, null, null)
      } catch (_: java.io.FileNotFoundException) {
        0
      }
      if (rows > 0) {
        deleted = true
      }
      if (uri.authority == "${context.packageName}.fileprovider") {
        val seg = uri.lastPathSegment?.substringAfterLast('/')
        if (!seg.isNullOrBlank()) {
          val cacheDir = File(context.cacheDir, "open-externally")
          val target = File(cacheDir, seg)
          val existedBefore = target.exists()
          if (TransferPathPolicy.deleteContainedCacheFile(cacheDir, target)) {
            deleted = true
          } else if (existedBefore && target.exists()) {
            throw java.io.IOException("Could not delete cached file ${target.name}.")
          }
        }
      }
    } else {
      val cacheDir = File(context.cacheDir, "open-externally")
      val target = File(uriOrPath)
      val existedBefore = target.exists()
      deleted = TransferPathPolicy.deleteContainedCacheFile(cacheDir, target)
      if (!deleted && existedBefore && target.exists()) {
        throw java.io.IOException("Could not delete cached file ${target.name}.")
      }
    }

    return JSONObject().put("ok", true).put("deleted", deleted)
  }

  private fun resolveDisplayNameForUri(context: Context, uri: Uri): String {
    runCatching {
      context.contentResolver.query(
        uri,
        arrayOf(OpenableColumns.DISPLAY_NAME),
        null,
        null,
        null,
      )?.use { cursor ->
        if (cursor.moveToFirst()) {
          val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
          if (idx >= 0 && !cursor.isNull(idx)) {
            val queried = cursor.getString(idx)?.trim().orEmpty()
            if (queried.isNotEmpty()) return queried
          }
        }
      }
    }
    return uri.lastPathSegment?.substringAfterLast('/') ?: "download"
  }

  private fun openTransferFile(context: Context, params: JSONObject): JSONObject {
    val uriOrPath = params.optString("uriOrPath", "").trim()
    if (uriOrPath.isEmpty()) {
      throw IllegalArgumentException("File URI or path is required.")
    }

    val uri: Uri
    val displayName: String
    if (uriOrPath.startsWith("content://")) {
      uri = Uri.parse(uriOrPath)
      displayName = resolveDisplayNameForUri(context, uri)
    } else {
      val allowedCache = File(context.cacheDir, "open-externally")
      val file = TransferPathPolicy.resolveContainedChild(
        allowedCache,
        File(uriOrPath),
        requireExistingFile = true,
      )
      displayName = file.name
      uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    }

    if (!TransferPathPolicy.canOpenFileName(displayName)) {
      throw SecurityException("Legion refuses to open this file type directly.")
    }
    // Always derive MIME on this device from the saved file's real extension.
    val mime = TransferPathPolicy.mimeForFileName(displayName)

    val viewIntent = Intent(Intent.ACTION_VIEW).apply {
      setDataAndType(uri, mime)
      addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    val handlers = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      context.packageManager.queryIntentActivities(
        viewIntent,
        PackageManager.ResolveInfoFlags.of(PackageManager.MATCH_DEFAULT_ONLY.toLong()),
      )
    } else {
      @Suppress("DEPRECATION")
      context.packageManager.queryIntentActivities(viewIntent, PackageManager.MATCH_DEFAULT_ONLY)
    }.filter { it.activityInfo?.packageName != context.packageName }
    if (handlers.isEmpty()) {
      throw NoHandlerException("No app on this phone can open this file type.")
    }

    val chooser = Intent.createChooser(viewIntent, "Open with").apply {
      addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      clipData = ClipData.newRawUri("", uri)
    }

    val act = currentActivity()
    if (act != null) {
      val latch = CountDownLatch(1)
      val launchErr = AtomicReference<Throwable?>(null)
      act.runOnUiThread {
        try {
          act.startActivity(chooser)
        } catch (t: Throwable) {
          launchErr.set(t)
        } finally {
          latch.countDown()
        }
      }
      val completed = latch.await(5L, TimeUnit.SECONDS)
      val err = launchErr.get()
      if (err is ActivityNotFoundException) {
        throw NoHandlerException("No app on this phone can open this file type.")
      }
      if (err != null) {
        if (BuildConfig.DEBUG) {
          Log.w(TAG, "Activity chooser launch failed: ${err.message}")
        } else {
          Log.w(TAG, "Activity chooser launch failed")
        }
        throw IllegalStateException(err.message ?: "Could not open the app chooser.", err)
      }
      if (!completed) {
        throw IllegalStateException("Timed out waiting to open the app chooser.")
      }
    } else {
      chooser.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      try {
        context.startActivity(chooser)
      } catch (e: ActivityNotFoundException) {
        throw NoHandlerException("No app on this phone can open this file type.")
      }
    }
    if (BuildConfig.DEBUG) {
      Log.i(TAG, "Opened chooser for $displayName (mime=$mime, handlers=${handlers.size})")
    }
    return JSONObject().put("ok", true).put("mime", mime)
  }

  private fun showInFolder(context: Context): JSONObject {
    val downloadsIntent = Intent(DownloadManager.ACTION_VIEW_DOWNLOADS)
    val act = currentActivity()
    if (act != null) {
      act.runOnUiThread {
        runCatching { act.startActivity(downloadsIntent) }
          .onFailure { e ->
            if (BuildConfig.DEBUG) {
              Log.w(TAG, "Could not open Downloads folder: ${e.message}")
            } else {
              Log.w(TAG, "Could not open Downloads folder")
            }
          }
      }
    } else {
      downloadsIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      context.startActivity(downloadsIntent)
    }
    return JSONObject().put("ok", true)
  }

  private fun openUriRead(context: Context, params: JSONObject): JSONObject {
    val rawUri = params.optString("uri", "").trim()
    if (!rawUri.startsWith("content://")) {
      throw IllegalArgumentException("Expected a content:// URI.")
    }
    val uri = Uri.parse(rawUri)
    val resolver = context.contentResolver
    var size = 0L
    runCatching {
      resolver.query(uri, arrayOf(OpenableColumns.SIZE), null, null, null)?.use { cursor ->
        if (cursor.moveToFirst()) {
          val idx = cursor.getColumnIndex(OpenableColumns.SIZE)
          if (idx >= 0 && !cursor.isNull(idx)) {
            size = cursor.getLong(idx).coerceAtLeast(0L)
          }
        }
      }
    }
    val stream = resolver.openInputStream(uri)
      ?: throw IllegalStateException("Could not open selected document for reading.")
    val handle = nextHandle.getAndIncrement()
    uriReaders[handle] = stream
    return JSONObject().apply {
      put("handle", handle)
      put("size", size)
    }
  }

  private fun readUriChunk(params: JSONObject): JSONObject {
    val handle = params.optLong("handle", -1L)
    val maxBytes = params.optInt("maxBytes", MAX_CHUNK_BYTES).coerceIn(1, MAX_CHUNK_BYTES)
    val stream = uriReaders[handle]
      ?: throw IllegalStateException("Upload URI reader handle $handle is closed.")
    val buf = ByteArray(maxBytes)
    val read = stream.read(buf)
    if (read <= 0) {
      return JSONObject().apply {
        put("data", "")
        put("eof", true)
      }
    }
    val encoded = Base64.encodeToString(buf, 0, read, Base64.NO_WRAP)
    return JSONObject().apply {
      put("data", encoded)
      put("eof", false)
    }
  }

  private fun skipUriBytes(params: JSONObject): JSONObject {
    val handle = params.optLong("handle", -1L)
    var remaining = params.optLong("bytes", 0L).coerceAtLeast(0L)
    val stream = uriReaders[handle]
      ?: throw IllegalStateException("Upload URI reader handle $handle is closed.")
    val discard = ByteArray(MAX_CHUNK_BYTES)
    while (remaining > 0L) {
      val skipped = stream.skip(remaining)
      if (skipped > 0L) {
        remaining -= skipped
      } else {
        val want = minOf(remaining, discard.size.toLong()).toInt()
        val read = stream.read(discard, 0, want)
        if (read <= 0) break
        remaining -= read.toLong()
      }
    }
    return JSONObject().put("ok", true)
  }

  private fun closeUriRead(params: JSONObject): JSONObject {
    val handle = params.optLong("handle", -1L)
    uriReaders.remove(handle)?.let { runCatching { it.close() } }
    return JSONObject().put("ok", true)
  }

  private fun releaseUriPermission(context: Context, params: JSONObject): JSONObject {
    val rawUri = params.optString("uri", "").trim()
    if (rawUri.startsWith("content://")) {
      runCatching {
        context.contentResolver.releasePersistableUriPermission(
          Uri.parse(rawUri),
          Intent.FLAG_GRANT_READ_URI_PERMISSION,
        )
      }
    }
    return JSONObject().put("ok", true)
  }

  private fun closeAllUriReaders() {
    val keys = uriReaders.keys().toList()
    for (k in keys) {
      uriReaders.remove(k)?.let { runCatching { it.close() } }
    }
  }
}
