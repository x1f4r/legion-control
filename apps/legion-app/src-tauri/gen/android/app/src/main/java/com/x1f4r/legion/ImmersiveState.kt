package com.x1f4r.legion

import android.content.pm.ActivityInfo
import android.view.WindowManager

/**
 * Pure state machine for Android immersive mode and screen-awake state so lifecycle transitions
 * and restoration rules can be unit tested on the JVM without an Android runtime.
 */
internal class ImmersiveState {
  var active: Boolean = false
    private set

  var orientation: String = ORIENTATION_NONE
    private set

  var keepScreenOn: Boolean = false
    private set

  var savedCutoutMode: Int? = null
    private set

  data class Transition(
    val active: Boolean,
    val cutoutMode: Int?,
    val requestedOrientation: Int,
    val hideSystemBars: Boolean,
  )

  fun enter(requestedOrientationName: String?, currentCutoutMode: Int): Transition {
    if (savedCutoutMode == null) {
      savedCutoutMode = currentCutoutMode
    }
    val normalized = normalizeOrientation(requestedOrientationName)
    active = true
    orientation = normalized
    return Transition(
      active = true,
      cutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS,
      requestedOrientation = toAndroidOrientation(normalized),
      hideSystemBars = true,
    )
  }

  fun leave(): Transition {
    val restoreCutout = savedCutoutMode
    savedCutoutMode = null
    active = false
    orientation = ORIENTATION_NONE
    return Transition(
      active = false,
      cutoutMode = restoreCutout,
      requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED,
      hideSystemBars = false,
    )
  }

  fun setKeepScreenOn(on: Boolean): Boolean {
    keepScreenOn = on
    return keepScreenOn
  }

  fun resetAll(): Transition {
    keepScreenOn = false
    return leave()
  }

  companion object {
    const val ORIENTATION_NONE = "none"
    const val ORIENTATION_LANDSCAPE = "landscape"
    const val ORIENTATION_PORTRAIT = "portrait"

    fun normalizeOrientation(raw: String?): String = when (raw?.trim()?.lowercase()) {
      ORIENTATION_LANDSCAPE -> ORIENTATION_LANDSCAPE
      ORIENTATION_PORTRAIT -> ORIENTATION_PORTRAIT
      else -> ORIENTATION_NONE
    }

    fun toAndroidOrientation(normalized: String): Int = when (normalized) {
      ORIENTATION_LANDSCAPE -> ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
      ORIENTATION_PORTRAIT -> ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
      else -> ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
    }
  }
}
