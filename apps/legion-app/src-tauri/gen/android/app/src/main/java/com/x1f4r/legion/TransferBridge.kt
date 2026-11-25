package com.x1f4r.legion

import android.content.ClipData
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
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
import java.io.FileOutputStream
import java.io.InputStream
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.lang.ref.WeakReference
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

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
  private const val MAX_OPEN_EXTERNALLY_CACHE_FILES = 24

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
        Log.w(TAG, "Could not push event to Rust transfer bridge: ${e.message}")
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
      Log.w(TAG, "Could not inspect active network meter state: ${e.message}")
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
      Log.w(TAG, "Could not register default network callback: ${e.message}")
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
          Log.e(TAG, "Failed to connect to Rust transfer bridge socket: ${e.message}")
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
      Log.i(TAG, "Rust transfer bridge reader closed: ${e.message}")
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
        Log.w(TAG, "Failed writing to Rust transfer bridge: ${e.message}")
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
        "openTransferFile" -> openTransferFile(context, params)
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
      Log.w(TAG, "Bridge method $method failed: ${e.message}")
      sendRaw(JSONObject().apply {
        put("id", id)
        put("error", e.message ?: "Android transfer operation failed.")
      })
    }
  }

  private fun verifyPartFile(context: Context, rawPath: String): File {
    val partFile = File(rawPath).canonicalFile
    val allowedRoot = File(context.dataDir, "transfers/parts").canonicalFile
    val allowedPrefix = allowedRoot.path.trimEnd(File.separatorChar) + File.separatorChar
    if (!partFile.path.startsWith(allowedPrefix) || !partFile.isFile) {
      throw SecurityException("Refusing transfer file outside the app's private parts directory.")
    }
    return partFile
  }

  private fun sanitizeFileName(raw: String): String {
    val trimmed = raw.trim().replace('/', '_').replace('\\', '_').replace('\u0000', '_')
    val cleaned = trimmed.trimStart('.')
    return if (cleaned.isEmpty()) "download" else cleaned.take(180)
  }

  private fun saveToDownloads(context: Context, params: JSONObject): JSONObject {
    val tempPath = params.optString("tempPath", "")
    val fileName = sanitizeFileName(params.optString("fileName", "download"))
    val mime = params.optString("mime", "application/octet-stream")
      .ifBlank { "application/octet-stream" }
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

      val publish = ContentValues().apply {
        put(MediaStore.Downloads.IS_PENDING, 0)
      }
      resolver.update(uri, publish, null, null)
    } catch (e: Exception) {
      runCatching { resolver.delete(uri, null, null) }
      throw e
    }

    runCatching { partFile.delete() }
    Log.i(TAG, "Saved download to MediaStore Downloads/Legion/$fileName ($uri)")
    return JSONObject().put("uri", uri.toString())
  }

  private fun stageOpenExternally(context: Context, params: JSONObject): JSONObject {
    val transferId = sanitizeFileName(params.optString("transferId", "item"))
    val tempPath = params.optString("tempPath", "")
    val fileName = sanitizeFileName(params.optString("fileName", "file"))
    val partFile = verifyPartFile(context, tempPath)

    val cacheDir = File(context.cacheDir, "open-externally")
    if (!cacheDir.exists() && !cacheDir.mkdirs()) {
      throw IllegalStateException("Could not create open-externally cache directory.")
    }
    pruneOpenExternallyCache(cacheDir)

    val destFile = File(cacheDir, "$transferId-$fileName")
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
    runCatching { partFile.delete() }

    val authority = "${context.packageName}.fileprovider"
    val uri = FileProvider.getUriForFile(context, authority, destFile)
    return JSONObject().put("uri", uri.toString())
  }

  private fun pruneOpenExternallyCache(dir: File) {
    val files = dir.listFiles()?.filter { it.isFile }?.sortedByDescending { it.lastModified() }
      ?: return
    if (files.size > MAX_OPEN_EXTERNALLY_CACHE_FILES) {
      for (stale in files.drop(MAX_OPEN_EXTERNALLY_CACHE_FILES)) {
        runCatching { stale.delete() }
      }
    }
  }

  private fun openTransferFile(context: Context, params: JSONObject): JSONObject {
    val uriOrPath = params.optString("uriOrPath", "").trim()
    val mime = params.optString("mime", "application/octet-stream")
      .ifBlank { "application/octet-stream" }
    if (uriOrPath.isEmpty()) {
      throw IllegalArgumentException("File URI or path is required.")
    }

    val uri: Uri = if (uriOrPath.startsWith("content://")) {
      Uri.parse(uriOrPath)
    } else {
      val file = File(uriOrPath).canonicalFile
      val allowedCache = File(context.cacheDir, "open-externally").canonicalFile
      val prefix = allowedCache.path.trimEnd(File.separatorChar) + File.separatorChar
      if (!file.path.startsWith(prefix) || !file.isFile) {
        throw SecurityException("Refusing to open file outside the open-externally cache.")
      }
      FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    }

    val viewIntent = Intent(Intent.ACTION_VIEW).apply {
      setDataAndType(uri, mime)
      addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    val chooser = Intent.createChooser(viewIntent, "Open with").apply {
      addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      clipData = ClipData.newRawUri("", uri)
    }

    val act = currentActivity()
    if (act != null) {
      act.runOnUiThread {
        runCatching { act.startActivity(chooser) }
          .onFailure { e -> Log.w(TAG, "Activity chooser launch failed: ${e.message}") }
      }
    } else {
      chooser.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      context.startActivity(chooser)
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
