package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.LocalAddress
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Deciding how to wake a machine from wherever this device happens to be (contract section 10.2).
 *
 * The shared wake vectors pin the plan case by case against one document. These are the cases they
 * leave to each client: a prefix without its dot that text matching would get wrong, the network a
 * packet is bound to, and what this app's own model of a machine hands the plan.
 */
class WakePlanTest {

    private val attic = SiteConfig(
        id = "attic-house",
        name = "Attic house",
        lanPrefixes = listOf("192.168.50."),
        broadcast = listOf("192.168.50.255"),
    )

    private fun machine(id: String, site: String? = null, wake: WakeTarget? = null) = Machine(
        id = id,
        name = id.replaceFirstChar(Char::uppercase),
        endpoints = emptyList(),
        systems = emptyList(),
        wake = wake,
        siteId = site,
    )

    private fun wake(
        broadcasts: List<String> = emptyList(),
        ports: List<Int> = emptyList(),
        lanPrefix: String? = null,
        helpers: List<WakeHelper> = emptyList(),
    ) = WakeTarget(
        mac = "02:00:00:00:00:01",
        broadcasts = broadcasts,
        ports = ports,
        lanPrefix = lanPrefix,
        siteId = null,
        helpers = helpers,
    )

    private fun plan(
        target: Machine,
        vararg addresses: LocalAddress,
        machines: List<Machine> = listOf(target),
        sites: List<SiteConfig> = listOf(attic),
        helpers: Map<String, HelperState> = emptyMap(),
        holds: List<WakeHold> = emptyList(),
    ) = planWake(target, machines, sites, addresses.toList(), null, helpers, holds)

    @Test
    fun `a prefix without its dot never places a machine for an address that merely starts with it`() {
        // The one case the shared plans cannot show: old-box's 10.0.0 ends in 0, where matching as
        // text happens to agree. 10.0.1 does not: as text it takes 10.0.10.x to 10.0.19.x.
        val box = machine("box", wake = wake(lanPrefix = "10.0.1"))
        val away = plan(box, LocalAddress("10.0.10.12", 24))
        assertTrue(away.steps.isEmpty())
        assertEquals(WakeSkip.Reason.ELSEWHERE, away.skipped.first().reason)

        val home = plan(box, LocalAddress("10.0.1.12", 24))
        assertEquals(listOf("10.0.1.255"), (home.steps.single() as WakeStep.Direct).broadcast)
    }

    @Test
    fun `the packet leaves by the network of the address that placed the machine`() {
        val pi = machine("pi", site = "attic-house", wake = wake())
        val tunnel = LocalAddress("100.64.0.9", 32)
        val wifi = LocalAddress("192.168.50.31", 24)
        val step = plan(pi, tunnel, wifi).steps.single() as WakeStep.Direct
        assertEquals(wifi, step.placing)
        assertEquals("the site's own broadcast comes before one derived from the address", listOf("192.168.50.255"), step.broadcast)
    }

    @Test
    fun `broadcasts and ports are sent as written, and only those that are addresses and ports`() {
        val pi = machine("pi", site = "attic-house", wake = wake(broadcasts = listOf("pi.local", "192.168.50.127"), ports = listOf(0, 9, 70_000)))
        val step = plan(pi, LocalAddress("192.168.50.31", 24)).steps.single() as WakeStep.Direct
        assertEquals("a name is never resolved", listOf("192.168.50.127"), step.broadcast)
        assertEquals(listOf(9), step.ports)
        assertEquals(WAKE_REPEATS, step.repeats)

        val defaults = plan(machine("pi", site = "attic-house", wake = wake()), LocalAddress("192.168.50.31", 24)).steps.single() as WakeStep.Direct
        assertEquals(DEFAULT_WAKE_PORTS, defaults.ports)
    }

    @Test
    fun `with no broadcast anywhere the directed broadcast of the placing address is used, and then the limited one`() {
        val bare = SiteConfig(id = "bare", lanPrefixes = listOf("192.168.16.0/20"))
        val nas = machine("nas", site = "bare", wake = wake())
        val directed = plan(nas, LocalAddress("192.168.20.4", 20), sites = listOf(bare)).steps.single() as WakeStep.Direct
        assertEquals(listOf("192.168.31.255"), directed.broadcast)

        val unknownLength = plan(nas, LocalAddress("192.168.20.4", null), sites = listOf(bare)).steps.single() as WakeStep.Direct
        assertEquals(listOf(LIMITED_BROADCAST), unknownLength.broadcast)
        val pointToPoint = plan(nas, LocalAddress("192.168.20.4", 31), sites = listOf(bare)).steps.single() as WakeStep.Direct
        assertEquals(listOf(LIMITED_BROADCAST), pointToPoint.broadcast)
    }

