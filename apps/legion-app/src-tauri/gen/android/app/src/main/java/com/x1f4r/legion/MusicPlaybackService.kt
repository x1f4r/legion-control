package com.x1f4r.legion

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.util.Base64
import android.util.Log
import androidx.annotation.OptIn
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.ForwardingPlayer
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.PlaybackException
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.DefaultLoadControl
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.DefaultMediaNotificationProvider
import androidx.media3.session.MediaSession
import androidx.media3.session.MediaSessionService
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/**
 * Android background music service built on Media3 [ExoPlayer] and [MediaSessionService].
 *
 * Why Media3 instead of WebView `<audio>`:
 * Empirical verification on the Android emulator confirmed that WebView halts track-to-track
 * advances while the screen is off and exposes no system `MediaSession` or media notification.
 * Running `ExoPlayer` inside a `mediaPlayback` `MediaSessionService` keeps music playing across
 * songs with the screen locked, provides full lock-screen and notification controls, handles
 * audio focus (permanent loss, call interruption, transient duck) and headphone unplug
 * (`ACTION_AUDIO_BECOMING_NOISY`), releases wake locks immediately whenever paused, and mints
 * fresh short-lived loopback content URLs through Rust over [AudioBridge] without needing the
 * WebView awake.
 */
@OptIn(UnstableApi::class)
class MusicPlaybackService : MediaSessionService() {
  private lateinit var exoPlayer: ExoPlayer
  private lateinit var forwardingPlayer: QueueForwardingPlayer
  private var mediaSession: MediaSession? = null

  private val mainHandler = Handler(Looper.getMainLooper())
  private val ioPool = Executors.newCachedThreadPool()
  private val queue = MusicQueueState()
  private val loadSeq = AtomicLong(0L)

  private var loaded: Boolean = false
  private var isLoadingTrack: Boolean = false
  private var activeUrl: String? = null
  private var activeTags: JSONObject? = null
  private var activeCoverDataUrl: String? = null
  private var activeCoverBytes: ByteArray? = null
  private var activeError: JSONObject? = null
  private var currentMetadata: MediaMetadata = MediaMetadata.EMPTY
  private var playbackSpeed: Float = 1.0f

  private val debugLock = Any()
  private val debugTicks = ArrayDeque<JSONObject>()
  private val debugEvents = ArrayDeque<JSONObject>()

  private val pausedDismissRunnable = Runnable {
    if (loaded && !isLoadingTrack && !exoPlayer.playWhenReady && activeError == null) {
      recordDebugEvent("paused_auto_dismiss", JSONObject())
      dismissPlayback()
    }
  }

  private val positionTickRunnable = object : Runnable {
    override fun run() {
      if (!loaded) return
      val playing = exoPlayer.isPlaying || exoPlayer.playWhenReady
      val buffering = exoPlayer.playbackState == Player.STATE_BUFFERING
      if (playing || buffering) {
        recordDebugTick()
        pushStateToRust(includeCover = false, includeTracks = false)
        mainHandler.postDelayed(this, POSITION_TICK_MS)
      }
    }
  }

  private inner class QueueForwardingPlayer(player: Player) : ForwardingPlayer(player) {
    private val rawListeners = ArrayList<Player.Listener>()

    override fun addListener(listener: Player.Listener) {
      rawListeners.add(listener)
      super.addListener(listener)
    }

    override fun removeListener(listener: Player.Listener) {
      rawListeners.remove(listener)
      super.removeListener(listener)
    }

    fun notifyMetadataAndCommandsChanged() {
      val commands = availableCommands
      val meta = mediaMetadata
      val snapshot = rawListeners.toList()
      for (listener in snapshot) {
        runCatching {
          listener.onMediaMetadataChanged(meta)
          listener.onAvailableCommandsChanged(commands)
        }
      }
    }

    override fun getAvailableCommands(): Player.Commands {
      val builder = super.getAvailableCommands().buildUpon()
      if (loaded) {
        builder.add(Player.COMMAND_SEEK_TO_PREVIOUS)
        builder.add(Player.COMMAND_SEEK_TO_PREVIOUS_MEDIA_ITEM)
        builder.add(Player.COMMAND_SEEK_TO_DEFAULT_POSITION)
        if (queue.hasNext()) {
          builder.add(Player.COMMAND_SEEK_TO_NEXT)
          builder.add(Player.COMMAND_SEEK_TO_NEXT_MEDIA_ITEM)
        } else {
          builder.remove(Player.COMMAND_SEEK_TO_NEXT)
          builder.remove(Player.COMMAND_SEEK_TO_NEXT_MEDIA_ITEM)
        }
      }
      return builder.build()
    }

    override fun isCommandAvailable(command: Int): Boolean {
      if (!loaded) return super.isCommandAvailable(command)
      return when (command) {
        Player.COMMAND_SEEK_TO_PREVIOUS,
        Player.COMMAND_SEEK_TO_PREVIOUS_MEDIA_ITEM,
        Player.COMMAND_SEEK_TO_DEFAULT_POSITION,
        -> true
        Player.COMMAND_SEEK_TO_NEXT,
        Player.COMMAND_SEEK_TO_NEXT_MEDIA_ITEM,
        -> queue.hasNext()
        else -> super.isCommandAvailable(command)
      }
    }

    override fun hasNextMediaItem(): Boolean = loaded && queue.hasNext()

    override fun hasPreviousMediaItem(): Boolean = loaded

    override fun getMediaMetadata(): MediaMetadata = currentMetadata

    override fun seekToNext() {
      skipNext()
    }

    override fun seekToNextMediaItem() {
      skipNext()
    }

    override fun seekToPrevious() {
      skipPrev()
    }

    override fun seekToPreviousMediaItem() {
      skipPrev()
    }

    override fun stop() {
      dismissPlayback()
    }
  }

