package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteKind
import com.x1f4r.legioncontrol.net.SiteMatch
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Deciding how to wake a machine from wherever this device happens to be.
 *
 * A magic packet is a link-local broadcast, so a phone in one city cannot wake a machine in another
 * however hard it tries. A machine already on that network can, and that is what a helper is. The
 * order matters, the failures are worth explaining rather than retrying, and nothing here ever wakes
 * a helper on its own: that would turn an always-off tower into an always-on one by accident.
 */
class WakePlanTest {

    private val attic = SiteConfig(
        id = "attic-house",
        name = "Attic house",
        lanPrefixes = listOf("192.168.178."),
        broadcast = listOf("192.168.178.255"),
    )
    private val flat = SiteConfig(
        id = "flat",
        name = "Flat",
        lanPrefixes = listOf("192.168.178."),
        broadcast = listOf("192.168.178.255"),
    )

    private fun machine(
        id: String,
        site: String? = null,
        wake: WakeTarget? = null,
        alwaysOn: Boolean = false,
    ) = Machine(
        id = id,
        name = id.replaceFirstChar(Char::uppercase),
        endpoints = listOf(
            Endpoint(
                id = "lan",
                kind = RouteKind.LAN,
                host = "192.168.178.5",
                port = 22,
                user = "me",
                systemHint = null,
                label = "LAN",
            ),
        ),
        systems = emptyList(),
        wake = wake,
        siteId = site,
        alwaysOn = alwaysOn,
    )

    private fun wakeTarget(
        broadcasts: List<String> = listOf("192.168.178.255"),
        lanPrefix: String? = null,
        site: String? = null,
        helpers: List<WakeHelper> = emptyList(),
    ) = WakeTarget(
        mac = "AA:BB:CC:DD:EE:FF",
        broadcasts = broadcasts,
        ports = listOf(9, 7),
        probeHost = "192.168.178.5",
        probePort = 22,
        lanPrefix = lanPrefix,
        siteId = site,
        helpers = helpers,
    )

    @Test
    fun `a device standing on the machine's site sends the packet itself`() {
        val target = machine("pi", site = "attic-house", wake = wakeTarget(site = "attic-house"))
        val plan = planWake(
            target = target,
            sites = listOf(attic),
            siteMatch = SiteMatch.Confirmed(attic, "192.168.178.31"),
            machines = listOf(target),
            localAddresses = listOf("192.168.178.31"),
        )
        assertTrue(plan is WakePlan.Direct)
        assertEquals(listOf("192.168.178.255"), (plan as WakePlan.Direct).broadcasts)
        assertTrue(plan.why.contains("Attic house"))
    }

    @Test
    fun `a device somewhere else asks a helper that is already there`() {
        val target = machine(
            "pi",
            site = "attic-house",
            wake = wakeTarget(site = "attic-house", helpers = listOf(WakeHelper("router", "wake-pi"))),
        )
        val plan = planWake(
            target = target,
            sites = listOf(attic),
            siteMatch = SiteMatch.Elsewhere,
            machines = listOf(target, machine("router", site = "attic-house", alwaysOn = true)),
            localAddresses = listOf("100.64.0.9"),
        )
        assertTrue(plan is WakePlan.ViaHelpers)
        assertEquals(1, (plan as WakePlan.ViaHelpers).helpers.size)
    }

    /**
     * The case the overlapping-subnet warning exists for.
     *
     * Two houses on the same router defaults look identical from here. Sending a broadcast at the
     * wrong one achieves nothing, while a helper that is genuinely on the target's LAN works from
     * anywhere, so ambiguity falls through to helpers rather than guessing.
     */
    @Test
    fun `an ambiguous site is treated as off-site and goes to the helpers`() {
        val target = machine(
            "pi",
            site = "attic-house",
            wake = wakeTarget(site = "attic-house", helpers = listOf(WakeHelper("router", "wake-pi"))),
        )
        val plan = planWake(
            target = target,
            sites = listOf(attic, flat),
            siteMatch = SiteMatch.Ambiguous(listOf(attic, flat), "192.168.178.31"),
            machines = listOf(target, machine("router", site = "attic-house")),
            localAddresses = listOf("192.168.178.31"),
        )
        assertTrue(plan is WakePlan.ViaHelpers)
        assertTrue((plan as WakePlan.ViaHelpers).why.contains("more than one site"))
    }

