package com.x1f4r.legion

import android.app.Activity
import android.app.PendingIntent
import android.app.PictureInPictureParams
import android.app.RemoteAction
import android.Manifest
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Color
import android.graphics.Rect
import android.graphics.drawable.Icon
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Rational
import android.view.WindowManager
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.Lifecycle
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import app.tauri.plugin.Plugin
import app.tauri.plugin.PluginManager
import org.json.JSONArray
import org.json.JSONObject
import java.lang.ref.WeakReference

class MainActivity : TauriActivity() {
  private var attachedWebView: WebView? = null
  @Volatile
  private var bridgeReplyProxy: JavaScriptReplyProxy? = null
  private lateinit var localNetworkPermissionLauncher: ActivityResultLauncher<String>
  private lateinit var notificationPermissionLauncher: ActivityResultLauncher<String>
  private val pendingLocalNetworkRequests = mutableListOf<Int>()
  private val pendingNotificationCallbacks = mutableListOf<(Boolean) -> Unit>()
  private val immersiveState = ImmersiveState()
  private val pipState = PipState()
  private val mainHandler = Handler(Looper.getMainLooper())
  private var pipReceiverRegistered = false
  private var webViewKeptResumedForPip = false
  private var pendingPipDismissCleanup = false
  private var pendingPipDismissPauseRunnable: Runnable? = null
  @Volatile
  private var pendingOpenMusicPlayer = false

