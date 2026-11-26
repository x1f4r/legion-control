package com.x1f4r.legion

import android.content.Context
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.util.Log
import org.json.JSONObject
import java.io.BufferedReader
import java.io.BufferedWriter
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.lang.ref.WeakReference
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * Process-scoped Android bridge between Rust's [AudioManager] and [MusicPlaybackService].
 *
 * Uses a private owner-only (`0o600`, 256-bit random token) Unix domain socket at
 * `<dataDir>/audio/bridge.sock` so [MusicPlaybackService] can mint fresh short-lived loopback
 * content URLs, revoke old tickets, fetch tags/covers on track transitions, and react to device
 * revocation even when the screen is off or `MainActivity` was swiped away.
 */
object AudioBridge {
  private const val TAG = "legion-audio"

  class BridgeRpcException(
    val errorCode: String,
    override val message: String,
    val revoked: Boolean = false,
  ) : IllegalStateException(message)

  @Volatile
  private var appContext: Context? = null

  @Volatile
  private var activeActivityRef: WeakReference<MainActivity>? = null

  @Volatile
  private var writer: BufferedWriter? = null

  @Volatile
  private var activeSocket: LocalSocket? = null

  private val writerLock = Any()
  private val workerPool = Executors.newCachedThreadPool()
  private val pendingRustCalls = ConcurrentHashMap<Long, CompletableFuture<JSONObject>>()
  private val nextReqId = AtomicLong(1L)

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
    workerPool.execute {
      connectLoop(ctx, socketPath, token)
    }
  }

  fun sendEvent(payload: JSONObject) {
    sendRaw(payload)
  }

  fun callRust(method: String, params: JSONObject, timeoutMs: Long = 15_000L): JSONObject {
    val reqId = nextReqId.getAndIncrement()
    val future = CompletableFuture<JSONObject>()
    pendingRustCalls[reqId] = future

    val envelope = JSONObject().apply {
      put("reqId", reqId)
      put("method", method)
      put("params", params)
    }
    if (!sendRaw(envelope)) {
      pendingRustCalls.remove(reqId)
      throw BridgeRpcException("unreachable", "Audio bridge to Legion is not connected.")
    }

    val reply = try {
      future.get(timeoutMs, TimeUnit.MILLISECONDS)
    } catch (e: Exception) {
      pendingRustCalls.remove(reqId)
      throw BridgeRpcException("timeout", "Timed out waiting for Legion ($method).")
    }

    val err = reply.optString("error", "")
    if (err.isNotEmpty()) {
      val code = reply.optString("errorCode", "unreachable")
      val revoked = reply.optBoolean("revoked", false)
      throw BridgeRpcException(code, err, revoked)
    }
    return reply.optJSONObject("result") ?: JSONObject()
  }

  private fun connectLoop(context: Context, socketPath: String, token: String) {
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
            Log.e(TAG, "Failed to connect to Rust audio bridge socket: ${e.message}")
          } else {
            Log.e(TAG, "Failed to connect to Rust audio bridge socket")
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

    sendRaw(JSONObject().apply {
      put("type", "hello")
      put("token", token)
    })
    Log.i(TAG, "Connected to Rust audio bridge")

    try {
      while (true) {
        val line = inReader.readLine() ?: break
        if (line.isBlank()) continue
        val msg = runCatching { JSONObject(line) }.getOrNull() ?: continue
        if (msg.has("respId")) {
          val respId = msg.optLong("respId", -1L)
          pendingRustCalls.remove(respId)?.complete(msg)
          continue
        }
        val id = msg.optLong("id", -1L)
        val method = msg.optString("method", "")
        val params = msg.optJSONObject("params") ?: JSONObject()
        if (id < 0L || method.isEmpty()) continue
        workerPool.execute {
          handleRustRequest(context, id, method, params)
        }
      }
    } catch (e: Exception) {
      if (BuildConfig.DEBUG) {
        Log.i(TAG, "Rust audio bridge reader closed: ${e.message}")
      } else {
        Log.i(TAG, "Rust audio bridge reader closed")
      }
    } finally {
      failAllPendingRustCalls()
      synchronized(writerLock) {
        if (activeSocket === liveSocket) {
          activeSocket = null
          writer = null
        }
      }
      runCatching { liveSocket.close() }
    }
  }

  private fun sendRaw(obj: JSONObject): Boolean {
    val line = obj.toString() + "\n"
    synchronized(writerLock) {
      val w = writer ?: return false
      return try {
        w.write(line)
        w.flush()
        true
      } catch (e: Exception) {
        if (BuildConfig.DEBUG) {
          Log.w(TAG, "Failed writing to Rust audio bridge: ${e.message}")
        } else {
          Log.w(TAG, "Failed writing to Rust audio bridge")
        }
        false
      }
    }
  }

  private fun handleRustRequest(context: Context, id: Long, method: String, params: JSONObject) {
    try {
      val result = MusicPlaybackService.handleBridgeCommand(context, method, params)
      sendRaw(JSONObject().apply {
        put("id", id)
        put("result", result)
      })
    } catch (e: BridgeRpcException) {
      sendRaw(JSONObject().apply {
        put("id", id)
        put("error", e.message)
        put("errorCode", e.errorCode)
      })
    } catch (e: Exception) {
      if (BuildConfig.DEBUG) {
        Log.w(TAG, "Audio bridge method $method failed: ${e.message}")
      } else {
        Log.w(TAG, "Audio bridge method $method failed (${e.javaClass.simpleName})")
      }
      sendRaw(JSONObject().apply {
        put("id", id)
        put("error", e.message ?: "Android audio playback command failed.")
      })
    }
  }

  private fun failAllPendingRustCalls() {
    val err = JSONObject().apply {
      put("error", "Audio bridge disconnected.")
      put("errorCode", "unreachable")
    }
    val keys = pendingRustCalls.keys().toList()
    for (k in keys) {
      pendingRustCalls.remove(k)?.complete(err)
    }
  }
}