    @Test
    fun `helpers are asked in the order they are written, and an asleep one is only offered to wake first`() {
        val pi = machine("pi", site = "attic-house")
        val nas = machine("nas", site = "attic-house", wake = wake())
        val tower = machine("tower", site = "attic-house", wake = wake(helpers = listOf(WakeHelper("pi", "wake-tower"), WakeHelper("nas", "wake-tower"))))
        val result = plan(
            tower,
            LocalAddress("198.51.100.7", 24),
            machines = listOf(tower, pi, nas),
            helpers = mapOf("pi" to HelperState(HelperState.Kind.READY, mapOf("wake-tower" to "wol")), "nas" to HelperState(HelperState.Kind.ASLEEP)),
        )
        assertEquals(listOf(WakeStep.Helper("pi", "wake-tower", readFirst = false)), result.steps)
        assertEquals(listOf("nas"), result.offerWakeFirst)
    }

    @Test
    fun `a hold stops every command helper and lets a declared wol one run`() {
        val pi = machine("pi")
        val nas = machine("nas")
        val tower = machine("tower", wake = wake(helpers = listOf(WakeHelper("pi", "wake-tower"), WakeHelper("nas", "wake-tower"))))
        val result = plan(
            tower,
            machines = listOf(tower, pi, nas),
            helpers = mapOf(
                "pi" to HelperState(HelperState.Kind.READY, mapOf("wake-tower" to "command")),
                "nas" to HelperState(HelperState.Kind.READY, mapOf("wake-tower" to "wol")),
            ),
            holds = listOf(WakeHold("pi", "wake-tower")),
        )
        assertEquals(listOf(WakeStep.Helper("nas", "wake-tower", readFirst = false)), result.steps)
        assertEquals(WakeSkip.Reason.HELD, result.skipped.single { it.machine == "pi" }.reason)
    }

    @Test
    fun `a helper's state is read off its last status read this session`() {
        val answer = AgentJson.decodeFromString(
            AgentStatus.serializer(),
            """{"ok":true,"contract":3,"actions":[{"id":"wake-tower","kind":"wol"},{"id":"lights","kind":"command"}]}""",
        )
        assertEquals(
            HelperState(HelperState.Kind.READY, mapOf("wake-tower" to "wol", "lights" to "command")),
            HelperState.of(driven = true, read = true, answer = answer, failure = null),
        )
        assertEquals("not read yet this session", HelperState.UNKNOWN, HelperState.of(true, read = false, answer = null, failure = null))
        assertEquals(
            "no system of it can be run from here, whatever it answered",
            HelperState.Kind.NOT_DRIVEN,
            HelperState.of(driven = false, read = true, answer = answer, failure = null).state,
        )
        val refused = AgentJson.decodeFromString(AgentStatus.serializer(), """{"ok":false,"contract":3,"reasonCode":"internal"}""")
        assertEquals("a refusal is read again before its step", HelperState.UNKNOWN, HelperState.of(true, true, refused, null))

        fun after(failure: FailureKind) = HelperState.of(driven = true, read = true, answer = null, failure = failure).state
        assertEquals(HelperState.Kind.ASLEEP, after(FailureKind.NO_ROUTE))
        assertEquals(HelperState.Kind.ASLEEP, after(FailureKind.HOST_UNREACHABLE))
        for (kind in listOf(
            FailureKind.AUTHENTICATION_FAILED, FailureKind.HOST_KEY_CHANGED, FailureKind.HOST_KEY_UNKNOWN,
            FailureKind.INTERPRETER_MISSING, FailureKind.AGENT_MISSING, FailureKind.BAD_REQUEST,
        )) {
            assertEquals("$kind", HelperState.Kind.UNUSABLE, after(kind))
        }
        val others = FailureKind.entries - setOf(
            FailureKind.NO_ROUTE, FailureKind.HOST_UNREACHABLE, FailureKind.AUTHENTICATION_FAILED,
            FailureKind.HOST_KEY_CHANGED, FailureKind.HOST_KEY_UNKNOWN, FailureKind.INTERPRETER_MISSING,
            FailureKind.AGENT_MISSING, FailureKind.BAD_REQUEST,
        )
        for (kind in others) assertEquals("$kind settles nothing", HelperState.Kind.UNKNOWN, after(kind))
    }

    @Test
    fun `readiness polls for 120 seconds after a send, and not at all after none`() {
        val starts = readinessSchedule(sentAnything = true)
        assertEquals(5_000L, starts.first())
        assertTrue(starts.last() <= READINESS_WINDOW_MS)
        assertTrue(starts.last() + 20_000L > READINESS_WINDOW_MS)
        assertEquals(emptyList<Long>(), readinessSchedule(sentAnything = false))
    }
}
