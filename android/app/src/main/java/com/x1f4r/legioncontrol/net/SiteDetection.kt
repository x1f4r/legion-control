package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.data.SiteConfig

/**
 * Which site this device is standing on, and how sure that is.
 *
 * The honest answer is usually "probably". A site is recognised by the phone holding an address
 * inside one of its prefixes, and two houses behind the same router model have the same private
 * subnet: 192.168.50.0/24 is one router vendor's default and there are millions of them. An address that
 * starts with 192.168.50. is therefore evidence about which network this is and no evidence at all
 * about which house it is in.
 *
 * So this never picks the first match. Where several sites match, the answer is [Ambiguous] and
 * everything downstream treats it as off-site unless the user has said otherwise, because sending a
 * broadcast at the wrong house is at best useless. What settles it is either the user saying so, in
 * their private bindings, or the thing that actually authenticates a machine: its pinned host key.
 * An address never authenticates anything.
 */
sealed interface SiteMatch {
    /** Exactly one site claims this address range, and nothing has contradicted it. */
    data class Confirmed(val matched: SiteConfig, val address: String) : SiteMatch

    /**
     * The user said which site this is, and nothing here contradicts it: one of its prefixes holds
     * [matchedAddress], or it names no prefix and is taken on their word.
     */
    data class Declared(val declared: SiteConfig, val matchedAddress: String?) : SiteMatch

    /** The saved choice names a site, but no attached interface currently supports that choice. */
    data class Unconfirmed(val declared: SiteConfig) : SiteMatch

    /**
     * Several sites use the same private range, so the address cannot say which one this is.
     *
     * Reported rather than resolved. Picking one would be a coin toss with somebody's machines on
     * the other side of it.
     */
    data class Ambiguous(val candidates: List<SiteConfig>, val address: String) : SiteMatch

    /** No site's prefixes match anything this device holds. */
    data object Elsewhere : SiteMatch

    /** The document names no sites at all, which is the ordinary single-house arrangement. */
    data object NoSites : SiteMatch

    /** The site this resolves to, when it resolves to one. */
    val site: SiteConfig?
        get() = when (this) {
            is Confirmed -> matched
            is Declared -> declared
            else -> null
        }

    /** Whether a broadcast sent from here would plausibly reach that site's machines. */
    val isOnSite: Boolean get() = this is Confirmed || this is Declared

    /** One line for the screen. Never claims more than it knows. */
    fun explain(): String = when (this) {
        is Confirmed -> "On ${matched.displayName}, from this device's address $address."
        is Declared -> "You have said this device is at ${declared.displayName}." +
            (matchedAddress?.let { " Its address $it agrees." } ?: "")

        is Unconfirmed -> "This device is saved as ${declared.displayName}, but none of its current " +
            "addresses match that site, so the site is unconfirmed and local wake is disabled."

        is Ambiguous -> "This device's address $address matches " +
            candidates.joinToString(" and ") { it.displayName } +
            ", which use the same private range, so the site is unconfirmed. Choose one under this " +
            "device to wake machines from here."

        Elsewhere -> "This device is not on any configured site's network."
        NoSites -> "No sites are configured, so each machine's own network prefix is used."
    }
}

/**
 * Where this device is, by contract section 10.1: every site with a valid prefix holding one of its
 * addresses, in document order, and the one site that is effective, if any.
 */
data class Placement(
    val matching: List<String>,
    val effective: String?,
    /** Several sites match and the person has not said which: "site unconfirmed". */
    val ambiguous: Boolean,
)

/** Whether a machine is on this device's network, and why not when it is not (section 10.1). */
data class NetworkTest(
    val on: Boolean,
    /** `ambiguous`, `unplaced` or `elsewhere` when it is not on it. */
    val reason: String?,
    /** The first of this device's addresses inside the prefix that placed the machine, or null. */
    val placing: LocalAddress?,
)

/** The prefixes of [site] that parse. The others never match anything. */
private fun validPrefixes(site: SiteConfig): List<LanPrefix> = site.lanPrefixes.mapNotNull(LanPrefix::parse)

/** The first of [addresses] inside any of [prefixes]: the placing address. */
private fun firstInside(prefixes: List<LanPrefix>, addresses: List<LocalAddress>): LocalAddress? =
    addresses.firstOrNull { candidate ->
        val value = candidate.value ?: return@firstOrNull false
        prefixes.any { it.contains(value) }
    }

