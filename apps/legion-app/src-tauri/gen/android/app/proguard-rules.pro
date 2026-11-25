# Preserve @JavascriptInterface entry points used for feeding WindowInsets into the WebView.
-keepattributes *Annotation*
-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}

# Preserve the in-app updater Tauri plugin, its invoke argument model, and its install receiver.
-keep class com.x1f4r.legion.AppUpdaterPlugin { *; }
-keep class com.x1f4r.legion.InstallApkArgs { *; }
-keep class com.x1f4r.legion.UpdateInstallReceiver { *; }
