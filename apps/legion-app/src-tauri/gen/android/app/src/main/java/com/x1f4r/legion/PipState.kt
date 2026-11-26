package com.x1f4r.legion

import kotlin.math.abs
import kotlin.math.roundToInt

/**
 * Pure JVM-testable state machine and geometry helpers for Android Picture-in-Picture mode.
 */
internal class PipState {
  var activeVideo: Boolean = false
    private set

  var playing: Boolean = false
    private set

  var inPip: Boolean = false
    private set

  var videoWidth: Int = 0
    private set

  var videoHeight: Int = 0
    private set

  var sourceRect: SourceRect? = null
    private set

  data class AspectRatio(val numerator: Int, val denominator: Int)

  data class SourceRect(val left: Int, val top: Int, val right: Int, val bottom: Int)

  enum class ExitKind {
    EXPANDED,
    DISMISSED,
  }

  fun updateConfig(
    activeVideo: Boolean,
    playing: Boolean,
    videoWidth: Int,
    videoHeight: Int,
    sourceRect: SourceRect?,
  ) {
    this.activeVideo = activeVideo
    this.playing = activeVideo && playing
    this.videoWidth = if (activeVideo && videoWidth > 0) videoWidth else 0
    this.videoHeight = if (activeVideo && videoHeight > 0) videoHeight else 0
    this.sourceRect = if (activeVideo) sourceRect else null
  }

  fun shouldAutoEnter(): Boolean = activeVideo && playing

  fun canEnterExplicitly(): Boolean = activeVideo

  fun aspectRatio(): AspectRatio = clampAspectRatio(videoWidth, videoHeight)

  fun onPipEntered() {
    inPip = true
  }

  fun onPipExited(lifecycleAtLeastStarted: Boolean): ExitKind {
    inPip = false
    return if (lifecycleAtLeastStarted) {
      ExitKind.EXPANDED
    } else {
      playing = false
      ExitKind.DISMISSED
    }
  }

  fun resetAll() {
    activeVideo = false
    playing = false
    inPip = false
    videoWidth = 0
    videoHeight = 0
    sourceRect = null
  }

  companion object {
    // Android enforces aspect ratio strictly inside [1/2.39, 2.39/1].
    const val MIN_RATIO_NUM = 100
    const val MIN_RATIO_DEN = 239
    const val MAX_RATIO_NUM = 239
    const val MAX_RATIO_DEN = 100
    const val DEFAULT_RATIO_NUM = 16
    const val DEFAULT_RATIO_DEN = 9

    const val ACTION_SEEK_BACK_10 = "seek_back_10"
    const val ACTION_TOGGLE_PLAY = "toggle_play"
    const val ACTION_SEEK_FWD_10 = "seek_fwd_10"

    fun clampAspectRatio(width: Int, height: Int): AspectRatio {
      if (width <= 0 || height <= 0) {
        return AspectRatio(DEFAULT_RATIO_NUM, DEFAULT_RATIO_DEN)
      }
      val ratio = width.toDouble() / height.toDouble()
      val minRatio = MIN_RATIO_NUM.toDouble() / MIN_RATIO_DEN.toDouble()
      val maxRatio = MAX_RATIO_NUM.toDouble() / MAX_RATIO_DEN.toDouble()
      if (ratio < minRatio) {
        return AspectRatio(MIN_RATIO_NUM, MIN_RATIO_DEN)
      }
      if (ratio > maxRatio) {
        return AspectRatio(MAX_RATIO_NUM, MAX_RATIO_DEN)
      }
      val divisor = gcd(width, height).coerceAtLeast(1)
      var num = width / divisor
      var den = height / divisor
      if (num > 10_000 || den > 10_000) {
        val scale = 1000.0 / maxOf(num, den).toDouble()
        num = (num * scale).roundToInt().coerceAtLeast(1)
        den = (den * scale).roundToInt().coerceAtLeast(1)
      }
      return AspectRatio(num, den)
    }

    fun computeSourceRect(
      webViewLeft: Int,
      webViewTop: Int,
      density: Float,
      cssLeft: Double,
      cssTop: Double,
      cssWidth: Double,
      cssHeight: Double,
      screenWidth: Int,
      screenHeight: Int,
    ): SourceRect? {
      if (
        !cssLeft.isFinite() ||
        !cssTop.isFinite() ||
        !cssWidth.isFinite() ||
        !cssHeight.isFinite() ||
        cssWidth <= 1.0 ||
        cssHeight <= 1.0 ||
        screenWidth <= 0 ||
        screenHeight <= 0
      ) {
        return null
      }
      val safeDensity = density.coerceAtLeast(1f).toDouble()
      val rawLeft = webViewLeft + (cssLeft * safeDensity).roundToInt()
      val rawTop = webViewTop + (cssTop * safeDensity).roundToInt()
      val rawRight = webViewLeft + ((cssLeft + cssWidth) * safeDensity).roundToInt()
      val rawBottom = webViewTop + ((cssTop + cssHeight) * safeDensity).roundToInt()

      val left = rawLeft.coerceIn(0, screenWidth)
      val top = rawTop.coerceIn(0, screenHeight)
      val right = rawRight.coerceIn(0, screenWidth)
      val bottom = rawBottom.coerceIn(0, screenHeight)
      if (right <= left || bottom <= top) {
        return null
      }
      return SourceRect(left, top, right, bottom)
    }

    private fun gcd(a: Int, b: Int): Int {
      var x = abs(a)
      var y = abs(b)
      while (y != 0) {
        val r = x % y
        x = y
        y = r
      }
      return x
    }
  }
}
