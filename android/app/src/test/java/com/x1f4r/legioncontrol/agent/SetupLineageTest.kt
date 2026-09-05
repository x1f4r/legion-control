package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Deciding what to do about the setup copy a machine is holding.
 *
 * Every device is a peer, so two of them can edit while one is offline. Revision numbers cannot tell
 * "newer" from "diverged": both people edit revision 5 and both produce a 6, and a number comparison
 * would let the second silently erase the first. Ancestry can, because descent is a fact rather than
 * a count.
 *
 * Only two things happen without asking, and both are along strict descent. Everything else stops at
 * a person, which is the property this file is here to hold.
 */
class SetupLineageTest {

    private fun provenance(
        id: String? = "setup-abc",
        revision: Long? = 5,
        hash: String? = "h5",
        lineage: List<String> = emptyList(),
    ) = SetupProvenance(
        authority = id,
        revision = revision,
        hash = hash,
        lineage = lineage,
    )

    // MARK: - What the status alone settles

    @Test
    fun `a machine carrying the same copy needs nothing`() {
        val decision = decideFromStatus("h5", provenance(), speaksV3 = true)
        assertEquals(SetupDecision.InSync, decision)
    }

    @Test
    fun `a machine carrying nothing is pushed to without a prompt`() {
        val decision = decideFromStatus(null, provenance(), speaksV3 = true)
        assertTrue(decision is SetupDecision.Push)
    }

    @Test
    fun `a machine carrying an ancestor of this copy is pushed to without a prompt`() {
        // Strict descent: my document was made from theirs, so nothing of theirs can be lost.
        val mine = provenance(hash = "h6", revision = 6, lineage = listOf("h5", "h4"))
        val decision = decideFromStatus("h5", mine, speaksV3 = true)
        assertTrue(decision is SetupDecision.Push)
    }

    @Test
    fun `anything else costs one round trip rather than a guess`() {
        val decision = decideFromStatus("unknown-hash", provenance(), speaksV3 = true)
        assertEquals(SetupDecision.AskForMeta, decision)
    }

    @Test
    fun `a machine that cannot carry a setup is left out rather than treated as a conflict`() {
        val decision = decideFromStatus("h9", provenance(), speaksV3 = false)
        assertTrue(decision is SetupDecision.CannotCarry)
    }

    // MARK: - What the ancestry settles

    @Test
    fun `a machine holding a newer copy of the same setup is adopted without a prompt`() {
        val mine = provenance(hash = "h5", revision = 5)
        val theirs = provenance(hash = "h7", revision = 7, lineage = listOf("h6", "h5"))
        assertTrue(decideFromMeta(theirs, mine) is SetupDecision.Adopt)
    }

    @Test
    fun `a machine holding an ancestor is pushed to`() {
        val mine = provenance(hash = "h7", revision = 7, lineage = listOf("h6", "h5"))
        val theirs = provenance(hash = "h5", revision = 5)
        assertTrue(decideFromMeta(theirs, mine) is SetupDecision.Push)
    }

    @Test
    fun `two edits from the same base are a divergence, whatever the revisions say`() {
        // The case the whole design exists for. Both are revision 6, both were made from h5, and a
        // number comparison would call one of them newer and lose the other.
        val mine = provenance(hash = "h6a", revision = 6, lineage = listOf("h5"))
        val theirs = provenance(hash = "h6b", revision = 6, lineage = listOf("h5"))
        val decision = decideFromMeta(theirs, mine)
        assertTrue(decision is SetupDecision.Diverged)
    }

    @Test
    fun `a higher revision that is not a descendant is still a divergence`() {
        // The exact loss this rule exists to prevent: A edits 5 to 6a, B edits 5 to 6b and publishes,
        // A edits again to 7a. 7a is numerically newest and B's edit is not in its ancestry, so
        // taking it would drop B's work. It has to stop here.
        val mine = provenance(hash = "h7a", revision = 7, lineage = listOf("h6a", "h5"))
        val theirs = provenance(hash = "h6b", revision = 6, lineage = listOf("h5"))
        assertTrue(decideFromMeta(theirs, mine) is SetupDecision.Diverged)
    }

    @Test
    fun `a different setup is never merged`() {
        val mine = provenance(id = "setup-abc")
        val theirs = provenance(id = "setup-xyz", hash = "other")
        val decision = decideFromMeta(theirs, mine)
        assertTrue(decision is SetupDecision.DifferentSetup)
        assertTrue((decision as SetupDecision.DifferentSetup).reason.contains("setup-xyz"))
    }

    @Test
    fun `a copy with no identity at all is a legacy push and is simply replaced`() {
        val mine = provenance()
        val theirs = provenance(id = null, revision = 0, hash = "legacy")
        assertTrue(decideFromMeta(theirs, mine) is SetupDecision.Push)
    }

    @Test
    fun `a device with no setup takes whatever it is offered`() {
        assertTrue(decideFromMeta(provenance(), applied = null) is SetupDecision.Adopt)
    }

    @Test
    fun `the same bytes are in sync even when the revisions disagree`() {
        // Two devices that arrived at the same document by different routes are not in conflict.
        val mine = provenance(hash = "same", revision = 4)
        val theirs = provenance(hash = "same", revision = 9)
        assertEquals(SetupDecision.InSync, decideFromMeta(theirs, mine))
    }

    /**
     * The property that makes automatic writes safe: they only ever happen along descent, and
     * descent has a direction.
     */
    @Test
    fun `two devices can never take turns overwriting each other`() {
        val a = provenance(hash = "h6", revision = 6, lineage = listOf("h5"))
        val b = provenance(hash = "h5", revision = 5)

        // A pushes to B's machine, because B's copy is A's ancestor.
        assertTrue(decideFromMeta(b, a) is SetupDecision.Push)
        // From B's side the same pair is an adoption, never a push back.
        assertTrue(decideFromMeta(a, b) is SetupDecision.Adopt)
    }
}