/**
 * Where this device is: `placement` in `contract/tools/wake-plan.mjs`.
 *
 * A [currentSite] that names a site decides, but only while that site has no valid prefix or one of
 * its prefixes matches: the person's answer resolves two sites behind the same subnet, it does not
 * put this device on a network it is not on. One that names no site is ignored. Otherwise exactly
 * one matching site is effective, and several are ambiguous: a device never picks the first of two
 * homes behind the same stock router.
 */
fun placement(sites: List<SiteConfig>, addresses: List<LocalAddress>, currentSite: String?): Placement {
    fun matches(site: SiteConfig) = firstInside(validPrefixes(site), addresses) != null
    val matching = sites.filter(::matches).map { it.id }
    val named = currentSite?.let { id -> sites.firstOrNull { it.id == id } }
    if (named != null) {
        val corroborated = validPrefixes(named).isEmpty() || matches(named)
        return Placement(matching, if (corroborated) named.id else null, ambiguous = false)
    }
    if (matching.size == 1) return Placement(matching, matching.single(), ambiguous = false)
    return Placement(matching, null, ambiguous = matching.size > 1)
}

/**
 * Whether a machine at site [machineSite], or with the deprecated [lanPrefix], is on this device's
 * network: `onTargetNetwork` in `contract/tools/wake-plan.mjs`, the one test route order and direct
 * wake share.
 *
 * A machine that names a site in [sites] is on the network exactly when that site is effective.
 * Only a machine that names no such site falls back to its deprecated `wake.lanPrefix`, matched on
 * octet and bit boundaries like every prefix, so `10.0.1` never places a machine for an address on
 * 10.0.10.x.
 */
fun onNetwork(
    machineSite: String?,
    lanPrefix: String?,
    sites: List<SiteConfig>,
    addresses: List<LocalAddress>,
    currentSite: String?,
): NetworkTest {
    val site = machineSite?.let { id -> sites.firstOrNull { it.id == id } }
    if (site != null) {
        val place = placement(sites, addresses, currentSite)
        if (place.effective == site.id) return NetworkTest(true, null, firstInside(validPrefixes(site), addresses))
        val reason = if (place.ambiguous && site.id in place.matching) "ambiguous" else "elsewhere"
        return NetworkTest(false, reason, null)
    }
    val prefix = LanPrefix.parse(lanPrefix) ?: return NetworkTest(false, "unplaced", null)
    val placing = firstInside(listOf(prefix), addresses) ?: return NetworkTest(false, "elsewhere", null)
    return NetworkTest(true, null, placing)
}

/** The ids of [machines] that are on this device's network, each by [onNetwork]. */
fun machinesOnSite(
    machines: List<Machine>,
    sites: List<SiteConfig>,
    addresses: List<LocalAddress>,
    currentSite: String?,
): Set<String> =
    machines.filter { onNetwork(it.siteId, it.wake?.lanPrefix, sites, addresses, currentSite).on }.map { it.id }.toSet()

/**
 * Works out the site from the addresses this device holds and what the user has declared.
 *
 * [addresses] are this device's candidate addresses: every IPv4 address on every network that can
 * carry a broadcast and is neither a VPN nor blocked for this app, and not only the default route's.
 * With a tunnel up the default network's only addresses are tunnel addresses, and asking it alone
 * would say "not at home" while the phone sits on the home Wi-Fi.
 */
fun matchSite(
    sites: List<SiteConfig>,
    addresses: List<LocalAddress>,
    declaredSiteId: String?,
): SiteMatch {
    if (sites.isEmpty()) return SiteMatch.NoSites
    val place = placement(sites, addresses, declaredSiteId)
    fun addressFor(site: SiteConfig): String? = firstInside(validPrefixes(site), addresses)?.address

    declaredSiteId?.let { id -> sites.firstOrNull { it.id == id } }?.let { declared ->
        return if (place.effective == declared.id) {
            SiteMatch.Declared(declared, addressFor(declared))
        } else {
            SiteMatch.Unconfirmed(declared)
        }
    }

    val matches = place.matching.mapNotNull { id -> sites.firstOrNull { it.id == id } }
    return when (matches.size) {
        0 -> SiteMatch.Elsewhere
        1 -> SiteMatch.Confirmed(matches.single(), addressFor(matches.single()).orEmpty())
        else -> SiteMatch.Ambiguous(matches, addressFor(matches.first()).orEmpty())
    }
}
