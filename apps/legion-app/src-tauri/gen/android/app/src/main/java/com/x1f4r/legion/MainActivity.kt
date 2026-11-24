package com.x1f4r.legion

import android.content.res.Configuration
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.system.Os
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature

class MainActivity : TauriActivity() {
  private var attachedWebView: WebView? = null

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
    fun toCsv(): String = String.format(
      java.util.Locale.US,
      "%.2f,%.2f,%.2f,%.2f",
      top,
      right,
      bottom,
      left,
    )
  }

  private inner class InsetsBridge {
    @JavascriptInterface
    fun get(): String = latestInsets.toCsv()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // Expose the device model to Rust so `device_name()` needs neither unsafe JNI nor subprocesses.
    runCatching {
      val model = Build.MODEL?.trim().orEmpty()
      if (model.isNotEmpty()) {
        Os.setenv("LEGION_DEVICE_MODEL", model, true)
      }
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
    applySystemBarStyle(resources.configuration)

    // Android 17 WebView reports 0px for CSS env(safe-area-inset-*). Expose the activity's real
    // insets via a read-only bridge and apply them at document start so cold launches never paint
    // under the status bar or gesture bar before the first insets callback reaches the loaded page.
    webView.addJavascriptInterface(InsetsBridge(), "__LEGION_ANDROID_INSETS__")
    if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
      WebViewCompat.addDocumentStartJavaScript(webView, DOCUMENT_START_INSETS_JS, setOf("*"))
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
    val webView = attachedWebView ?: return
    val insets = latestInsets
    val js = String.format(
      java.util.Locale.US,
      """
      (function(){
        var r = document.documentElement;
        if (!r) return;
        r.style.setProperty('--safe-area-inset-top', '%.2fpx');
        r.style.setProperty('--safe-area-inset-right', '%.2fpx');
        r.style.setProperty('--safe-area-inset-bottom', '%.2fpx');
        r.style.setProperty('--safe-area-inset-left', '%.2fpx');
        window.dispatchEvent(new Event('legion-insets'));
      })();
      """.trimIndent(),
      insets.top,
      insets.right,
      insets.bottom,
      insets.left,
    )
    webView.post { webView.evaluateJavascript(js, null) }
  }

  companion object {
    private val DOCUMENT_START_INSETS_JS = """
      (function(){
        function applyInsets(){
          var r = document.documentElement;
          var bridge = window.__LEGION_ANDROID_INSETS__;
          if (!r || !bridge || typeof bridge.get !== 'function') return;
          var parts = String(bridge.get()).split(',');
          if (parts.length !== 4) return;
          r.style.setProperty('--safe-area-inset-top', parts[0] + 'px');
          r.style.setProperty('--safe-area-inset-right', parts[1] + 'px');
          r.style.setProperty('--safe-area-inset-bottom', parts[2] + 'px');
          r.style.setProperty('--safe-area-inset-left', parts[3] + 'px');
          window.dispatchEvent(new Event('legion-insets'));
        }
        applyInsets();
        document.addEventListener('DOMContentLoaded', applyInsets, { once: true });
      })();
    """.trimIndent()
  }
}
