package com.x1f4r.legion

import android.Manifest
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Color
import android.os.Build
import android.os.Bundle
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
import org.json.JSONArray
import org.json.JSONObject

class MainActivity : TauriActivity() {
  private var attachedWebView: WebView? = null
  @Volatile
  private var bridgeReplyProxy: JavaScriptReplyProxy? = null
  private lateinit var localNetworkPermissionLauncher: ActivityResultLauncher<String>
  private val pendingLocalNetworkRequests = mutableListOf<Int>()

  private val backCallback = object : OnBackPressedCallback(false) {
    override fun handleOnBackPressed() {
      val webView = attachedWebView
      if (webView != null && webView.canGoBack()) {
        webView.goBack()
      } else {
        isEnabled = false
        onBackPressedDispatcher.onBackPressed()
      }
    }
  }

  @Volatile
  private var latestInsets: InsetValues = InsetValues(0f, 0f, 0f, 0f)

  private data class InsetValues(
    val top: Float,
    val right: Float,
    val bottom: Float,
    val left: Float,
  ) {
    fun toJsonArray(): JSONArray = JSONArray().apply {
      put(top.toDouble())
      put(right.toDouble())
      put(bottom.toDouble())
      put(left.toDouble())
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
    applySystemBarStyle(resources.configuration)
    super.onCreate(savedInstanceState)

    // Observe system bar + display cutout insets on the decor view without consuming them so the
    // WebView still receives raw WindowInsets and we can feed CSS variables whenever they change.
    ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { view, windowInsets ->
      val bars = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      val density = view.resources.displayMetrics.density.coerceAtLeast(1f)
      latestInsets = InsetValues(
        top = bars.top / density,
        right = bars.right / density,
        bottom = bars.bottom / density,
        left = bars.left / density,
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
    applySystemBarStyle(resources.configuration)

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

  override fun onWindowFocusChanged(hasFocus: Boolean) {
    super.onWindowFocusChanged(hasFocus)
    if (hasFocus) {
      ViewCompat.requestApplyInsets(window.decorView)
      pushInsetsToWebView()
    }
  }

  override fun onConfigurationChanged(newConfig: Configuration) {
    super.onConfigurationChanged(newConfig)
    applySystemBarStyle(newConfig)
    ViewCompat.requestApplyInsets(window.decorView)
  }

  private fun applySystemBarStyle(config: Configuration) {
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
          if (!Array.isArray(values) || values.length !== 4) return;
          cachedInsets = values;
          var r = document.documentElement;
          if (!r) return;
          r.style.setProperty('--safe-area-inset-top', Number(values[0]).toFixed(2) + 'px');
          r.style.setProperty('--safe-area-inset-right', Number(values[1]).toFixed(2) + 'px');
          r.style.setProperty('--safe-area-inset-bottom', Number(values[2]).toFixed(2) + 'px');
          r.style.setProperty('--safe-area-inset-left', Number(values[3]).toFixed(2) + 'px');
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
          }
        };

        document.addEventListener('DOMContentLoaded', function(){
          if (cachedInsets) applyInsets(cachedInsets);
        }, { once: true });

        try {
          bridge.postMessage(JSON.stringify({ type: 'init' }));
        } catch (_) {
          // Ignore if bridge is unavailable.
        }
      })();
    """.trimIndent()
  }
}
