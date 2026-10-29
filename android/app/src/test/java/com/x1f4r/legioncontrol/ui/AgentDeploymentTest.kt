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

    // MARK: - Version floor (contract section 13)

    private val current = status("""{"ok":true,"contract":3,"agentVersion":"3.1.0","system":{"id":"linux"},"agent":{"version":"3.1.0","floor":"3.1.0"}}""")

    private fun install(status: AgentStatus, bundled: String?) =
        agentDeploymentFor(machine, AgentAbilities.of(status), status, LinkState.Online(linux), bundled)

    @Test
    fun `the bundled agent is never offered below the version the machine runs`() {
        val newer = status("""{"ok":true,"contract":3,"agentVersion":"3.2.0","system":{"id":"linux"},"agent":{"version":"3.2.0","floor":"3.1.0"}}""")
        assertTrue(reason(install(newer, "3.1.0")).contains("runs control agent 3.2.0"))
        // An older agent reports no floor at all, and is held to its own version all the same.
        val older = status("""{"ok":true,"contract":3,"agentVersion":"3.0.9","system":{"id":"linux"}}""")
        assertTrue(reason(install(older, "3.0.1")).contains("newer than the 3.0.1"))
    }

    @Test
    fun `the bundled agent is never offered below the floor the machine reports`() {
        val floored = status("""{"ok":true,"contract":3,"agentVersion":"3.0.0","system":{"id":"linux"},"agent":{"version":"3.0.0","floor":"3.1.0"}}""")
        assertTrue(reason(install(floored, "3.0.5")).contains("will not go below version 3.1.0"))
    }

    @Test
    fun `the same or a newer agent is installed over standard input`() {
        assertEquals(AgentDeploymentPath.Stdin, install(current, "3.1.0"))
        assertEquals(AgentDeploymentPath.Stdin, install(current, "3.10.0"))
        assertEquals("a version that does not read decides nothing", AgentDeploymentPath.Stdin, install(current, null))
        assertEquals(
            "the floor rides on the status as the agent reported it",
            "3.1.0",
            current.agent?.floor,
        )
    }

    @Test
    fun `a 2 x agent still takes the bootstrap`() {
        assertTrue(install(legacy, "3.1.0") is AgentDeploymentPath.UploadAndInstall)
    }
}
