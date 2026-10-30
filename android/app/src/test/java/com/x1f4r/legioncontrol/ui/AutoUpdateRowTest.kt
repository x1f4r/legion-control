package com.x1f4r.legioncontrol.ui

import androidx.compose.ui.state.ToggleableState
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * What the automatic-update row announces (A14).
 *
 * The row was one switch announcing Off for a machine that had not said whether it updates on its
 * own: the drawn switch can only be on or off, but what a screen reader hears is a statement.
 */
class AutoUpdateRowTest {

    @Test
    fun `a setting the machine has not reported is announced as neither on nor off`() {
        assertEquals(ToggleableState.Indeterminate, autoUpdateState(null))
    }

    @Test
    fun `a reported setting is announced as it is`() {
        assertEquals(ToggleableState.On, autoUpdateState(true))
        assertEquals(ToggleableState.Off, autoUpdateState(false))
    }
}
