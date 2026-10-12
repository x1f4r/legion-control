package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.security.KeyStoreException

/**
 * What a failure does to the link drawn for a machine.
 *
 * Offline is the one state that says something about the machine itself: asleep, off or away. It
 * offers a wake and, during a reboot, reads as the machine still being on its way back, so nothing
 * that happened on this side of the connection may end up there.
 */
class LinkStateTest {

    @Test
    fun `a phone key that could not be read is not a machine that is offline`() {
        val state = AgentFailure.KeyUnavailable(KeyStoreException("keystore is not ready")).toLinkState()
        assertTrue("got $state", state is LinkState.PhoneKeyUnavailable)
        assertEquals(
            "This phone's key could not be read just now. Nothing was sent.",
            (state as LinkState.PhoneKeyUnavailable).reason,
        )
    }

    @Test
    fun `an address that never answered is still offline`() {
        val state = AgentFailure.Unreachable("Tower", "no answer", null, DispatchStage.NOT_STARTED, FailureKind.HOST_UNREACHABLE)
            .toLinkState()
        assertTrue("got $state", state is LinkState.Offline)
    }
}