  private val playerListener = object : Player.Listener {
    override fun onPlaybackStateChanged(playbackState: Int) {
      recordDebugEvent(
        "playback_state",
        JSONObject()
          .put("state", playbackStateName(playbackState))
          .put("playWhenReady", exoPlayer.playWhenReady)
          .put("positionMs", exoPlayer.currentPosition.coerceAtLeast(0L)),
      )
      if (playbackState == Player.STATE_ENDED && loaded && !isLoadingTrack) {
        handleTrackEnded()
        return
      }
      if (playbackState == Player.STATE_READY) {
        isLoadingTrack = false
      }
      syncTimersAndState(includeCover = false)
    }

    override fun onPlayWhenReadyChanged(playWhenReady: Boolean, reason: Int) {
      recordDebugEvent(
        "play_when_ready",
        JSONObject()
          .put("playWhenReady", playWhenReady)
          .put("reason", playWhenReadyReasonName(reason))
          .put("positionMs", exoPlayer.currentPosition.coerceAtLeast(0L)),
      )
      syncTimersAndState(includeCover = false)
    }

    override fun onIsPlayingChanged(isPlaying: Boolean) {
      if (isPlaying) {
        recordDebugTick()
      }
      syncTimersAndState(includeCover = false)
    }

    override fun onPlaybackSuppressionReasonChanged(playbackSuppressionReason: Int) {
      recordDebugEvent(
        "suppression_reason",
        JSONObject()
          .put("reason", suppressionReasonName(playbackSuppressionReason))
          .put("playWhenReady", exoPlayer.playWhenReady)
          .put("isPlaying", exoPlayer.isPlaying),
      )
      syncTimersAndState(includeCover = false)
    }

    override fun onAvailableCommandsChanged(availableCommands: Player.Commands) {
      if (::forwardingPlayer.isInitialized && loaded) {
        forwardingPlayer.notifyMetadataAndCommandsChanged()
      }
    }

    override fun onMediaMetadataChanged(mediaMetadata: MediaMetadata) {
      if (::forwardingPlayer.isInitialized && loaded) {
        forwardingPlayer.notifyMetadataAndCommandsChanged()
      }
    }

    override fun onVolumeChanged(volume: Float) {
      recordDebugEvent(
        "volume",
        JSONObject().put("volume", volume.toDouble()),
      )
    }

    override fun onPlayerError(error: PlaybackException) {
      val failedUrl = activeUrl.orEmpty()
      val failedMachine = queue.machine
      val failedPath = queue.currentTrack()?.path.orEmpty()
      val seq = loadSeq.get()
      Log.w(TAG, "ExoPlayer error (${error.errorCodeName}) on $failedPath")
      recordDebugEvent(
        "player_error",
        JSONObject()
          .put("errorCodeName", error.errorCodeName)
          .put("path", failedPath),
      )

      // Stop immediately; never loop on a broken stream.
      exoPlayer.playWhenReady = false
      exoPlayer.stop()
      isLoadingTrack = false
      cancelPositionTicker()

      ioPool.execute {
        val inspected = runCatching {
          AudioBridge.callRust(
            "inspectTrackError",
            JSONObject()
              .put("machine", failedMachine)
              .put("path", failedPath)
              .put("url", failedUrl),
            timeoutMs = 6_000L,
          )
        }.getOrNull()

        val code = inspected?.optString("code", "").orEmpty().ifEmpty { "unreachable" }
        val message = inspected?.optString("message", "").orEmpty().ifEmpty {
          "Playback stopped unexpectedly."
        }
        val revoked = inspected?.optBoolean("revoked", false) ?: false

        mainHandler.post {
          if (loadSeq.get() == seq && loaded) {
            applyPlaybackError(code, message, revoked)
          }
        }
      }
    }
  }

  override fun onCreate() {
    super.onCreate()
    ensureChannels(this)

    val notificationProvider = DefaultMediaNotificationProvider.Builder(this)
      .setChannelId(CHANNEL_MUSIC)
      .setChannelName(R.string.app_name)
      .setNotificationId(MEDIA_NOTIFICATION_ID)
      .build()
    setMediaNotificationProvider(notificationProvider)

    val loadControl = DefaultLoadControl.Builder()
      .setBufferDurationsMs(
        /* minBufferMs = */ 10_000,
        /* maxBufferMs = */ 20_000,
        /* bufferForPlaybackMs = */ 1_500,
        /* bufferForPlaybackAfterRebufferMs = */ 3_000,
      )
      .build()

    val audioAttributes = AudioAttributes.Builder()
      .setUsage(C.USAGE_MEDIA)
      .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
      .build()

    exoPlayer = ExoPlayer.Builder(this)
      .setLoadControl(loadControl)
      .setAudioAttributes(audioAttributes, /* handleAudioFocus = */ true)
      .setHandleAudioBecomingNoisy(true)
      .setWakeMode(C.WAKE_MODE_LOCAL)
      .build()
    exoPlayer.addListener(playerListener)

    forwardingPlayer = QueueForwardingPlayer(exoPlayer)

    val openPlayerIntent = Intent(this, MainActivity::class.java).apply {
      addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
      putExtra(MainActivity.EXTRA_OPEN_MUSIC_PLAYER, true)
    }
    val sessionActivity = PendingIntent.getActivity(
      this,
      MEDIA_NOTIFICATION_ID,
      openPlayerIntent,
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )

    val session = MediaSession.Builder(this, forwardingPlayer)
      .setSessionActivity(sessionActivity)
      .build()
    mediaSession = session
    addSession(session)

    synchronized(serviceLock) {
      runningInstance = this
      serviceReadyLatch.countDown()
    }
    Log.i(TAG, "MusicPlaybackService created")
  }

