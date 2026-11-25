package com.x1f4r.legion

import android.app.Activity
import android.Manifest
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.WindowManager
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
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
          put("model", Build.MODEL?.trim().orEmpty())
        }
        runCatching { replyProxy.postMessage(payload.toString()) }
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
    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    applyImmersiveTransition(transition)
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
    requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    applySystemBarStyle(resources.configuration)
    super.onCreate(savedInstanceState)
    // If the process stayed alive for TransferForegroundService after the user swiped Legion away
    // from Recents, notify Rust so it rebuilds the main WebView inside this recreated Activity.
    TransferBridge.sendEvent(JSONObject().put("event", "activity_created"))

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
    }
    pluginsInitializedOnce = true
    lastPluginActivityRef = WeakReference(this)
  }

  override fun onResume() {
    super.onResume()
    PluginManager.activity = this
    TransferBridge.setActiveActivity(this)
    attachedWebView?.let { syncBackNavigation(it) }
    reapplyImmersiveBarsIfActive()
  }

  override fun onDestroy() {
    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    immersiveState.resetAll()
    if (TransferBridge.currentActivity() === this) {
      TransferBridge.setActiveActivity(null)
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
    if (immersiveState.active) {
      reapplyImmersiveBarsIfActive()
    } else {
      applySystemBarStyle(newConfig)
    }
    ViewCompat.requestApplyInsets(window.decorView)
  }

  private fun applySystemBarStyle(config: Configuration) {
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
          } else if (msg.type === 'immersive_ack' || msg.type === 'keep_screen_on_ack') {
            var resolveAck = pending[msg.id];
            if (resolveAck) {
              delete pending[msg.id];
              resolveAck(Boolean(msg.active));
            }
          }
        };

        window.__LEGION_ANDROID__ = {
          hasLocalNetwork: function(){
            return hasLocalNetwork;
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
