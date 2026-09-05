package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.SiteConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Working out which network this device is standing on, and admitting when that cannot be done.
 *
 * Two houses behind the same router model have the same private subnet. 192.168.178.0/24 is one
 * router vendor's default and there are millions of them, so an address starting with 192.168.178. is evidence
 * about which network this is and no evidence at all about which house it is in. Getting this wrong
 * means sending a wake packet at the wrong building, so the answer here is allowed to be "not sure".
 */
class SiteDetectionTest {

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
    private val distinct = SiteConfig(
        id = "office",
        name = "Office",
        lanPrefixes = listOf("10.4."),
        broadcast = listOf("10.4.255.255"),
    )

    @Test
    fun `one matching site is confirmed`() {
        val match = matchSite(listOf(attic, distinct), listOf("192.168.178.31"), declaredSiteId = null)
        assertTrue(match is SiteMatch.Confirmed)
        assertEquals("attic-house", match.site?.id)
        assertTrue(match.isOnSite)
    }

    @Test
    fun `two sites on the same private subnet are ambiguous, never the first one`() {
        val match = matchSite(
            listOf(attic, flat),
            listOf("192.168.178.31"),
            declaredSiteId = null,
        )
        assertTrue("picking one would be a coin toss", match is SiteMatch.Ambiguous)
        assertEquals(2, (match as SiteMatch.Ambiguous).candidates.size)
        assertFalse("ambiguity has to read as off-site", match.isOnSite)
        assertTrue(match.explain().contains("unconfirmed"))
    }

    @Test
    fun `the user saying which site it is settles the ambiguity`() {
        val match = matchSite(
            listOf(attic, flat),
            listOf("192.168.178.31"),
            declaredSiteId = "flat",
        )
        assertTrue(match is SiteMatch.Declared)
        assertEquals("flat", match.site?.id)
        assertTrue(match.isOnSite)
        assertTrue(match.explain().contains("Its address 192.168.178.31 agrees"))
    }

    @Test
    fun `a declaration that no address supports stays unconfirmed and cannot authorize wake`() {
        val match = matchSite(listOf(attic, distinct), listOf("100.64.0.9"), declaredSiteId = "office")
        assertTrue(match is SiteMatch.Unconfirmed)
        assertFalse(match.isOnSite)
        assertEquals(null, match.site)
        assertTrue(match.explain().contains("unconfirmed"))
    }

    @Test
    fun `an address on no configured site is elsewhere`() {
        val match = matchSite(listOf(attic), listOf("100.64.0.9"), declaredSiteId = null)
        assertEquals(SiteMatch.Elsewhere, match)
        assertFalse(match.isOnSite)
    }

    @Test
    fun `a document with no sites is the ordinary single-house arrangement`() {
        assertEquals(SiteMatch.NoSites, matchSite(emptyList(), listOf("10.0.0.4"), null))
    }

    @Test
    fun `every attached interface is considered, not only the default route`() {
        // With a tunnel up the default network's only addresses are tunnel addresses, and asking it
        // alone would say "not at home" while the phone sits on the home Wi-Fi.
        val match = matchSite(listOf(distinct), listOf("100.64.0.9", "10.4.1.7"), null)
        assertTrue(match is SiteMatch.Confirmed)
        assertEquals("10.4.1.7", (match as SiteMatch.Confirmed).address)
    }
}
