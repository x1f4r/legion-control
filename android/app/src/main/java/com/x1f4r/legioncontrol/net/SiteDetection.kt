package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.SiteConfig

/**
 * Which site this device is standing on, and how sure that is.
 *
 * The honest answer is usually "probably". A site is recognised by the phone holding an address that
 * starts with one of its prefixes, and two houses behind the same router model have the same private
 * subnet: 192.168.178.0/24 is one router vendor's default and there are millions of them. An address that
 * starts with 192.168.178. is therefore evidence about which network this is and no evidence at all
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

    /** The user said which site this is. Their word beats a prefix, which is only a guess. */
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
            " Its address $matchedAddress agrees."

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
 * Works out the site from the addresses this device holds and what the user has declared.
 *
 * [addresses] is every IPv4 address on every attached interface, not only the default route's: with
 * a tunnel up the default network's only addresses are tunnel addresses, and asking it alone would
 * say "not at home" while the phone sits on the home Wi-Fi.
 */
fun matchSite(
    sites: List<SiteConfig>,
    addresses: List<String>,
    declaredSiteId: String?,
): SiteMatch {
    if (sites.isEmpty()) return SiteMatch.NoSites

    val matches = sites.filter { site ->
        site.lanPrefixes.any { prefix ->
            prefix.isNotBlank() && addresses.any { it.startsWith(prefix) }
        }
    }

    fun addressFor(site: SiteConfig): String? = site.lanPrefixes
        .firstNotNullOfOrNull { prefix ->
            prefix.takeIf { it.isNotBlank() }?.let { p -> addresses.firstOrNull { it.startsWith(p) } }
        }

    declaredSiteId?.takeIf { it.isNotBlank() }?.let { declared ->
        sites.firstOrNull { it.id == declared }?.let { site ->
            val address = addressFor(site)
            return if (address != null) SiteMatch.Declared(site, address) else SiteMatch.Unconfirmed(site)
        }
    }

    return when (matches.size) {
        0 -> SiteMatch.Elsewhere
        1 -> SiteMatch.Confirmed(matches.single(), addressFor(matches.single()).orEmpty())
        else -> SiteMatch.Ambiguous(matches, addressFor(matches.first()).orEmpty())
    }
}