  override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaSession? {
    return mediaSession
  }

  override fun onTaskRemoved(rootIntent: Intent?) {
    // Keep playing in the background when the user swipes Legion away from Recents while playing.
    if (!loaded) {
      stopSelf()
    }
  }

  override fun onDestroy() {
    cancelPositionTicker()
    cancelPausedDismiss()
    val urlToRevoke = activeUrl
    activeUrl = null
    if (!urlToRevoke.isNullOrEmpty()) {
      ioPool.execute {
        runCatching {
          AudioBridge.callRust("revokeUrl", JSONObject().put("url", urlToRevoke), timeoutMs = 3_000L)
        }
      }
    }
    mediaSession?.let { session ->
      runCatching { removeSession(session) }
      runCatching { session.release() }
    }
    mediaSession = null
    if (::exoPlayer.isInitialized) {
      exoPlayer.removeListener(playerListener)
      runCatching { exoPlayer.release() }
    }
    synchronized(serviceLock) {
      if (runningInstance === this) {
        runningInstance = null
        serviceReadyLatch = CountDownLatch(1)
      }
    }
    Log.i(TAG, "MusicPlaybackService destroyed")
    super.onDestroy()
  }

  internal fun handleLoadQueue(params: JSONObject): JSONObject {
    val machine = params.optString("machine", "").trim()
    val tracksArr = params.optJSONArray("tracks") ?: JSONArray()
    val items = ArrayList<MusicQueueState.TrackItem>(tracksArr.length())
    for (i in 0 until tracksArr.length()) {
      val obj = tracksArr.optJSONObject(i) ?: continue
      val path = obj.optString("path", "")
      if (path.isEmpty()) continue
      val name = obj.optString("name", "").ifEmpty { path.substringAfterLast('/') }
      items.add(
        MusicQueueState.TrackItem(
          path = path,
          name = name,
          size = if (obj.has("size") && !obj.isNull("size")) obj.optLong("size") else null,
          mtime = obj.optString("mtime", "").takeIf { it.isNotEmpty() },
          mime = obj.optString("mime", "").takeIf { it.isNotEmpty() },
          durationS = if (obj.has("duration_s") && !obj.isNull("duration_s")) {
            obj.optDouble("duration_s")
          } else {
            null
          },
        ),
      )
    }
    val startIndex = params.optInt("startIndex", 0)
    val shuffle = params.optBoolean("shuffle", false)
    val repeat = MusicQueueState.RepeatMode.fromString(params.optString("repeat", "off"))
    val speed = params.optDouble("playbackRate", 1.0).toFloat().coerceIn(0.5f, 2.0f)
    val autoPlay = params.optBoolean("autoPlay", false)

    runOnMainSync {
      cancelErrorNotification()
      playbackSpeed = speed
      val first = queue.load(machine, items, startIndex, shuffle, repeat)
      if (first == null) {
        dismissPlayback()
      } else {
        loaded = true
        activeError = null
        recordDebugEvent(
          "load_queue",
          JSONObject()
            .put("machine", machine)
            .put("count", items.size)
            .put("startPath", first.path)
            .put("autoPlay", autoPlay),
        )
        loadCurrentTrack(autoPlay = autoPlay)
      }
    }
    return JSONObject().put("ok", true)
  }

  internal fun handleCommand(params: JSONObject): JSONObject {
    val action = params.optString("action", "").trim().lowercase()
    runOnMainSync {
      when (action) {
        "play" -> {
          if (!loaded) return@runOnMainSync
          cancelErrorNotification()
          if (activeError != null) {
            activeError = null
            loadCurrentTrack(autoPlay = true)
          } else if (exoPlayer.playbackState == Player.STATE_ENDED) {
            exoPlayer.seekTo(0L)
            exoPlayer.playWhenReady = true
            exoPlayer.play()
          } else {
            exoPlayer.playWhenReady = true
            exoPlayer.play()
          }
          syncTimersAndState(includeCover = false)
        }
        "pause" -> {
          if (!loaded) return@runOnMainSync
          exoPlayer.playWhenReady = false
          exoPlayer.pause()
          syncTimersAndState(includeCover = false)
        }
        "toggle" -> {
          if (!loaded) return@runOnMainSync
          if (exoPlayer.playWhenReady && exoPlayer.playbackState != Player.STATE_ENDED) {
            exoPlayer.playWhenReady = false
            exoPlayer.pause()
          } else {
            cancelErrorNotification()
            if (activeError != null) {
              activeError = null
              loadCurrentTrack(autoPlay = true)
            } else {
              if (exoPlayer.playbackState == Player.STATE_ENDED) {
                exoPlayer.seekTo(0L)
              }
              exoPlayer.playWhenReady = true
              exoPlayer.play()
            }
          }
          syncTimersAndState(includeCover = false)
        }
        "next" -> skipNext()
        "prev" -> skipPrev()
        "seek" -> {
          if (!loaded) return@runOnMainSync
          val posMs = params.optLong("positionMs", 0L).coerceAtLeast(0L)
          exoPlayer.seekTo(posMs)
          syncTimersAndState(includeCover = false)
        }
        "speed" -> {
          val rate = params.optDouble("playbackRate", 1.0).toFloat().coerceIn(0.5f, 2.0f)
          playbackSpeed = rate
          exoPlayer.playbackParameters = PlaybackParameters(rate)
          syncTimersAndState(includeCover = false)
        }
      }
    }
    return JSONObject().put("ok", true)
  }

