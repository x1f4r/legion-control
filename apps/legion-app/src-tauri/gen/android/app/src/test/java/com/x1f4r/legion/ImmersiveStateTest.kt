package com.x1f4r.legion

import android.content.pm.ActivityInfo
import android.view.WindowManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ImmersiveStateTest {
  @Test
  fun enterLandscapeAndLeaveRestoresCutoutAndOrientation() {
    val state = ImmersiveState()
    val enter = state.enter("landscape", WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT)
    assertTrue(state.active)
    assertTrue(enter.active)
    assertTrue(enter.hideSystemBars)
    assertEquals(WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS, enter.cutoutMode)
    assertEquals(ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE, enter.requestedOrientation)

    // Re-entering with a different orientation preserves the original saved cutout mode.
    val portrait = state.enter("portrait", WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS)
    assertEquals(ActivityInfo.SCREEN_ORIENTATION_PORTRAIT, portrait.requestedOrientation)
    assertEquals(WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT, state.savedCutoutMode)

    val leave = state.leave()
    assertFalse(state.active)
    assertFalse(leave.active)
    assertFalse(leave.hideSystemBars)
    assertEquals(WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT, leave.cutoutMode)
    assertEquals(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, leave.requestedOrientation)
    assertNull(state.savedCutoutMode)
  }

  @Test
  fun enterNoneKeepsOrientationUnspecified() {
    val state = ImmersiveState()
    val enter = state.enter("none", WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES)
    assertTrue(enter.hideSystemBars)
    assertEquals(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, enter.requestedOrientation)
  }

  @Test
  fun resetAllClearsImmersiveAndKeepScreenOn() {
    val state = ImmersiveState()
    state.enter("landscape", WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT)
    assertTrue(state.setKeepScreenOn(true))
    assertTrue(state.keepScreenOn)

    val reset = state.resetAll()
    assertFalse(state.active)
    assertFalse(state.keepScreenOn)
    assertFalse(reset.hideSystemBars)
    assertEquals(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, reset.requestedOrientation)
    assertEquals(WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT, reset.cutoutMode)
  }
}
