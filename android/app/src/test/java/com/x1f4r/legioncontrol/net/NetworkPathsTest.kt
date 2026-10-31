package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.data.MachineHints
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Whether a silent address is this phone having no tunnel there, and the bookkeeping of the
 * tunnels and the default network that decides it.
 *
 * With Tailscale off every tailnet address times out, and the app said each machine did not answer:
 * people went looking at machines that were up, when the fix was one switch on the phone.
 */
class NetworkPathsTest {

    private fun endpoint(host: String, kind: RouteKind = RouteKind.REMOTE) =
        Endpoint(id = "tailnet", kind = kind, host = host, port = 22, user = "me", systemHint = null, label = "Tailnet")

    private fun prefix(text: String) = LanPrefix.parse(text)!!

    private val silent = AgentFailure.Unreachable("Tower", "Tailnet: 100.64.0.9:22 did not answer in time.")

    @Test
    fun `a tailnet address no tunnel carries is the phone's doing`() {
        val tailnet = endpoint("100.64.0.9")
        assertTrue(lacksTunnel(tailnet, silent, emptyList()))
        assertTrue("a tunnel for another network carries none of it", lacksTunnel(tailnet, silent, listOf(prefix("10.0.0.0/24"))))
        assertTrue("one that routes a neighbour does not route this one", lacksTunnel(tailnet, silent, listOf(prefix("100.64.0.10/32"))))

        assertFalse(lacksTunnel(tailnet, silent, listOf(TAILNET)))
        assertFalse(lacksTunnel(tailnet, silent, listOf(prefix("100.64.0.9/32"))))
        assertFalse("an exit node routes everything", lacksTunnel(tailnet, silent, listOf(LanPrefix(0L, 0, LanPrefix.Form.CIDR))))
    }

    @Test
    fun `a tailnet name counts as carried by any tunnel that routes part of the tailnet`() {
        val named = endpoint("tower.tail0000.ts.net")
        assertTrue(lacksTunnel(named, silent, emptyList()))
        assertTrue(lacksTunnel(endpoint("TOWER.TAIL0000.TS.NET"), silent, listOf(prefix("10.0.0.0/24"))))
        assertFalse(lacksTunnel(named, silent, listOf(prefix("100.64.0.9/32"))))
        assertFalse(lacksTunnel(named, silent, listOf(TAILNET)))
    }

    @Test
    fun `nothing is put down to a missing tunnel that is not about one`() {
        val tailnet = endpoint("100.64.0.9")
        assertFalse("nothing said about tunnels yet", lacksTunnel(tailnet, silent, null))
        assertFalse("a LAN address", lacksTunnel(endpoint("100.64.0.9", RouteKind.LAN), silent, emptyList()))
        assertFalse("outside the tailnet's range", lacksTunnel(endpoint("100.128.0.1"), silent, emptyList()))
        assertFalse("not a tailnet name", lacksTunnel(endpoint("tower.example"), silent, emptyList()))

        val answered = listOf(
            AgentFailure.NotAuthorised(tailnet, "Tower", "key refused"),
            AgentFailure.HostKeyChanged(tailnet.address, emptyList(), "SHA256:test", "public"),
            AgentFailure.Unreachable("Tower", "link dropped", stage = DispatchStage.AMBIGUOUS),
            AgentFailure.TimedOut(30, "partial"),
        )
        answered.forEach { assertFalse("$it", lacksTunnel(tailnet, it, emptyList())) }
        listOf(FailureKind.HOST_UNREACHABLE, FailureKind.CONNECTION_REFUSED, FailureKind.NO_ROUTE).forEach { kind ->
            assertTrue("$kind", lacksTunnel(tailnet, AgentFailure.Unreachable("Tower", "no", reason = kind), emptyList()))
        }
    }