    @Test
    fun `helpers are tried in the order they are written`() {
        val helpers = listOf(WakeHelper("pi", "wake-tower"), WakeHelper("router", "wake-tower"))
        val target = machine("tower", site = "attic-house", wake = wakeTarget(site = "attic-house", helpers = helpers))
        val plan = planWake(
            target = target,
            sites = listOf(attic),
            siteMatch = SiteMatch.Elsewhere,
            machines = listOf(target),
            localAddresses = emptyList(),
        )
        assertEquals(helpers, (plan as WakePlan.ViaHelpers).helpers)
    }

    @Test
    fun `a machine with no site and no helper falls back to its own network prefix`() {
        // The pre-sites arrangement, which still has to work exactly as it did.
        val target = machine("pi", wake = wakeTarget(lanPrefix = "10.0.0."))
        val onIt = planWake(
            target = target,
            sites = emptyList(),
            siteMatch = SiteMatch.NoSites,
            machines = listOf(target),
            localAddresses = listOf("10.0.0.31"),
        )
        assertTrue(onIt is WakePlan.Direct)

        val away = planWake(
            target = target,
            sites = emptyList(),
            siteMatch = SiteMatch.NoSites,
            machines = listOf(target),
            localAddresses = listOf("100.64.0.9"),
        )
        assertTrue(away is WakePlan.Impossible)
        assertTrue((away as WakePlan.Impossible).reasons.any { it.contains("No wake helper") })
    }

    @Test
    fun `a machine that cannot be woken at all says so rather than offering a dead button`() {
        val target = machine("pi")
        assertEquals(
            WakePlan.NotConfigured,
            planWake(target, emptyList(), SiteMatch.NoSites, listOf(target), emptyList()),
        )
    }

    @Test
    fun `waking the helper first is offered only when the helper can itself be woken`() {
        val router = machine("router", site = "attic-house")
        val pi = machine("pi", site = "attic-house", wake = wakeTarget(site = "attic-house"))
        val helpers = listOf(WakeHelper("router", "wake-tower"), WakeHelper("pi", "wake-tower"))

        val failure = helperFailure(
            target = machine("tower", site = "attic-house"),
            attempts = listOf("Router did not answer.", "Pi did not answer."),
            machines = listOf(router, pi),
            helpers = helpers,
        )
        assertEquals(
            "only the helper with its own wake block can be offered",
            listOf("Pi"),
            failure.wakeableHelpers,
        )
        assertTrue(failure.reasons.first().contains("Nothing could wake"))
        assertEquals(3, failure.reasons.size)
    }

    @Test
    fun `a machine on its own site with no broadcast address asks a helper instead`() {
        val target = machine(
            "pi",
            site = "attic-house",
            wake = wakeTarget(broadcasts = emptyList(), site = "attic-house", helpers = listOf(WakeHelper("router", "w"))),
        )
        val plan = planWake(
            target = target,
            // The site itself carries no broadcast either, so there is genuinely nowhere to send it.
            sites = listOf(attic.copy(broadcast = emptyList())),
            siteMatch = SiteMatch.Confirmed(attic.copy(broadcast = emptyList()), "192.168.178.31"),
            machines = listOf(target),
            localAddresses = listOf("192.168.178.31"),
        )
        assertTrue(plan is WakePlan.ViaHelpers)
    }

    @Test
    fun `a machine takes the site's broadcast when it names none of its own`() {
        val target = machine("pi", site = "attic-house", wake = wakeTarget(broadcasts = emptyList(), site = "attic-house"))
        val plan = planWake(
            target = target,
            sites = listOf(attic),
            siteMatch = SiteMatch.Confirmed(attic, "192.168.178.31"),
            machines = listOf(target),
            localAddresses = listOf("192.168.178.31"),
        )
        assertEquals(listOf("192.168.178.255"), (plan as WakePlan.Direct).broadcasts)
    }
}
