package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.data.MachineHints
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.junit.Assert.assertEquals
import org.junit.Test

class RouteSelectorTest {
    private fun endpoint(id: String, kind: RouteKind, system: String? = null) = Endpoint(
        id = id,
        kind = kind,
        host = "$id.invalid",
        port = 22,
        user = "me",
        systemHint = system,
        label = id,
    )

    private val linux = endpoint("linux-remote", RouteKind.REMOTE, "linux")
    private val windows = endpoint("windows-remote", RouteKind.REMOTE, "windows")
    private val lan = endpoint("lan", RouteKind.LAN)

    @Test
    fun `boot target leads and on-site LAN is next`() {
        val ordered = orderEndpoints(listOf(linux, windows, lan), null, "windows", preferLan = true)
        assertEquals(listOf("windows-remote", "lan", "linux-remote"), ordered.map { it.id })
    }

    @Test
    fun `off-site routing keeps remote endpoints ahead of LAN`() {
        val ordered = orderEndpoints(listOf(lan, linux, windows), null, null, preferLan = false)
        assertEquals(listOf("linux-remote", "windows-remote", "lan"), ordered.map { it.id })
    }

    @Test
    fun `an authenticated remembered route still leads ordinary calls`() {
        val ordered = orderEndpoints(listOf(linux, windows, lan), "lan", null, preferLan = false)
        assertEquals("lan", ordered.first().id)
    }

    @Test
    fun `an address this app does not dial never stands in for one in backoff`() {
        // A trailing dot passes the document's guard and fails the dial grammar. It is never dialled,
        // so never penalised, and counted as ready it was all the poll after a sleep would try.
        val dotted = endpoint("dotted", RouteKind.REMOTE).copy(host = "tower.example.")
        val good = endpoint("good", RouteKind.REMOTE)
        val machine = Machine(id = "tower", name = "Tower", endpoints = listOf(dotted, good), systems = emptyList(), wake = null)
        val routes = RouteSelector(machine, NoHints) { 1_000L }
        assertEquals(listOf("good"), routes.candidates().map { it.id })

        routes.penalise(good, "did not answer")
        assertEquals(listOf("good"), routes.candidates().map { it.id })

        // With nothing dialable at all, every entry comes back, so the caller can say why.
        val alone = RouteSelector(machine.copy(endpoints = listOf(dotted)), NoHints) { 1_000L }
        assertEquals(listOf("dotted"), alone.candidates().map { it.id })
    }

    /**
     * After a network change, a tunnel coming up or a refresh somebody asked for, an address held
     * back for going unanswered was still held back for up to four minutes.
     */
    @Test
    fun `lifting the backoff dials every address again, from the route that worked last`() {
        val tailnet = endpoint("tailnet", RouteKind.REMOTE)
        val home = endpoint("home", RouteKind.LAN)
        val machine = Machine(id = "tower", name = "Tower", endpoints = listOf(tailnet, home), systems = emptyList(), wake = null)
        val routes = RouteSelector(machine, OneRoute()) { 1_000L }
        routes.remember(home)
        routes.penalise(home, "did not answer")
        routes.penalise(tailnet, "did not answer")
        assertEquals("only the one due soonest", 1, routes.candidates().size)

        routes.liftBackoff()
        assertEquals(listOf("home", "tailnet"), routes.candidates().map { it.id })
    }

    /** Hints that remember the one route that answered. */
    private class OneRoute : MachineHints {
        private val route = MutableStateFlow<String?>(null)
        override fun lastGoodRoute(machineId: String): StateFlow<String?> = route
        override fun rememberRoute(machineId: String, id: String) { route.value = id }
        override fun forgetRoute(machineId: String) { route.value = null }
        override fun lastKnownSystem(machineId: String): StateFlow<String?> = MutableStateFlow(null)
        override fun rememberSystem(machineId: String, systemId: String) = Unit
    }

    private object NoHints : MachineHints {
        override fun lastGoodRoute(machineId: String): StateFlow<String?> = MutableStateFlow(null)
        override fun rememberRoute(machineId: String, id: String) = Unit
        override fun forgetRoute(machineId: String) = Unit
        override fun lastKnownSystem(machineId: String): StateFlow<String?> = MutableStateFlow(null)
        override fun rememberSystem(machineId: String, systemId: String) = Unit
    }
}