  internal fun handleSetModes(params: JSONObject): JSONObject {
    val shuffle = params.optBoolean("shuffle", queue.shuffle)
    val repeat = MusicQueueState.RepeatMode.fromString(params.optString("repeat", queue.repeat.wireName))
    val orderArr = params.optJSONArray("order")
    val newOrder = if (orderArr != null) {
      ArrayList<String>(orderArr.length()).apply {
        for (i in 0 until orderArr.length()) {
          val p = orderArr.optString(i, "")
          if (p.isNotEmpty()) add(p)
        }
      }
    } else {
      null
    }
    runOnMainSync {
      queue.setModes(shuffle, repeat, newOrder)
      forwardingPlayer.notifyMetadataAndCommandsChanged()
      pushStateToRust(includeCover = false, includeTracks = false)
    }
    return JSONObject().put("ok", true)
  }

  internal fun handleStopWithError(params: JSONObject): JSONObject {
    val code = params.optString("code", "denied")
    val message = params.optString("message", "Playback stopped.")
    val revoked = params.optBoolean("revoked", false)
    runOnMainSync {
      if (loaded) {
        applyPlaybackError(code, message, revoked)
      }
    }
    return JSONObject().put("ok", true)
  }

  internal fun handleDismiss(): JSONObject {
    runOnMainSync {
      dismissPlayback()
    }
    return JSONObject().put("ok", true)
  }

  internal fun handleDebugTrace(): JSONObject {
    val ticks = JSONArray()
    val events = JSONArray()
    synchronized(debugLock) {
      for (t in debugTicks) ticks.put(t)
      for (e in debugEvents) events.put(e)
    }
    val snapshot = JSONObject()
    runOnMainSync {
      snapshot.put("loaded", loaded)
      snapshot.put("playing", isEffectivelyPlaying())
      snapshot.put("playWhenReady", loaded && exoPlayer.playWhenReady)
      snapshot.put("suppressionReason", suppressionReasonName(exoPlayer.playbackSuppressionReason))
      snapshot.put("sessionsCount", sessions.size)
      snapshot.put("isPlaybackOngoing", isPlaybackOngoing)
      snapshot.put("positionMs", if (loaded) exoPlayer.currentPosition.coerceAtLeast(0L) else 0L)
      snapshot.put("durationMs", if (loaded) exoPlayer.duration.coerceAtLeast(0L) else 0L)
      snapshot.put("path", queue.currentTrack()?.path ?: JSONObject.NULL)
      snapshot.put("title", currentMetadata.title?.toString() ?: JSONObject.NULL)
      snapshot.put("artist", currentMetadata.artist?.toString() ?: JSONObject.NULL)
      snapshot.put("error", activeError ?: JSONObject.NULL)
    }
    return JSONObject()
      .put("supported", true)
      .put("state", snapshot)
      .put("ticks", ticks)
      .put("events", events)
  }

  private fun skipNext() {
    if (!loaded) return
    val nextIdx = queue.nextIndex() ?: return
    val continuePlaying = exoPlayer.playWhenReady || exoPlayer.isPlaying
    queue.moveTo(nextIdx)
    recordDebugEvent(
      "skip_next",
      JSONObject().put("path", queue.currentTrack()?.path.orEmpty()).put("autoPlay", continuePlaying),
    )
    loadCurrentTrack(autoPlay = continuePlaying)
  }

  private fun skipPrev() {
    if (!loaded) return
    val posMs = exoPlayer.currentPosition.coerceAtLeast(0L)
    when (val decision = queue.resolvePrevAction(posMs)) {
      is MusicQueueState.PrevDecision.Restart -> {
        exoPlayer.seekTo(0L)
        syncTimersAndState(includeCover = false)
      }
      is MusicQueueState.PrevDecision.Previous -> {
        val continuePlaying = exoPlayer.playWhenReady || exoPlayer.isPlaying
        queue.moveTo(decision.index)
        recordDebugEvent(
          "skip_prev",
          JSONObject().put("path", queue.currentTrack()?.path.orEmpty()).put("autoPlay", continuePlaying),
        )
        loadCurrentTrack(autoPlay = continuePlaying)
      }
    }
  }

  private fun handleTrackEnded() {
    val currentPath = queue.currentTrack()?.path.orEmpty()
    when (val decision = queue.resolveEndedAction()) {
      is MusicQueueState.EndedDecision.RepeatCurrent -> {
        recordDebugEvent("ended_repeat", JSONObject().put("path", currentPath))
        // Mint a fresh ticket URL on repeat as well so long-running repeats never outlive a ticket.
        loadCurrentTrack(autoPlay = true)
      }
      is MusicQueueState.EndedDecision.Advance -> {
        queue.moveTo(decision.index)
        recordDebugEvent(
          "ended_advance",
          JSONObject()
            .put("from", currentPath)
            .put("to", queue.currentTrack()?.path.orEmpty()),
        )
        loadCurrentTrack(autoPlay = true)
      }
      is MusicQueueState.EndedDecision.Stop -> {
        recordDebugEvent("ended_stop", JSONObject().put("path", currentPath))
        exoPlayer.playWhenReady = false
        syncTimersAndState(includeCover = false)
      }
    }
  }

