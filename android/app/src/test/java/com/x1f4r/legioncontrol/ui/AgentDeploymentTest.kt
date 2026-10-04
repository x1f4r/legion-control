package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentDeploymentPath
import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.Platform
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteKind
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Why the machine page offers to install the control agent, or says it cannot.
 *
 * The page keeps the last status on screen after a failed action, while the link says the machine
 * is not answering. Read against the link's system, which there is none of then, that status was
 * said to name no system at all, which sent the user looking at the agent instead of the network.
 */
class AgentDeploymentTest {

    private val linux = MachineSystem(
        id = "linux",
        name = "Linux",
        platform = Platform.LINUX,
        agent = listOf("/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"),
        grammar = RemoteShell.POSIX,
    )

    private val machine = Machine(
        id = "tower",
        name = "Tower",
        endpoints = listOf(
            Endpoint(id = "lan", kind = RouteKind.LAN, host = "192.0.2.10", port = 22, user = "me", systemHint = null, label = "lan"),
        ),
        systems = listOf(linux),
        wake = null,
    )

    private fun status(json: String): AgentStatus = AgentJson.decodeFromString(AgentStatus.serializer(), json)

    private fun path(status: AgentStatus?, link: LinkState) =
        agentDeploymentFor(machine, status?.let { AgentAbilities.of(it) } ?: AgentAbilities.unknown, status, link)

    private fun reason(path: AgentDeploymentPath): String =
        (path as? AgentDeploymentPath.Unavailable)?.reason ?: throw AssertionError("expected unavailable, got $path")

    private val legacy = status("""{"ok":true,"agentVersion":"2.1.0","system":{"id":"linux"}}""")

    @Test
    fun `a status on screen from before a failure asks for a new one`() {
        assertEquals(AgentDeploymentPath.READ_STATUS_FIRST, reason(path(legacy, LinkState.Unsettled("timed out"))))
        assertEquals(AgentDeploymentPath.READ_STATUS_FIRST, reason(path(legacy, LinkState.Offline("no answer"))))
    }

    @Test
    fun `a current status is decided as the wiring decides it`() {
        val online = path(legacy, LinkState.Online(linux))
        assertTrue("got $online", online is AgentDeploymentPath.UploadAndInstall)
    }

    @Test
    fun `a current status that names no system says so`() {
        val nameless = status("""{"ok":true,"agentVersion":"2.1.0"}""")
        assertEquals(
            "The agent did not say which of this machine's systems it is.",
            reason(path(nameless, LinkState.Offline("the agent did not say which system it is"))),
        )
    }

    @Test
    fun `no status at all asks for one, whatever the link`() {
        assertEquals(AgentDeploymentPath.READ_STATUS_FIRST, reason(path(null, LinkState.Unknown)))
    }
}
