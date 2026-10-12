package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentAction
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Which of an agent's actions are offered as buttons (A5).
 *
 * An action id goes onto a command line when it is pressed. One that is not a token used to be
 * offered anyway, and pressing it threw an IllegalArgumentException that nothing caught.
 */
class RunnableActionsTest {

    @Test
    fun `only actions whose id can be sent back are offered`() {
        val offered = listOf(
            AgentAction(id = "wake-tower"),
            AgentAction(id = "wake tower"),
            AgentAction(id = "wake;reboot"),
            AgentAction(id = ""),
            AgentAction(id = null),
            AgentAction(id = "-flag"),
        )
        assertEquals(listOf("wake-tower"), runnableActions(offered).map { it.id })
        assertEquals(emptyList<AgentAction>(), runnableActions(null))
    }
}