  private fun loadCurrentTrack(autoPlay: Boolean) {
    val track = queue.currentTrack() ?: return
    val machine = queue.machine
    val seq = loadSeq.incrementAndGet()
    val prevUrl = activeUrl
    activeUrl = null
    activeTags = null
    activeCoverDataUrl = null
    activeCoverBytes = null
    activeError = null
    isLoadingTrack = true
    cancelPausedDismiss()

    currentMetadata = buildMediaMetadata(track, null, null)
    // Keep the current timeline and foreground service intact while `mintTrack` fetches the next
    // short-lived content URL: calling `stop()`/`clearMediaItems()` here drops Media3 out of
    // foreground, which Android 12+ refuses to restart when advancing tracks with the screen off.
    if (exoPlayer.playbackState != Player.STATE_ENDED && exoPlayer.playbackState != Player.STATE_IDLE) {
      exoPlayer.pause()
    }
    forwardingPlayer.notifyMetadataAndCommandsChanged()
    pushStateToRust(includeCover = false, includeTracks = true)

    ioPool.execute {
      // Hold a short bounded transition wake lock ONLY when advancing/starting active playback so
      // Doze cannot pause the CPU during the ~30 ms mintTrack RPC between tracks while screen is
      // off. Always released in `finally`; never acquired when paused.
      val transitionLock = if (autoPlay) acquireTransitionWakeLock() else null
      try {
        val resp = AudioBridge.callRust(
          "mintTrack",
          JSONObject().apply {
            put("machine", machine)
            put("path", track.path)
            if (!prevUrl.isNullOrEmpty()) put("prevUrl", prevUrl)
            put("fetchMeta", true)
          },
          timeoutMs = 15_000L,
        )
        val url = resp.optString("url", "")
        if (url.isEmpty()) {
          throw AudioBridge.BridgeRpcException("unreachable", "Could not start streaming this track.")
        }
        val tags = resp.optJSONObject("tags")
        val coverB64 = resp.optString("coverBase64", "").takeIf { it.isNotEmpty() }
        val coverMime = resp.optString("coverMime", "").takeIf { it.isNotEmpty() }
        val coverBytes = if (coverB64 != null) {
          runCatching { Base64.decode(coverB64, Base64.DEFAULT) }.getOrNull()
        } else {
          null
        }
        val coverDataUrl = if (coverB64 != null && coverMime != null) {
          "data:$coverMime;base64,$coverB64"
        } else {
          null
        }

        if (loadSeq.get() != seq) {
          runCatching {
            AudioBridge.callRust("revokeUrl", JSONObject().put("url", url), timeoutMs = 3_000L)
          }
          return@execute
        }

        val readyLatch = CountDownLatch(1)
        mainHandler.post {
          try {
            if (loadSeq.get() != seq || !loaded) {
              ioPool.execute {
                runCatching {
                  AudioBridge.callRust("revokeUrl", JSONObject().put("url", url), timeoutMs = 3_000L)
                }
              }
              return@post
            }
            activeUrl = url
            activeTags = tags
            activeCoverBytes = coverBytes
            activeCoverDataUrl = coverDataUrl
            activeError = null
            isLoadingTrack = false
            currentMetadata = buildMediaMetadata(track, tags, coverBytes)

            val mediaItem = MediaItem.Builder()
              .setUri(url)
              .setMediaId(track.path)
              .setMediaMetadata(currentMetadata)
              .build()
            exoPlayer.playbackParameters = PlaybackParameters(playbackSpeed)
            exoPlayer.setMediaItem(mediaItem)
            exoPlayer.playWhenReady = autoPlay
            exoPlayer.prepare()
            forwardingPlayer.notifyMetadataAndCommandsChanged()

            recordDebugEvent(
              "track_prepared",
              JSONObject()
                .put("path", track.path)
                .put("title", currentMetadata.title?.toString().orEmpty())
                .put("artist", currentMetadata.artist?.toString().orEmpty())
                .put("hasCover", coverBytes != null && coverBytes.isNotEmpty())
                .put("autoPlay", autoPlay),
            )
            syncTimersAndState(includeCover = true)
          } finally {
            readyLatch.countDown()
          }
        }
        // Keep the short transition lock until `exoPlayer.prepare()` has handed wake management
        // over to ExoPlayer's own `WakeLockManager`.
        runCatching { readyLatch.await(3L, TimeUnit.SECONDS) }
      } catch (e: AudioBridge.BridgeRpcException) {
        mainHandler.post {
          if (loadSeq.get() == seq && loaded) {
            applyPlaybackError(e.errorCode, e.message, e.revoked)
          }
        }
      } catch (e: Exception) {
        mainHandler.post {
          if (loadSeq.get() == seq && loaded) {
            applyPlaybackError("unreachable", e.message ?: "Could not load this song.", false)
          }
        }
      } finally {
        releaseTransitionWakeLock(transitionLock)
      }
    }
  }

