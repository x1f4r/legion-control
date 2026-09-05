package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.SiteMatch

/**
 * How to wake one machine from where this device is standing.
 *
 * There is no longer one house and no longer one machine that is always on. A magic packet is a
 * link-local broadcast, so waking a machine in another building is not a matter of trying harder: it
 * needs somebody already on that network to send it. That somebody is a helper, helpers can themselves be asleep,
 * and so there is an order and there are failures worth explaining rather than retrying.
 *
 * Nothing here wakes a helper. The user asked not to be pushed towards leaving machines on, and a
 * cascade that wakes the tower to wake the laptop turns an always-off tower into an always-on one by
 * accident. Where a helper is itself wakeable, the app offers that as a separate thing to press.
 */
sealed interface WakePlan {
    /** This device is on the machine's own network. Send the packet from here. */
    data class Direct(
        val broadcasts: List<String>,
        val ports: List<Int>,
        /** The prefixes the socket has to leave by, so a tunnel does not swallow the broadcast. */
        val prefixes: List<String>,
        val why: String,
    ) : WakePlan

    /** Ask each of these in turn, stopping at the first that says it sent something. */
    data class ViaHelpers(val helpers: List<WakeHelper>, val why: String) : WakePlan

    /**
     * Nothing here can wake it, and this says why for each thing that was considered.
     *
     * [wakeableHelpers] are helpers that could themselves be woken, offered as a separate explicit
     * action rather than done automatically.
     */
    data class Impossible(
        val reasons: List<String>,
        val wakeableHelpers: List<String>,
    ) : WakePlan

    /** The machine has no wake configuration at all, so the action is not drawn. */
    data object NotConfigured : WakePlan
}

/**
 * Works out the plan.
 *
 * [siteMatch] is deliberately allowed to be uncertain. An ambiguous site means several configured
 * sites share a private range, and the safe reading of that is "not on site": sending a broadcast at
 * the wrong house achieves nothing, whereas asking a helper that is genuinely on the target's LAN
 * works from anywhere. So ambiguity falls through to helpers rather than guessing.
 */
fun planWake(
    target: Machine,
    sites: List<SiteConfig>,
    siteMatch: SiteMatch,
    /** Every machine in the document, so a helper can be named and its own wake seen. */
    machines: List<Machine>,
    /** The prefixes this device holds an address in, for the unplaced single-house case. */
    localAddresses: List<String>,
): WakePlan {
    val wake = target.wake ?: return WakePlan.NotConfigured
    val site = target.siteId?.let { id -> sites.firstOrNull { it.id == id } }

    val onTargetNetwork = when {
        // The machine names a site, and this device is on it.
        site != null -> siteMatch.site?.id == site.id
        // No sites in the document: the old single-house arrangement, one prefix per machine.
        !wake.lanPrefix.isNullOrBlank() -> localAddresses.any { it.startsWith(wake.lanPrefix) }
        else -> false
    }

    val broadcasts = wake.broadcasts.ifEmpty { site?.broadcast.orEmpty() }
    val prefixes = site?.lanPrefixes ?: listOfNotNull(wake.lanPrefix)

    if (onTargetNetwork && broadcasts.isNotEmpty()) {
        return WakePlan.Direct(
            broadcasts = broadcasts,
            ports = wake.ports,
            prefixes = prefixes,
            why = site?.let { "This device is on ${it.displayName}." }
                ?: "This device is on ${target.name}'s network.",
        )
    }

    if (wake.helpers.isNotEmpty()) {
        val why = when {
            onTargetNetwork ->
                "This device is on the right network and ${target.name} has no broadcast address " +
                    "configured, so a helper is asked instead."

            siteMatch is SiteMatch.Ambiguous ->
                "This device's address matches more than one site, so it cannot be trusted to be on " +
                    "${target.name}'s network. A helper that is already there is asked instead."

            else -> "This device is not on ${target.name}'s network, so a helper that is asks for it."
        }
        return WakePlan.ViaHelpers(wake.helpers, why)
    }

    val reasons = buildList {
        if (onTargetNetwork && broadcasts.isEmpty()) {
            add("${target.name} has no broadcast address configured, so a packet has nowhere to go.")
        } else {
            add(
                when (siteMatch) {
                    is SiteMatch.Ambiguous -> siteMatch.explain()
                    else -> "This device is not on ${target.name}'s network, and a magic packet is a " +
                        "LAN broadcast that a tunnel cannot carry."
                },
            )
        }
        add("No wake helper is configured for ${target.name}.")
    }
    return WakePlan.Impossible(reasons, wakeableHelpers = emptyList())
}

/**
 * What to say and offer when every helper has been tried and none of them worked.
 *
 * [attempts] is one line per helper, in the order they were asked. The offer to wake a helper first
 * is deliberately a separate press: it is a second machine being started, which is exactly the
 * energy cost the user asked not to be signed up to silently.
 */
fun helperFailure(
    target: Machine,
    attempts: List<String>,
    machines: List<Machine>,
    helpers: List<WakeHelper>,
): WakePlan.Impossible {
    val wakeable = helpers.mapNotNull { helper ->
        machines.firstOrNull { it.id == helper.machineId && it.wake != null }?.name
    }.distinct()
    return WakePlan.Impossible(
        reasons = listOf("Nothing could wake ${target.name} from here.") + attempts,
        wakeableHelpers = wakeable,
    )
}
