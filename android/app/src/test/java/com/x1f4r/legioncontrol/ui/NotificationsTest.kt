package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.TrackedState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * When a change that ended is announced, and what the setting says it comes to (A1, A13).
 *
 * The notification used to be posted whether or not the app was in front, on top of the page that
 * was already saying the same thing, and the setting said On after Android had been told no.
 */
class NotificationsTest {

    private fun ended(action: String) = OperationRecord(
        id = "5b1d0c2e-7f3a-4e19-9c8d-2a6b4f0e1d37",
        machineId = "tower",
        machineName = "Tower",
        intent = OperationIntent(OpKind.UPDATE, service = "demo"),
        subjectName = "Demo",
        initiator = "this phone",
        startedAt = 1_700_000_000_000L,
        updatedAt = 1_700_000_060_000L,
        state = TrackedState.FINISHED,
        summary = "Demo is now 2.0.0.",
        action = action,
    )

    private val updated = ended("updated")

    @Test
    fun `an ending is announced only to somebody who is not looking at the app`() {
        assertTrue(announces(updated, enabled = true, inFront = false))
        assertFalse("the page already says it", announces(updated, enabled = true, inFront = true))
    }

    @Test
    fun `nothing is announced with the setting off, or for a change that did nothing`() {
        assertFalse(announces(updated, enabled = false, inFront = false))
        assertFalse("there was nothing to install", announces(ended("noop"), enabled = true, inFront = false))
    }

    @Test
    fun `the setting is on only while Android lets the app post`() {
        assertEquals(NotificationState.OFF, NotificationState.of(enabled = false, permitted = true))
        assertEquals(NotificationState.OFF, NotificationState.of(enabled = false, permitted = false))
        assertEquals(NotificationState.ON, NotificationState.of(enabled = true, permitted = true))
        assertEquals(
            "turned on here and taken back in the system settings",
            NotificationState.BLOCKED,
            NotificationState.of(enabled = true, permitted = false),
        )
    }
}