  private fun applyPlaybackError(code: String, message: String, revoked: Boolean) {
    loadSeq.incrementAndGet()
    isLoadingTrack = false
    exoPlayer.playWhenReady = false
    exoPlayer.stop()
    exoPlayer.clearMediaItems()
    val prevUrl = activeUrl
    activeUrl = null
    if (!prevUrl.isNullOrEmpty()) {
      ioPool.execute {
        runCatching {
          AudioBridge.callRust("revokeUrl", JSONObject().put("url", prevUrl), timeoutMs = 3_000L)
        }
      }
    }
    activeError = JSONObject()
      .put("code", code)
      .put("message", message)
      .put("revoked", revoked)
    cancelPositionTicker()
    cancelPausedDismiss()
    recordDebugEvent(
      if (revoked) "revoked_stop" else "playback_error",
      JSONObject()
        .put("code", code)
        .put("message", message)
        .put("revoked", revoked),
    )
    forwardingPlayer.notifyMetadataAndCommandsChanged()
    pushStateToRust(includeCover = false, includeTracks = false)
    if (revoked) {
      postStoppedNotice(message)
    }
    // Detach/stop foreground playback cleanly so zero wake locks remain held after an error.
    stopForeground(STOP_FOREGROUND_REMOVE)
  }

  private fun dismissPlayback() {
    loadSeq.incrementAndGet()
    loaded = false
    isLoadingTrack = false
    activeError = null
    activeTags = null
    activeCoverDataUrl = null
    activeCoverBytes = null
    currentMetadata = MediaMetadata.EMPTY
    queue.clear()
    cancelPositionTicker()
    cancelPausedDismiss()
    cancelErrorNotification()
    exoPlayer.playWhenReady = false
    exoPlayer.stop()
    exoPlayer.clearMediaItems()
    val urlToRevoke = activeUrl
    activeUrl = null
    if (!urlToRevoke.isNullOrEmpty()) {
      ioPool.execute {
        runCatching {
          AudioBridge.callRust("revokeUrl", JSONObject().put("url", urlToRevoke), timeoutMs = 3_000L)
        }
      }
    }
    forwardingPlayer.notifyMetadataAndCommandsChanged()
    pushStateToRust(includeCover = false, includeTracks = false)
    stopForeground(STOP_FOREGROUND_REMOVE)
    stopSelf()
  }

  private fun isEffectivelyPlaying(): Boolean {
    if (!loaded || activeError != null) return false
    if (!exoPlayer.playWhenReady) return false
    if (exoPlayer.playbackState == Player.STATE_ENDED || exoPlayer.playbackState == Player.STATE_IDLE) {
      return false
    }
    return exoPlayer.playbackSuppressionReason == Player.PLAYBACK_SUPPRESSION_REASON_NONE
  }

  private fun syncTimersAndState(includeCover: Boolean) {
    val wantsPlay = loaded && exoPlayer.playWhenReady && exoPlayer.playbackState != Player.STATE_ENDED
    val buffering = loaded && exoPlayer.playbackState == Player.STATE_BUFFERING
    if (wantsPlay || buffering) {
      cancelPausedDismiss()
      startPositionTicker()
    } else {
      cancelPositionTicker()
      if (loaded && !isLoadingTrack && activeError == null) {
        schedulePausedDismiss()
      } else {
        cancelPausedDismiss()
      }
    }
    pushStateToRust(includeCover = includeCover, includeTracks = false)
  }

  private fun startPositionTicker() {
    mainHandler.removeCallbacks(positionTickRunnable)
    mainHandler.postDelayed(positionTickRunnable, POSITION_TICK_MS)
  }

  private fun cancelPositionTicker() {
    mainHandler.removeCallbacks(positionTickRunnable)
  }

  private fun schedulePausedDismiss() {
    mainHandler.removeCallbacks(pausedDismissRunnable)
    mainHandler.postDelayed(pausedDismissRunnable, PAUSED_DISMISS_DELAY_MS)
  }

  private fun cancelPausedDismiss() {
    mainHandler.removeCallbacks(pausedDismissRunnable)
  }

  private fun buildMediaMetadata(
    track: MusicQueueState.TrackItem,
    tags: JSONObject?,
    coverBytes: ByteArray?,
  ): MediaMetadata {
    val tagTitle = tags?.optString("title", "")?.takeIf { it.isNotBlank() }
    val tagArtist = tags?.optString("artist", "")?.takeIf { it.isNotBlank() }
    val tagAlbum = tags?.optString("album", "")?.takeIf { it.isNotBlank() }
    val (displayTitle, displayArtist) = MusicQueueState.formatTitleAndArtist(tagTitle, tagArtist, track.name)

    val builder = MediaMetadata.Builder()
      .setTitle(displayTitle)
      .setDisplayTitle(displayTitle)
    if (displayArtist.isNotEmpty()) {
      builder.setArtist(displayArtist)
      builder.setSubtitle(displayArtist)
    }
    if (!tagAlbum.isNullOrEmpty()) {
      builder.setAlbumTitle(tagAlbum)
    }
    if (coverBytes != null && coverBytes.isNotEmpty()) {
      builder.setArtworkData(coverBytes, MediaMetadata.PICTURE_TYPE_FRONT_COVER)
    }
    return builder.build()
  }

