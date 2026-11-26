package com.x1f4r.legion

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PipStateTest {
  @Test
  fun autoEnterOnlyWhenActiveVideoIsPlaying() {
    val state = PipState()
    assertFalse(state.shouldAutoEnter())
    assertFalse(state.canEnterExplicitly())

    // Paused video: explicit button allowed, auto-enter on Home forbidden.
    state.updateConfig(activeVideo = true, playing = false, videoWidth = 1920, videoHeight = 1080, sourceRect = null)
    assertTrue(state.canEnterExplicitly())
    assertFalse(state.shouldAutoEnter())

    // Playing video: auto-enter enabled.
    state.updateConfig(activeVideo = true, playing = true, videoWidth = 1920, videoHeight = 1080, sourceRect = null)
    assertTrue(state.canEnterExplicitly())
    assertTrue(state.shouldAutoEnter())

    // Closed / inactive video: neither allowed.
    state.updateConfig(activeVideo = false, playing = true, videoWidth = 1920, videoHeight = 1080, sourceRect = null)
    assertFalse(state.canEnterExplicitly())
    assertFalse(state.shouldAutoEnter())
  }

  @Test
  fun clampAspectRatioRespectsAndroidBounds() {
    assertEquals(PipState.AspectRatio(16, 9), PipState.clampAspectRatio(1920, 1080))
    assertEquals(PipState.AspectRatio(4, 3), PipState.clampAspectRatio(640, 480))
    assertEquals(PipState.AspectRatio(9, 16), PipState.clampAspectRatio(1080, 1920))
    assertEquals(PipState.AspectRatio(239, 100), PipState.clampAspectRatio(3840, 1080))
    assertEquals(PipState.AspectRatio(100, 239), PipState.clampAspectRatio(400, 2000))
    assertEquals(PipState.AspectRatio(16, 9), PipState.clampAspectRatio(0, 0))
    assertEquals(PipState.AspectRatio(16, 9), PipState.clampAspectRatio(-100, 100))
  }

  @Test
  fun computeSourceRectMapsAndClampsToScreen() {
    val rect = PipState.computeSourceRect(
      webViewLeft = 0,
      webViewTop = 100,
      density = 2.0f,
      cssLeft = 10.0,
      cssTop = 20.0,
      cssWidth = 300.0,
      cssHeight = 200.0,
      screenWidth = 1080,
      screenHeight = 2400,
    )
    assertEquals(PipState.SourceRect(20, 140, 620, 540), rect)

    val clamped = PipState.computeSourceRect(
      webViewLeft = 0,
      webViewTop = 0,
      density = 3.0f,
      cssLeft = -20.0,
      cssTop = 0.0,
      cssWidth = 500.0,
      cssHeight = 900.0,
      screenWidth = 1080,
      screenHeight = 2400,
    )
    assertEquals(PipState.SourceRect(0, 0, 1080, 2400), clamped)

    assertNull(
      PipState.computeSourceRect(0, 0, 2.0f, 0.0, 0.0, 0.0, 200.0, 1080, 2400),
    )
    assertNull(
      PipState.computeSourceRect(0, 0, 2.0f, Double.NaN, 0.0, 300.0, 200.0, 1080, 2400),
    )
  }

  @Test
  fun pipExitClassifiesExpandVersusDismissAndClearsPlayingOnDismiss() {
    val state = PipState()
    state.updateConfig(true, true, 1280, 720, null)
    state.onPipEntered()
    assertTrue(state.inPip)

    // Expanding back to full screen keeps playing state intact.
    val expanded = state.onPipExited(lifecycleAtLeastStarted = true)
    assertEquals(PipState.ExitKind.EXPANDED, expanded)
    assertFalse(state.inPip)
    assertTrue(state.playing)

    // Dismissing (closing PiP window) clears playing so auto-enter stays off.
    state.onPipEntered()
    val dismissed = state.onPipExited(lifecycleAtLeastStarted = false)
    assertEquals(PipState.ExitKind.DISMISSED, dismissed)
    assertFalse(state.inPip)
    assertFalse(state.playing)
    assertFalse(state.shouldAutoEnter())
  }
}
