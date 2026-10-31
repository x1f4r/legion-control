package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
        assertFalse((state as LinkState.Offline).noTunnel)
    }

    /**
     * The machine page said "Asleep or unreachable" and the overview "Not reachable" while the
     * footer said the phone was off the tailnet: the most prominent words still blamed the machine.
     */
    @Test
    fun `a tailnet address no tunnel carries is offline, and says it is the phone's doing`() {
        val state = AgentFailure.NoTunnel("No tunnel on this phone carries 100.64.0.9, which is a tailnet address.").toLinkState()
        assertTrue("got $state", state is LinkState.Offline)
        assertTrue((state as LinkState.Offline).noTunnel)
        assertEquals("This phone is not connected to the tailnet. Is Tailscale on?", state.reason)
    }
}