  private fun pushStateToRust(includeCover: Boolean, includeTracks: Boolean) {
    val state = JSONObject()
    if (!loaded) {
      state.put("loaded", false)
    } else {
      val track = queue.currentTrack()
      val durationMs = exoPlayer.duration.let { d ->
        if (d != C.TIME_UNSET && d > 0L) d else ((track?.durationS ?: 0.0) * 1000.0).toLong().coerceAtLeast(0L)
      }
      val positionMs = exoPlayer.currentPosition.coerceAtLeast(0L)
      val bufferedMs = exoPlayer.bufferedPosition.coerceAtLeast(0L)
      val isPlaying = isEffectivelyPlaying()
      val isBuffering = exoPlayer.playbackState == Player.STATE_BUFFERING && !isLoadingTrack

      state.put("loaded", true)
      state.put("machine", queue.machine)
      state.put("path", track?.path ?: JSONObject.NULL)
      state.put("name", track?.name ?: "")
      state.put("shuffle", queue.shuffle)
      state.put("repeat", queue.repeat.wireName)
      state.put("order", JSONArray(queue.currentOrderPaths()))
      state.put("loading", isLoadingTrack)
      state.put("playing", isPlaying)
      state.put("buffering", isBuffering)
      state.put("positionMs", positionMs)
      state.put("durationMs", durationMs)
      state.put("bufferedMs", bufferedMs)
      state.put("playbackRate", playbackSpeed.toDouble())
      if (activeTags != null) {
        state.put("tags", activeTags)
      }
      if (includeCover && activeCoverDataUrl != null) {
        state.put("coverDataUrl", activeCoverDataUrl)
      }
      if (includeTracks) {
        val arr = JSONArray()
        for (item in queue.allTracksInOrder()) {
          arr.put(
            JSONObject()
              .put("path", item.path)
              .put("name", item.name)
              .put("kind", "file"),
          )
        }
        state.put("tracks", arr)
      }
      state.put("error", activeError ?: JSONObject.NULL)
    }

    AudioBridge.sendEvent(
      JSONObject()
        .put("event", "audio_state")
        .put("state", state),
    )
  }

