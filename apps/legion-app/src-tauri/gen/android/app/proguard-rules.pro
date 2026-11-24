# Preserve @JavascriptInterface entry points used for feeding WindowInsets into the WebView.
-keepattributes *Annotation*
-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}