  private val pipControlReceiver = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
      if (intent.action != ACTION_PIP_CONTROL) return
      val action = intent.getStringExtra(EXTRA_PIP_ACTION)?.trim().orEmpty()
      if (action.isEmpty()) return
      val proxy = bridgeReplyProxy ?: return
      val payload = JSONObject().apply {
        put("type", "pip_action")
        put("action", action)
      }.toString()
      runOnUiThread {
        runCatching { proxy.postMessage(payload) }
      }
    }
  }

  private val backCallback = object : OnBackPressedCallback(false) {
    override fun handleOnBackPressed() {
      val webView = attachedWebView
      if (webView != null && webView.canGoBack()) {
        webView.goBack()
        webView.post { syncBackNavigation(webView) }
        webView.postDelayed({ syncBackNavigation(webView) }, 80L)
      } else {
        isEnabled = false
        onBackPressedDispatcher.onBackPressed()
      }
    }
  }

  @Volatile
  private var latestInsets: InsetValues = InsetValues(0f, 0f, 0f, 0f, 0f)

  private data class InsetValues(
    val top: Float,
    val right: Float,
    val bottom: Float,
    val left: Float,
    val imeBottom: Float,
  ) {
    fun toJsonArray(): JSONArray = JSONArray().apply {
      put(top.toDouble())
      put(right.toDouble())
      put(bottom.toDouble())
      put(left.toDouble())
      put(imeBottom.toDouble())
    }
  }

  private fun hasLocalNetworkPermission(): Boolean {
    if (Build.VERSION.SDK_INT < 37) return true
    return ContextCompat.checkSelfPermission(
      this@MainActivity,
      Manifest.permission.ACCESS_LOCAL_NETWORK,
    ) == PackageManager.PERMISSION_GRANTED
  }

  private fun supportsPictureInPicture(): Boolean {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return false
    return runCatching {
      packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)
    }.getOrDefault(false)
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private fun buildPipActions(playing: Boolean): List<RemoteAction> {
    fun makeAction(requestCode: Int, iconRes: Int, title: String, actionName: String): RemoteAction {
      val intent = Intent(ACTION_PIP_CONTROL)
        .setPackage(packageName)
        .putExtra(EXTRA_PIP_ACTION, actionName)
      val pending = PendingIntent.getBroadcast(
        this,
        requestCode,
        intent,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
      )
      return RemoteAction(Icon.createWithResource(this, iconRes), title, title, pending)
    }

    val playPauseIcon = if (playing) R.drawable.ic_pip_pause else R.drawable.ic_pip_play
    val playPauseTitle = if (playing) "Pause" else "Play"
    return listOf(
      makeAction(PIP_REQ_SEEK_BACK, R.drawable.ic_pip_replay_10, "Back 10 seconds", PipState.ACTION_SEEK_BACK_10),
      makeAction(PIP_REQ_TOGGLE_PLAY, playPauseIcon, playPauseTitle, PipState.ACTION_TOGGLE_PLAY),
      makeAction(PIP_REQ_SEEK_FWD, R.drawable.ic_pip_forward_10, "Forward 10 seconds", PipState.ACTION_SEEK_FWD_10),
    )
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private fun buildPipParams(): PictureInPictureParams {
    val ratio = pipState.aspectRatio()
    val builder = PictureInPictureParams.Builder()
      .setAspectRatio(Rational(ratio.numerator, ratio.denominator))
      .setActions(buildPipActions(pipState.playing))
    pipState.sourceRect?.let { rect ->
      builder.setSourceRectHint(Rect(rect.left, rect.top, rect.right, rect.bottom))
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      builder.setAutoEnterEnabled(pipState.shouldAutoEnter())
      builder.setSeamlessResizeEnabled(true)
    }
    return builder.build()
  }

  private fun applyPipParams() {
    if (!supportsPictureInPicture()) return
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      runCatching { setPictureInPictureParams(buildPipParams()) }
    }
  }

  private fun parsePipSourceRect(rectObj: JSONObject?): PipState.SourceRect? {
    if (rectObj == null) return null
    val webView = attachedWebView ?: return null
    val loc = IntArray(2)
    webView.getLocationOnScreen(loc)
    val metrics = resources.displayMetrics
    return PipState.computeSourceRect(
      webViewLeft = loc[0],
      webViewTop = loc[1],
      density = metrics.density,
      cssLeft = rectObj.optDouble("left", Double.NaN),
      cssTop = rectObj.optDouble("top", Double.NaN),
      cssWidth = rectObj.optDouble("width", Double.NaN),
      cssHeight = rectObj.optDouble("height", Double.NaN),
      screenWidth = metrics.widthPixels,
      screenHeight = metrics.heightPixels,
    )
  }

  private fun cancelPendingPipDismissPause() {
    pendingPipDismissPauseRunnable?.let { mainHandler.removeCallbacks(it) }
    pendingPipDismissPauseRunnable = null
    pendingPipDismissCleanup = false
  }

  private fun schedulePipDismissPauseFallback() {
    cancelPendingPipDismissPause()
    pendingPipDismissCleanup = true
    val runnable = Runnable {
      pendingPipDismissPauseRunnable = null
      pendingPipDismissCleanup = false
      if (!lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED) && !isInPictureInPictureMode) {
        webViewKeptResumedForPip = false
        attachedWebView?.onPause()
      }
    }
    pendingPipDismissPauseRunnable = runnable
    mainHandler.postDelayed(runnable, PIP_DISMISS_CLEANUP_MS)
  }

  private fun pushPipChangedToWebView(active: Boolean, dismissed: Boolean) {
    val proxy = bridgeReplyProxy ?: return
    val payload = JSONObject().apply {
      put("type", "pip_changed")
      put("active", active)
      put("dismissed", dismissed)
    }.toString()
    runOnUiThread {
      runCatching { proxy.postMessage(payload) }
    }
  }

  private fun handleBridgeMessage(raw: String, replyProxy: JavaScriptReplyProxy) {
    val msg = runCatching { JSONObject(raw) }.getOrNull() ?: return
    when (msg.optString("type")) {
      "init" -> {
        bridgeReplyProxy = replyProxy
        runOnUiThread {
          resetPlayerWindowState()
        }
        val payload = JSONObject().apply {
          put("type", "init")
          put("insets", latestInsets.toJsonArray())
          put("hasLocalNetwork", hasLocalNetworkPermission())
          put("hasPip", supportsPictureInPicture())
          put("model", Build.MODEL?.trim().orEmpty())
        }
        runCatching { replyProxy.postMessage(payload.toString()) }
        if (pendingOpenMusicPlayer) {
          pendingOpenMusicPlayer = false
          val openMsg = JSONObject().put("type", "open_music_player").toString()
          runCatching { replyProxy.postMessage(openMsg) }
        }
      }
      "request_local_network" -> {
        bridgeReplyProxy = replyProxy
        val requestId = msg.optInt("id", -1)
        if (requestId < 0) return
        if (hasLocalNetworkPermission()) {
          resolveLocalNetworkRequests(listOf(requestId), true)
          return
        }
        val shouldLaunch = pendingLocalNetworkRequests.isEmpty()
        pendingLocalNetworkRequests.add(requestId)
        if (shouldLaunch) {
          localNetworkPermissionLauncher.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
        }
      }
      "history_changed" -> {
        runOnUiThread {
          attachedWebView?.let { syncBackNavigation(it) }
        }
      }
      "immersive" -> {
        bridgeReplyProxy = replyProxy
        val requestId = msg.optInt("id", -1)
        val on = msg.optBoolean("on", false)
        val orientation = msg.optString("orientation", ImmersiveState.ORIENTATION_NONE)
        runOnUiThread {
          applyImmersive(on, orientation)
          if (requestId >= 0) {
            val payload = JSONObject().apply {
              put("type", "immersive_ack")
              put("id", requestId)
              put("active", immersiveState.active)
            }
            runCatching { replyProxy.postMessage(payload.toString()) }
          }
        }
      }
      "keep_screen_on" -> {
        val requestId = msg.optInt("id", -1)
        val on = msg.optBoolean("on", false)
        runOnUiThread {
          val active = applyKeepScreenOn(on)
          if (requestId >= 0) {
            val payload = JSONObject().apply {
              put("type", "keep_screen_on_ack")
              put("id", requestId)
              put("active", active)
            }
            runCatching { replyProxy.postMessage(payload.toString()) }
          }
        }
      }
      "haptic" -> {
        val constant = ImmersiveState.hapticFeedbackConstant(msg.optString("kind", "")) ?: return
        runOnUiThread {
          val target = attachedWebView ?: window.decorView
          target.performHapticFeedback(constant)
        }
      }
      "pip_config" -> {
        bridgeReplyProxy = replyProxy
        val active = msg.optBoolean("active", false)
        val playing = msg.optBoolean("playing", false)
        val width = msg.optInt("width", 0)
        val height = msg.optInt("height", 0)
        val rectObj = msg.optJSONObject("rect")
        runOnUiThread {
          pipState.updateConfig(
            activeVideo = active,
            playing = playing,
            videoWidth = width,
            videoHeight = height,
            sourceRect = parsePipSourceRect(rectObj),
          )
          applyPipParams()
        }
      }
      "enter_pip" -> {
        bridgeReplyProxy = replyProxy
        val requestId = msg.optInt("id", -1)
        runOnUiThread {
          val entered = if (
            supportsPictureInPicture() &&
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
            pipState.canEnterExplicitly() &&
            !isInPictureInPictureMode
          ) {
            runCatching { enterPictureInPictureMode(buildPipParams()) }.getOrDefault(false)
          } else {
            isInPictureInPictureMode
          }
          if (requestId >= 0) {
            val payload = JSONObject().apply {
              put("type", "pip_enter_ack")
              put("id", requestId)
              put("active", entered)
            }
            runCatching { replyProxy.postMessage(payload.toString()) }
          }
        }
      }
      "pip_dismiss_ack" -> {
        runOnUiThread {
          cancelPendingPipDismissPause()
          if (!lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED) && !isInPictureInPictureMode) {
            webViewKeptResumedForPip = false
            attachedWebView?.onPause()
          }
        }
      }
    }
  }

  private fun applyImmersive(on: Boolean, orientation: String?) {
    val transition = if (on) {
      immersiveState.enter(orientation, window.attributes.layoutInDisplayCutoutMode)
    } else {
      immersiveState.leave()
    }
    applyImmersiveTransition(transition)
  }

  private fun applyImmersiveTransition(transition: ImmersiveState.Transition) {
    if (transition.cutoutMode != null) {
      val attrs = window.attributes
      if (attrs.layoutInDisplayCutoutMode != transition.cutoutMode) {
        attrs.layoutInDisplayCutoutMode = transition.cutoutMode
        window.attributes = attrs
      }
    }
    val controller = WindowInsetsControllerCompat(window, window.decorView)
    if (transition.hideSystemBars) {
      controller.systemBarsBehavior =
        WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
      controller.hide(WindowInsetsCompat.Type.systemBars())
      window.decorView.setBackgroundColor(Color.BLACK)
      attachedWebView?.setBackgroundColor(Color.BLACK)
    } else {
      controller.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_DEFAULT
      controller.show(WindowInsetsCompat.Type.systemBars())
      applySystemBarStyle(resources.configuration)
    }
    if (requestedOrientation != transition.requestedOrientation) {
      requestedOrientation = transition.requestedOrientation
    }
    ViewCompat.requestApplyInsets(window.decorView)
  }

  private fun reapplyImmersiveBarsIfActive() {
    if (!immersiveState.active) return
    val attrs = window.attributes
    if (attrs.layoutInDisplayCutoutMode != WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS) {
      attrs.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
      window.attributes = attrs
    }
    val controller = WindowInsetsControllerCompat(window, window.decorView)
    controller.systemBarsBehavior =
      WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
    controller.hide(WindowInsetsCompat.Type.systemBars())
    window.decorView.setBackgroundColor(Color.BLACK)
    attachedWebView?.setBackgroundColor(Color.BLACK)
  }

  private fun applyKeepScreenOn(on: Boolean): Boolean {
    val active = immersiveState.setKeepScreenOn(on)
    if (active) {
      window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    } else {
      window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }
    return active
  }

  private fun resetPlayerWindowState() {
    val transition = immersiveState.resetAll()
    pipState.resetAll()
    cancelPendingPipDismissPause()
    webViewKeptResumedForPip = false
    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    applyImmersiveTransition(transition)
    applyPipParams()
  }

  fun requestNotificationPermission(callback: (Boolean) -> Unit) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
      callback(true)
      return
    }
    if (ContextCompat.checkSelfPermission(
        this,
        Manifest.permission.POST_NOTIFICATIONS,
      ) == PackageManager.PERMISSION_GRANTED
    ) {
      callback(true)
      return
    }
    runOnUiThread {
      val shouldLaunch = pendingNotificationCallbacks.isEmpty()
      pendingNotificationCallbacks.add(callback)
      if (shouldLaunch) {
        notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
      }
    }
  }

  private fun resolveLocalNetworkRequests(requestIds: List<Int>, granted: Boolean) {
    val proxy = bridgeReplyProxy ?: return
    runOnUiThread {
      for (requestId in requestIds) {
        val payload = JSONObject().apply {
          put("type", "local_network")
          put("id", requestId)
          put("granted", granted)
        }
        runCatching { proxy.postMessage(payload.toString()) }
      }
    }
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    localNetworkPermissionLauncher =
      registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        val ids = pendingLocalNetworkRequests.toList()
        pendingLocalNetworkRequests.clear()
        resolveLocalNetworkRequests(ids, granted)
      }
    notificationPermissionLauncher =
      registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        val cbs = pendingNotificationCallbacks.toList()
        pendingNotificationCallbacks.clear()
        for (cb in cbs) {
          runCatching { cb(granted) }
        }
      }
    TransferBridge.setActiveActivity(this)
    AudioBridge.setActiveActivity(this)
    if (!pipReceiverRegistered) {
      ContextCompat.registerReceiver(
        this,
        pipControlReceiver,
        IntentFilter(ACTION_PIP_CONTROL),
        ContextCompat.RECEIVER_NOT_EXPORTED,
      )
      pipReceiverRegistered = true
    }
    if (intent?.getBooleanExtra(EXTRA_OPEN_MUSIC_PLAYER, false) == true) {
      pendingOpenMusicPlayer = true
    }
    requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    applySystemBarStyle(resources.configuration)
    super.onCreate(savedInstanceState)
    // If the process stayed alive for TransferForegroundService or MusicPlaybackService after
    // MainActivity was swiped away from Recents, notify Rust so it rebuilds the main WebView.
    TransferBridge.sendEvent(JSONObject().put("event", "activity_created"))
    AudioBridge.sendEvent(JSONObject().put("event", "activity_created"))

    // Observe system bar + display cutout insets on the decor view without consuming them so the
    // WebView still receives raw WindowInsets and we can feed CSS variables whenever they change.
    ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { view, windowInsets ->
      val bars = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      val ime = windowInsets.getInsets(WindowInsetsCompat.Type.ime())
      val density = view.resources.displayMetrics.density.coerceAtLeast(1f)
      latestInsets = InsetValues(
        top = bars.top / density,
        right = bars.right / density,
        bottom = bars.bottom / density,
        left = bars.left / density,
        imeBottom = ime.bottom / density,
      )
      pushInsetsToWebView()
      windowInsets
    }
    ViewCompat.requestApplyInsets(window.decorView)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    attachedWebView = webView
    bridgeReplyProxy = null
    rebindPluginsIfActivityRecreated(webView)
    resetPlayerWindowState()

    // Android 17 WebView reports 0px for CSS env(safe-area-inset-*). Expose insets, device model,
    // and the local-network runtime permission prompt over an origin-restricted WebMessageListener
    // and document-start script scoped strictly to the bundled app origin.
    if (WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
      WebViewCompat.addWebMessageListener(
        webView,
        BRIDGE_NAME,
        ALLOWED_APP_ORIGINS,
      ) { _, message, sourceOrigin, isMainFrame, replyProxy ->
        if (!isMainFrame || !ALLOWED_APP_ORIGINS.contains(sourceOrigin.toString())) {
          return@addWebMessageListener
        }
        handleBridgeMessage(message.data.orEmpty(), replyProxy)
      }
    }
    if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
      WebViewCompat.addDocumentStartJavaScript(
        webView,
        DOCUMENT_START_BRIDGE_JS,
        ALLOWED_APP_ORIGINS,
      )
    }
    syncBackNavigation(webView)
    ViewCompat.requestApplyInsets(window.decorView)
    pushInsetsToWebView()
  }

  override fun onWebViewHistoryChanged(webView: WebView) {
    super.onWebViewHistoryChanged(webView)
    syncBackNavigation(webView)
  }

  private fun syncBackNavigation(webView: WebView) {
    val canGoBack = webView.canGoBack()
    backCallback.remove()
    if (canGoBack) {
      backCallback.isEnabled = true
      onBackPressedDispatcher.addCallback(this, backCallback)
    } else {
      backCallback.isEnabled = false
    }
  }

  private fun rebindPluginsIfActivityRecreated(webView: WebView) {
    // When TransferForegroundService keeps the process alive after MainActivity is swiped away,
    // PluginManager still holds plugin instances bound to the destroyed Activity and WebView
    // (because PluginManager.onWebViewCreated skips plugins whose `loaded` flag is already true).
    // Re-registering fresh instances against this live Activity + WebView keeps the file picker,
    // barcode scanner, and self-updater working after window recreation.
    val previous = lastPluginActivityRef?.get()
    if (pluginsInitializedOnce && previous !== this) {
      PluginManager.activity = this
      runCatching {
        val cls = Class.forName("app.tauri.barcodescanner.BarcodeScannerPlugin")
        val scanner = cls.getConstructor(Activity::class.java).newInstance(this) as Plugin
        PluginManager.load(webView, "barcode-scanner", scanner, "{}")
      }
      PluginManager.load(webView, "app-updater", AppUpdaterPlugin(this), "{}")
      PluginManager.load(webView, "files-transfer", FilesTransferPlugin(this), "{}")
      PluginManager.load(webView, "audio-player", AudioPlugin(this), "{}")
    }
    pluginsInitializedOnce = true
    lastPluginActivityRef = WeakReference(this)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    if (intent.getBooleanExtra(EXTRA_OPEN_MUSIC_PLAYER, false)) {
      val proxy = bridgeReplyProxy
      if (proxy != null) {
        val openMsg = JSONObject().put("type", "open_music_player").toString()
        runOnUiThread {
          runCatching { proxy.postMessage(openMsg) }
        }
      } else {
        pendingOpenMusicPlayer = true
      }
    }
  }

  override fun onUserLeaveHint() {
    super.onUserLeaveHint()
    if (
      Build.VERSION.SDK_INT in Build.VERSION_CODES.O until Build.VERSION_CODES.S &&
      supportsPictureInPicture() &&
      pipState.shouldAutoEnter() &&
      !isInPictureInPictureMode
    ) {
      runCatching { enterPictureInPictureMode(buildPipParams()) }
    }
  }

  override fun onPause() {
    super.onPause()
    if (isInPictureInPictureMode || pipState.inPip || (supportsPictureInPicture() && pipState.shouldAutoEnter())) {
      // WryActivity.onPause() calls WebView.onPause(), which would freeze video playback inside
      // Picture-in-Picture mode. Keep the WebView resumed while entering or running in PiP;
      // onStop() pauses it cleanly if the activity leaves without entering PiP (e.g. power off).
      attachedWebView?.onResume()
      webViewKeptResumedForPip = true
    }
  }

  override fun onStop() {
    super.onStop()
    if (!isInPictureInPictureMode && !pipState.inPip && !pendingPipDismissCleanup && webViewKeptResumedForPip) {
      webViewKeptResumedForPip = false
      attachedWebView?.onPause()
    }
  }

  override fun onPictureInPictureModeChanged(
    isInPictureInPictureMode: Boolean,
    newConfig: Configuration,
  ) {
    super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
    if (isInPictureInPictureMode) {
      pipState.onPipEntered()
      cancelPendingPipDismissPause()
      attachedWebView?.onResume()
      webViewKeptResumedForPip = true
      window.decorView.setBackgroundColor(Color.BLACK)
      attachedWebView?.setBackgroundColor(Color.BLACK)
      pushPipChangedToWebView(active = true, dismissed = false)
      applyPipParams()
    } else {
      val exitKind = pipState.onPipExited(lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED))
      val dismissed = exitKind == PipState.ExitKind.DISMISSED
      if (dismissed) {
        applyKeepScreenOn(false)
        applyPipParams()
        schedulePipDismissPauseFallback()
      } else {
        cancelPendingPipDismissPause()
        webViewKeptResumedForPip = false
        if (immersiveState.active) {
          reapplyImmersiveBarsIfActive()
        } else {
          applySystemBarStyle(newConfig)
        }
        ViewCompat.requestApplyInsets(window.decorView)
      }
      pushPipChangedToWebView(active = false, dismissed = dismissed)
    }
  }

  override fun onResume() {
    super.onResume()
    cancelPendingPipDismissPause()
    webViewKeptResumedForPip = false
    PluginManager.activity = this
    TransferBridge.setActiveActivity(this)
    AudioBridge.setActiveActivity(this)
    attachedWebView?.let { syncBackNavigation(it) }
    reapplyImmersiveBarsIfActive()
    if (pipState.activeVideo) {
      applyPipParams()
    }
  }

  override fun onDestroy() {
    cancelPendingPipDismissPause()
    if (pipReceiverRegistered) {
      runCatching { unregisterReceiver(pipControlReceiver) }
      pipReceiverRegistered = false
    }
    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    immersiveState.resetAll()
    pipState.resetAll()
    if (TransferBridge.currentActivity() === this) {
      TransferBridge.setActiveActivity(null)
    }
    if (AudioBridge.currentActivity() === this) {
      AudioBridge.setActiveActivity(null)
    }
    super.onDestroy()
  }

  override fun onWindowFocusChanged(hasFocus: Boolean) {
    super.onWindowFocusChanged(hasFocus)
    if (hasFocus) {
      reapplyImmersiveBarsIfActive()
      ViewCompat.requestApplyInsets(window.decorView)
      pushInsetsToWebView()
    }
  }

  override fun onConfigurationChanged(newConfig: Configuration) {
    super.onConfigurationChanged(newConfig)
    if (isInPictureInPictureMode || pipState.inPip) {
      window.decorView.setBackgroundColor(Color.BLACK)
      attachedWebView?.setBackgroundColor(Color.BLACK)
    } else if (immersiveState.active) {
      reapplyImmersiveBarsIfActive()
    } else {
      applySystemBarStyle(newConfig)
    }
    ViewCompat.requestApplyInsets(window.decorView)
  }

  private fun applySystemBarStyle(config: Configuration) {
    if (isInPictureInPictureMode || pipState.inPip) {
      window.decorView.setBackgroundColor(Color.BLACK)
      attachedWebView?.setBackgroundColor(Color.BLACK)
      return
    }
    if (immersiveState.active) {
      reapplyImmersiveBarsIfActive()
      return
    }
    val isDark = (config.uiMode and Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES
    val barStyle = if (isDark) {
      SystemBarStyle.dark(Color.TRANSPARENT)
    } else {
      SystemBarStyle.light(Color.TRANSPARENT, Color.TRANSPARENT)
    }
    enableEdgeToEdge(statusBarStyle = barStyle, navigationBarStyle = barStyle)

    val controller = WindowInsetsControllerCompat(window, window.decorView)
    controller.isAppearanceLightStatusBars = !isDark
    controller.isAppearanceLightNavigationBars = !isDark

    val bg = if (isDark) 0xFF0D0E10.toInt() else 0xFFF3F2EE.toInt()
    window.decorView.setBackgroundColor(bg)
    attachedWebView?.setBackgroundColor(bg)
  }

  private fun pushInsetsToWebView() {
    val proxy = bridgeReplyProxy ?: return
    val payload = JSONObject().apply {
      put("type", "insets")
      put("insets", latestInsets.toJsonArray())
    }.toString()
    runOnUiThread {
      runCatching { proxy.postMessage(payload) }
    }
  }

  companion object {
    const val EXTRA_OPEN_MUSIC_PLAYER = "com.x1f4r.legion.OPEN_MUSIC_PLAYER"
    private const val ACTION_PIP_CONTROL = "com.x1f4r.legion.PIP_CONTROL"
    private const val EXTRA_PIP_ACTION = "action"
    private const val PIP_REQ_SEEK_BACK = 1001
    private const val PIP_REQ_TOGGLE_PLAY = 1002
    private const val PIP_REQ_SEEK_FWD = 1003
    private const val PIP_DISMISS_CLEANUP_MS = 500L
    @Volatile
    private var pluginsInitializedOnce = false
    @Volatile
    private var lastPluginActivityRef: WeakReference<MainActivity>? = null

    private const val BRIDGE_NAME = "__LEGION_ANDROID_BRIDGE__"
    private val ALLOWED_APP_ORIGINS = setOf(
      "https://tauri.localhost",
      "http://tauri.localhost",
    )

    private val DOCUMENT_START_BRIDGE_JS = """
      (function(){
        var bridge = window.__LEGION_ANDROID_BRIDGE__;
        if (!bridge || typeof bridge.postMessage !== 'function') return;
        var cachedInsets = null;
        var hasLocalNetwork = null;
        var hasPip = false;
        var deviceModel = '';
        var nextId = 1;
        var pending = {};

        function applyInsets(values){
          if (!Array.isArray(values) || values.length < 4) return;
          cachedInsets = values;
          var r = document.documentElement;
          if (!r) return;
          r.style.setProperty('--safe-area-inset-top', Number(values[0]).toFixed(2) + 'px');
          r.style.setProperty('--safe-area-inset-right', Number(values[1]).toFixed(2) + 'px');
          r.style.setProperty('--safe-area-inset-bottom', Number(values[2]).toFixed(2) + 'px');
          r.style.setProperty('--safe-area-inset-left', Number(values[3]).toFixed(2) + 'px');
          r.style.setProperty('--ime-inset-bottom', Number(values[4] || 0).toFixed(2) + 'px');
          window.dispatchEvent(new Event('legion-insets'));
        }

        bridge.onmessage = function(event){
          var msg;
          try {
            msg = JSON.parse(String(event && event.data || ''));
          } catch (_) {
            return;
          }
          if (!msg || typeof msg !== 'object') return;
          if (msg.type === 'init') {
            if (typeof msg.hasLocalNetwork === 'boolean') hasLocalNetwork = msg.hasLocalNetwork;
            if (typeof msg.hasPip === 'boolean') {
              hasPip = msg.hasPip;
              window.dispatchEvent(new CustomEvent('legion:pip-support', { detail: { supported: hasPip } }));
            }
            if (typeof msg.model === 'string') deviceModel = msg.model;
            if (msg.insets) applyInsets(msg.insets);
          } else if (msg.type === 'insets') {
            if (msg.insets) applyInsets(msg.insets);
          } else if (msg.type === 'local_network') {
            var granted = Boolean(msg.granted);
            if (granted) hasLocalNetwork = true;
            var resolve = pending[msg.id];
            if (resolve) {
              delete pending[msg.id];
              resolve(granted);
            }
          } else if (msg.type === 'immersive_ack' || msg.type === 'keep_screen_on_ack' || msg.type === 'pip_enter_ack') {
            var resolveAck = pending[msg.id];
            if (resolveAck) {
              delete pending[msg.id];
              resolveAck(Boolean(msg.active));
            }
          } else if (msg.type === 'pip_changed') {
            window.dispatchEvent(new CustomEvent('legion:pip-change', {
              detail: { active: Boolean(msg.active), dismissed: Boolean(msg.dismissed) }
            }));
          } else if (msg.type === 'pip_action') {
            window.dispatchEvent(new CustomEvent('legion:pip-action', {
              detail: { action: String(msg.action || '') }
            }));
          } else if (msg.type === 'open_music_player') {
            window.dispatchEvent(new Event('legion:open-music-player'));
          }
        };

        window.__LEGION_ANDROID__ = {
          hasLocalNetwork: function(){
            return hasLocalNetwork;
          },
          hasPip: function(){
            return hasPip;
          },
          deviceModel: function(){
            return deviceModel;
          },
          requestLocalNetwork: function(){
            if (hasLocalNetwork === true) return Promise.resolve(true);
            return new Promise(function(resolve){
              var id = nextId++;
              pending[id] = resolve;
              try {
                bridge.postMessage(JSON.stringify({ type: 'request_local_network', id: id }));
              } catch (_) {
                delete pending[id];
                resolve(true);
              }
            });
          },
          setImmersive: function(on, orientation){
            return new Promise(function(resolve){
              var id = nextId++;
              pending[id] = resolve;
              try {
                bridge.postMessage(JSON.stringify({
                  type: 'immersive',
                  id: id,
                  on: Boolean(on),
                  orientation: typeof orientation === 'string' ? orientation : 'none'
                }));
              } catch (_) {
                delete pending[id];
                resolve(false);
              }
            });
          },
          setKeepScreenOn: function(on){
            return new Promise(function(resolve){
              var id = nextId++;
              pending[id] = resolve;
              try {
                bridge.postMessage(JSON.stringify({
                  type: 'keep_screen_on',
                  id: id,
                  on: Boolean(on)
                }));
              } catch (_) {
                delete pending[id];
                resolve(false);
              }
            });
          },
          setPipConfig: function(cfg){
            try {
              var payload = {
                type: 'pip_config',
                active: Boolean(cfg && cfg.active),
                playing: Boolean(cfg && cfg.playing),
                width: Number(cfg && cfg.width) || 0,
                height: Number(cfg && cfg.height) || 0
              };
              if (cfg && cfg.rect && typeof cfg.rect === 'object') {
                payload.rect = {
                  left: Number(cfg.rect.left),
                  top: Number(cfg.rect.top),
                  width: Number(cfg.rect.width),
                  height: Number(cfg.rect.height)
                };
              }
              bridge.postMessage(JSON.stringify(payload));
            } catch (_) {
              // Ignore if bridge is unavailable.
            }
          },
          enterPip: function(){
            return new Promise(function(resolve){
              var id = nextId++;
              pending[id] = resolve;
              try {
                bridge.postMessage(JSON.stringify({ type: 'enter_pip', id: id }));
              } catch (_) {
                delete pending[id];
                resolve(false);
              }
            });
          },
          ackPipDismiss: function(){
            try {
              bridge.postMessage(JSON.stringify({ type: 'pip_dismiss_ack' }));
            } catch (_) {
              // Ignore if bridge is unavailable.
            }
          },
          haptic: function(kind){
            if (kind !== 'tick' && kind !== 'tap') return;
            try {
              bridge.postMessage(JSON.stringify({
                type: 'haptic',
                kind: kind
              }));
            } catch (_) {
              // Ignore if bridge is unavailable.
            }
          }
        };

        document.addEventListener('DOMContentLoaded', function(){
          if (cachedInsets) applyInsets(cachedInsets);
        }, { once: true });

        function notifyHistoryChanged(){
          try {
            bridge.postMessage(JSON.stringify({ type: 'history_changed' }));
          } catch (_) {
            // Ignore if bridge is unavailable.
          }
        }
        try {
          var origPushState = history.pushState;
          if (typeof origPushState === 'function') {
            history.pushState = function(){
              var res = origPushState.apply(this, arguments);
              notifyHistoryChanged();
              return res;
            };
          }
          var origReplaceState = history.replaceState;
          if (typeof origReplaceState === 'function') {
            history.replaceState = function(){
              var res = origReplaceState.apply(this, arguments);
              notifyHistoryChanged();
              return res;
            };
          }
          window.addEventListener('hashchange', notifyHistoryChanged);
          window.addEventListener('popstate', notifyHistoryChanged);
        } catch (_) {
          // Ignore if history hooks cannot be installed.
        }

        try {
          bridge.postMessage(JSON.stringify({ type: 'init' }));
        } catch (_) {
          // Ignore if bridge is unavailable.
        }
      })();
    """.trimIndent()
  }
}