  private fun acquireTransitionWakeLock(): PowerManager.WakeLock? {
    return runCatching {
      val pm = getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return null
      pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, TRANSITION_WAKELOCK_TAG).apply {
        setReferenceCounted(false)
        acquire(TRANSITION_WAKELOCK_TIMEOUT_MS)
      }
    }.getOrNull()
  }

  private fun releaseTransitionWakeLock(lock: PowerManager.WakeLock?) {
    if (lock == null) return
    runCatching {
      if (lock.isHeld) {
        lock.release()
      }
    }
  }

  private fun postStoppedNotice(message: String) {
    ensureChannels(this)
    val nm = getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager ?: return
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      val granted = ContextCompat.checkSelfPermission(
        this,
        Manifest.permission.POST_NOTIFICATIONS,
      ) == PackageManager.PERMISSION_GRANTED
      if (!granted) return
    }
    val openApp = PendingIntent.getActivity(
      this,
      ERROR_NOTIFICATION_ID,
      Intent(this, MainActivity::class.java).apply {
        addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        putExtra(MainActivity.EXTRA_OPEN_MUSIC_PLAYER, true)
      },
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val notification = NotificationCompat.Builder(this, CHANNEL_MUSIC_ALERTS)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("Playback stopped")
      .setContentText(message)
      .setStyle(NotificationCompat.BigTextStyle().bigText(message))
      .setAutoCancel(true)
      .setContentIntent(openApp)
      .build()
    runCatching { nm.notify(ERROR_NOTIFICATION_ID, notification) }
  }

  private fun cancelErrorNotification() {
    val nm = getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager ?: return
    runCatching { nm.cancel(ERROR_NOTIFICATION_ID) }
  }

  private fun recordDebugTick() {
    if (!BuildConfig.DEBUG) return
    val track = queue.currentTrack() ?: return
    val tick = JSONObject()
      .put("wallMs", System.currentTimeMillis())
      .put("elapsedMs", SystemClock.elapsedRealtime())
      .put("trackIndex", queue.currentIndex)
      .put("path", track.path)
      .put("title", currentMetadata.title?.toString().orEmpty())
      .put("artist", currentMetadata.artist?.toString().orEmpty())
      .put("positionMs", exoPlayer.currentPosition.coerceAtLeast(0L))
      .put("durationMs", exoPlayer.duration.coerceAtLeast(0L))
      .put("playing", exoPlayer.isPlaying)
      .put("playWhenReady", exoPlayer.playWhenReady)
      .put("state", playbackStateName(exoPlayer.playbackState))
      .put("volume", exoPlayer.volume.toDouble())
    synchronized(debugLock) {
      if (debugTicks.size >= MAX_DEBUG_TICKS) debugTicks.removeFirst()
      debugTicks.addLast(tick)
    }
    Log.i(
      TICK_TAG,
      "t=${tick.optLong("elapsedMs")} idx=${queue.currentIndex} pos=${tick.optLong("positionMs")}/${tick.optLong("durationMs")} playing=${exoPlayer.isPlaying} title=${tick.optString("title")}",
    )
  }

  private fun recordDebugEvent(kind: String, detail: JSONObject) {
    if (!BuildConfig.DEBUG) return
    val entry = JSONObject()
      .put("wallMs", System.currentTimeMillis())
      .put("elapsedMs", SystemClock.elapsedRealtime())
      .put("kind", kind)
      .put("detail", detail)
    synchronized(debugLock) {
      if (debugEvents.size >= MAX_DEBUG_EVENTS) debugEvents.removeFirst()
      debugEvents.addLast(entry)
    }
    Log.i(TAG, "event=$kind detail=$detail")
  }

  private fun runOnMainSync(block: () -> Unit) {
    if (Looper.myLooper() == Looper.getMainLooper()) {
      block()
      return
    }
    val latch = CountDownLatch(1)
    val err = AtomicReference<Throwable?>(null)
    mainHandler.post {
      try {
        block()
      } catch (t: Throwable) {
        err.set(t)
      } finally {
        latch.countDown()
      }
    }
    runCatching { latch.await(8L, TimeUnit.SECONDS) }
    err.get()?.let { throw it }
  }

  companion object {
    private const val TAG = "legion-audio"
    private const val TICK_TAG = "legion-audio-tick"
    private const val CHANNEL_MUSIC = "legion_music_playback"
    private const val CHANNEL_MUSIC_ALERTS = "legion_music_alerts"
    private const val MEDIA_NOTIFICATION_ID = 4301
    private const val ERROR_NOTIFICATION_ID = 4302
    private const val POSITION_TICK_MS = 1_000L
    private const val PAUSED_DISMISS_DELAY_MS = 10L * 60L * 1000L
    private const val TRANSITION_WAKELOCK_TAG = "com.x1f4r.legion:MusicTrackTransition"
    private const val TRANSITION_WAKELOCK_TIMEOUT_MS = 10_000L
    private const val MAX_DEBUG_TICKS = 600
    private const val MAX_DEBUG_EVENTS = 250

    private val serviceLock = Any()

    @Volatile
    private var runningInstance: MusicPlaybackService? = null

    @Volatile
    private var serviceReadyLatch: CountDownLatch = CountDownLatch(1)

    fun ensureChannels(context: Context) {
      val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
        ?: return
      if (nm.getNotificationChannel(CHANNEL_MUSIC) == null) {
        val ch = NotificationChannel(
          CHANNEL_MUSIC,
          "Music playback",
          NotificationManager.IMPORTANCE_LOW,
        ).apply {
          description = "Playback controls while Legion plays music in the background."
          setShowBadge(false)
        }
        nm.createNotificationChannel(ch)
      }
      if (nm.getNotificationChannel(CHANNEL_MUSIC_ALERTS) == null) {
        val alerts = NotificationChannel(
          CHANNEL_MUSIC_ALERTS,
          "Music alerts",
          NotificationManager.IMPORTANCE_DEFAULT,
        ).apply {
          description = "Alerts when background playback stops because access changed."
        }
        nm.createNotificationChannel(alerts)
      }
    }

    fun handleBridgeCommand(context: Context, method: String, params: JSONObject): JSONObject {
      val existing = runningInstance
      when (method) {
        "dismiss" -> {
          return existing?.handleDismiss() ?: JSONObject().put("ok", true)
        }
        "stopWithError" -> {
          return existing?.handleStopWithError(params) ?: JSONObject().put("ok", true)
        }
        "debugTrace" -> {
          return existing?.handleDebugTrace() ?: JSONObject()
            .put("supported", true)
            .put("state", JSONObject().put("loaded", false))
            .put("ticks", JSONArray())
            .put("events", JSONArray())
        }
      }

      val service = ensureServiceRunning(context)
      return when (method) {
        "loadQueue" -> service.handleLoadQueue(params)
        "command" -> service.handleCommand(params)
        "setModes" -> service.handleSetModes(params)
        else -> throw IllegalArgumentException("Unknown audio command: $method")
      }
    }

    private fun ensureServiceRunning(context: Context): MusicPlaybackService {
      runningInstance?.let { return it }
      val appCtx = context.applicationContext
      ensureChannels(appCtx)
      val latch = synchronized(serviceLock) {
        runningInstance?.let { return it }
        if (serviceReadyLatch.count == 0L) {
          serviceReadyLatch = CountDownLatch(1)
        }
        serviceReadyLatch
      }
      val intent = Intent(appCtx, MusicPlaybackService::class.java)
      try {
        appCtx.startService(intent)
      } catch (_: Exception) {
        ContextCompat.startForegroundService(appCtx, intent)
      }
      latch.await(6L, TimeUnit.SECONDS)
      return runningInstance
        ?: throw IllegalStateException("Android music playback service did not start in time.")
    }

    private fun playbackStateName(state: Int): String = when (state) {
      Player.STATE_IDLE -> "idle"
      Player.STATE_BUFFERING -> "buffering"
      Player.STATE_READY -> "ready"
      Player.STATE_ENDED -> "ended"
      else -> "unknown($state)"
    }

    private fun playWhenReadyReasonName(reason: Int): String = when (reason) {
      Player.PLAY_WHEN_READY_CHANGE_REASON_USER_REQUEST -> "user_request"
      Player.PLAY_WHEN_READY_CHANGE_REASON_AUDIO_FOCUS_LOSS -> "audio_focus_loss"
      Player.PLAY_WHEN_READY_CHANGE_REASON_AUDIO_BECOMING_NOISY -> "audio_becoming_noisy"
      Player.PLAY_WHEN_READY_CHANGE_REASON_REMOTE -> "remote"
      Player.PLAY_WHEN_READY_CHANGE_REASON_END_OF_MEDIA_ITEM -> "end_of_media_item"
      else -> "other($reason)"
    }

    private fun suppressionReasonName(reason: Int): String = when (reason) {
      Player.PLAYBACK_SUPPRESSION_REASON_NONE -> "none"
      Player.PLAYBACK_SUPPRESSION_REASON_TRANSIENT_AUDIO_FOCUS_LOSS -> "transient_audio_focus_loss"
      Player.PLAYBACK_SUPPRESSION_REASON_UNSUITABLE_AUDIO_OUTPUT -> "unsuitable_audio_output"
      else -> "other($reason)"
    }
  }
}