    @Test
    fun `the failure says so, and keeps what the socket said`() {
        val tailnet = endpoint("100.64.0.9")
        val said = untunnelled(tailnet, silent, emptyList())
        assertTrue("$said", said is AgentFailure.NoTunnel)
        assertEquals("This phone is not connected to the tailnet. Is Tailscale on?", said.summary)
        assertEquals(
            "Tailnet: 100.64.0.9:22 did not answer in time. No tunnel on this phone carries 100.64.0.9, which is a tailnet address.",
            said.detail,
        )
        assertEquals(FailureKind.HOST_UNREACHABLE, said.kind)
        assertEquals(DispatchStage.NOT_STARTED, said.dispatch)

        assertSame(silent, untunnelled(tailnet, silent, listOf(TAILNET)))
    }

    // MARK: - The book

    private val wifi = 101L
    private val tailscale = 102L
    private val cellular = 103L

    @Test
    fun `a tunnel's routes count while it is up and not blocked`() {
        val book = NetworkPathBook()
        assertEquals(NetworkPaths(null, emptyList()), book.snapshot())

        book.tunnel(tailscale, listOf(TAILNET))
        assertEquals(listOf(TAILNET), book.snapshot().tunnels)

        book.blocked(tailscale, true)
        assertEquals(emptyList<LanPrefix>(), book.snapshot().tunnels)
        book.blocked(tailscale, false)
        book.tunnel(tailscale, listOf(TAILNET, prefix("10.0.0.0/24")))
        assertEquals(listOf(TAILNET, prefix("10.0.0.0/24")), book.snapshot().tunnels)

        book.remove(tailscale)
        assertEquals(emptyList<LanPrefix>(), book.snapshot().tunnels)
    }

    /** Each of these is a change in the way out, which lets every address held back be dialled again. */
    @Test
    fun `the default network and every tunnel coming or going change the way out`() {
        val book = NetworkPathBook()
        book.defaultNetwork(wifi)
        val home = book.snapshot()

        book.tunnel(tailscale, listOf(TAILNET))
        book.defaultNetwork(tailscale)
        val tunnelled = book.snapshot()
        assertNotEquals(home, tunnelled)

        book.defaultLost(tailscale)
        book.remove(tailscale)
        book.defaultNetwork(cellular)
        val away = book.snapshot()
        assertNotEquals(tunnelled, away)
        assertNotEquals(home, away)
        assertEquals(NetworkPaths(cellular, emptyList()), away)

        // A network that went away after another took over leaves the new default where it is.
        book.defaultLost(wifi)
        assertEquals(away, book.snapshot())
    }

    /**
     * A dead address was held back for up to four minutes after the network changed, a tunnel came
     * up, or a person pulled to refresh, which looked like a fix that had done nothing.
     */
    @Test
    fun `a change in the way out, or a refresh, dials every held-back address again`() {
        val tailnet = endpoint("100.64.0.9")
        val machine = Machine(id = "tower", name = "Tower", endpoints = listOf(tailnet), systems = emptyList(), wake = null)
        val routes = RouteSelector(machine, NoHints) { 1_000L }
        val wayOut = WayOut { listOf(routes) }
        fun heldBack() = routes.heldBackFor(tailnet) != null

        val home = NetworkPaths(wifi, emptyList())
        wayOut.follow(home)
        assertEquals(home, wayOut.paths)
        routes.penalise(tailnet, "did not answer")
        wayOut.follow(home)
        assertTrue("the same way out lifts nothing", heldBack())

        wayOut.follow(NetworkPaths(wifi, listOf(TAILNET)))
        assertFalse("a tunnel came up", heldBack())
        assertEquals(listOf(TAILNET), wayOut.paths?.tunnels)

        routes.penalise(tailnet, "did not answer")
        wayOut.follow(NetworkPaths(cellular, listOf(TAILNET)))
        assertFalse("the default network changed", heldBack())

        routes.penalise(tailnet, "did not answer")
        wayOut.retry()
        assertFalse("a person asked to check again", heldBack())
    }

    private object NoHints : MachineHints {
        override fun lastGoodRoute(machineId: String): StateFlow<String?> = MutableStateFlow(null)
        override fun rememberRoute(machineId: String, id: String) = Unit
        override fun forgetRoute(machineId: String) = Unit
        override fun lastKnownSystem(machineId: String): StateFlow<String?> = MutableStateFlow(null)
        override fun rememberSystem(machineId: String, systemId: String) = Unit
    }
}
