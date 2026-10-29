package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Deciding what to do about the setup copy a machine is holding (contract section 9.5).
 *
 * Every device is a peer, so two of them can edit while one is offline. Revision numbers cannot tell
 * "newer" from "diverged": both people edit revision 5 and both produce a 6, and a number comparison
 * would let the second silently erase the first. Ancestry can, because descent is a fact rather than
 * a count.
 *
 * Only two things happen without asking, and both are along strict descent. Everything else stops at
 * a person, which is the property this file is here to hold. The shared vectors pin every rule one
 * case at a time; these follow a few histories through it.
 */
class SetupLineageTest {

    private fun head(hash: String, lineage: List<String> = emptyList(), id: String? = "setup-abc") =
        SetupHead(hash, id, lineage)

    private fun mark(hash: String?, id: String? = "setup-abc") = ControllerMark(hash = hash, id = id)

    private fun decided(
        mine: SetupHead?,
        theirs: ControllerMark?,
        meta: Map<String, SetupMeta> = emptyMap(),
        contract: Int? = 3,
    ): SetupDecision = decide(mine, contract, theirs, meta).decision

    @Test
    fun `a machine carrying the same copy needs nothing`() {
        assertEquals(SetupDecision.IN_SYNC, decided(head("h5"), mark("h5")))
    }

    @Test
    fun `the same bytes are in sync even when the revisions disagree`() {
        // Two devices that arrived at the same document by different routes are not in conflict.
        assertEquals(SetupDecision.IN_SYNC, decided(head("same", listOf("h3")), mark("same")))
    }

    @Test
    fun `a machine carrying nothing, or a copy nobody claims, is pushed to without a prompt`() {
        assertEquals(SetupDecision.PUSH, decided(head("h5"), mark(null, id = null)))
        assertEquals(SetupDecision.PUSH, decided(head("h5"), mark("legacy", id = null)))
    }

    @Test
    fun `a machine carrying an ancestor of this copy is pushed to without a prompt`() {
        // Strict descent: my document was made from theirs, so nothing of theirs can be lost.
        assertEquals(SetupDecision.PUSH, decided(head("h6", listOf("h5", "h4")), mark("h5")))
    }

    @Test
    fun `anything else costs one round trip rather than a guess`() {
        assertEquals(SetupDecision.READ_META, decided(head("h5"), mark("unknown-hash")))
    }

    @Test
    fun `a machine holding a newer copy of the same setup is fetched without a prompt`() {
        val meta = mapOf("h7" to SetupMeta("setup-abc", listOf("h6", "h5")))
        val decision = decide(head("h5"), 3, mark("h7"), meta)
        assertEquals(SetupDecision.FETCH, decision.decision)
        assertEquals("the bytes have to be the ones the decision was about", "h7", decision.expect)
    }

    @Test
    fun `two edits from the same base are a divergence, whatever the revisions say`() {
        // The case the whole design exists for. Both were made from h5, and a number comparison
        // would call one of them newer and lose the other.
        val meta = mapOf("h6b" to SetupMeta("setup-abc", listOf("h5")))
        assertEquals(SetupDecision.DIVERGED, decided(head("h6a", listOf("h5")), mark("h6b"), meta))
    }

    @Test
    fun `a longer branch that does not descend from this copy is still a divergence`() {
        // A edits 5 to 6a, B edits 5 to 6b and publishes, A edits again to 7a. 7a is numerically
        // newest and B's edit is not in its ancestry, so taking it would drop B's work.
        val meta = mapOf("h6b" to SetupMeta("setup-abc", listOf("h5")))
        assertEquals(SetupDecision.DIVERGED, decided(head("h7a", listOf("h6a", "h5")), mark("h6b"), meta))
    }

    @Test
    fun `a different setup is never merged, even when its hash is in this copy's ancestry`() {
        assertEquals(SetupDecision.DIFFERENT_SETUP, decided(head("h6", listOf("h5")), mark("h5", id = "setup-xyz")))
        // What the machine filed about the hash outranks the status.
        val meta = mapOf("h9" to SetupMeta("setup-xyz", listOf("h6")))
        assertEquals(SetupDecision.DIFFERENT_SETUP, decided(head("h6"), mark("h9"), meta))
    }

    @Test
    fun `nothing is decided without a document of this device's own, or an agent that carries one`() {
        assertEquals(SetupDecision.UNKNOWN, decided(null, mark("h5")))
        assertEquals(SetupDecision.UNKNOWN, decided(head("h5", id = null), mark("h6")))
        assertEquals(SetupDecision.UNSUPPORTED, decided(head("h5"), mark("h6"), contract = 2))
        assertEquals(SetupDecision.UNSUPPORTED, decided(head("h5"), null))
    }

    /**
     * The property that makes automatic writes safe: they only ever happen along descent, and
     * descent has a direction.
     */
    @Test
    fun `two devices can never take turns overwriting each other`() {
        val a = head("h6", listOf("h5"))
        val b = head("h5")

        // A pushes to the machine holding B's copy, because B's copy is A's ancestor.
        assertEquals(SetupDecision.PUSH, decided(a, mark("h5")))
        // From B's side the same pair is a fetch, never a push back.
        assertEquals(SetupDecision.FETCH, decided(b, mark("h6"), mapOf("h6" to SetupMeta("setup-abc", listOf("h5")))))
    }
}
