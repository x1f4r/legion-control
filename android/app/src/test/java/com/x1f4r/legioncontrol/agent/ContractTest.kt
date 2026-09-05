package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The contract's own grammars, which are what stops a document from writing a command line. */
class ContractTest {

    @Test
    fun `a token is what the contract says it is`() {
        listOf("t3", "legion-win", "a.b_c", "x1f4r@host", "C:\\Users\\me", "v1.2.3+build").forEach {
            assertTrue("$it should be a token", Contract.isToken(it))
        }
        listOf("", " ", "-leading", ".leading", "a b", "a;b", "a\$b", "a'b", "a\"b", "a|b").forEach {
            assertFalse("$it should not be a token", Contract.isToken(it))
        }
    }

    @Test
    fun `an operation id is lowercase and long enough to be one`() {
        val fresh = Contract.newOpId()
        assertTrue(fresh, Contract.isOpId(fresh))
        assertEquals(fresh.lowercase(), fresh)
        assertFalse(Contract.isOpId("SHORT"))
        assertFalse(Contract.isOpId("UPPERCASE-UUID-0000-0000"))
        assertFalse(Contract.isOpId("has spaces in it"))
        assertTrue(Contract.isOpId("a".repeat(64)))
        assertFalse(Contract.isOpId("a".repeat(65)))
    }

    @Test
    fun `two fresh operation ids are never the same`() {
        val ids = (1..200).map { Contract.newOpId() }.toSet()
        assertEquals(200, ids.size)
    }

    @Test
    fun `a duration is written the way the agent reads it`() {
        assertTrue(Contract.isDuration("30m"))
        assertTrue(Contract.isDuration("4h"))
        assertTrue(Contract.isDuration("2d"))
        assertFalse(Contract.isDuration("4 h"))
        assertFalse(Contract.isDuration("4hours"))
        assertFalse(Contract.isDuration("-4h"))
        assertEquals("2d", Contract.durationOf(2 * 86_400))
        assertEquals("4h", Contract.durationOf(4 * 3_600))
        assertEquals("90m", Contract.durationOf(5_400))
        assertEquals("1m", Contract.durationOf(0))
    }

    @Test
    fun `an unknown reason code is shown rather than flattened`() {
        // The rule that keeps a later agent honest on an older phone: a code this build has never
        // heard of is a failure whose own words are shown, never something cheerful.
        assertEquals("the schedule is off for this service", ReasonCode.describe("policy-off"))
        assertEquals("a brand new reason", ReasonCode.describe("a-brand-new-reason"))
        assertNull(ReasonCode.describe(null))
        assertNull(ReasonCode.describe(""))
        assertNull(ReasonCode.fromWire("a-brand-new-reason"))
    }

    @Test
    fun `only the busy reasons invite forcing`() {
        // Force bypasses the busy gate and nothing else. A policy decision, a held
        // lock or an unverifiable install must never grow a "do it anyway" button.
        assertTrue(ReasonCode.BUSY.isForceable)
        assertTrue(ReasonCode.BUSY_UNKNOWN.isForceable)
        listOf(
            ReasonCode.POLICY_OFF,
            ReasonCode.POLICY_PAUSED,
            ReasonCode.OUTSIDE_WINDOW,
            ReasonCode.LOCK_HELD,
            ReasonCode.OPERATION_IN_PROGRESS,
            ReasonCode.LATEST_UNKNOWN,
            ReasonCode.POSTCONDITION_FAILED,
            ReasonCode.CONFIG_INVALID,
        ).forEach { assertFalse(it.name, it.isForceable) }
    }

    @Test
    fun `being told to reboot is not the same as having rebooted`() {
        // The two "in progress" actions are not endings, and an app
        // that treated them as one would report a machine that never moved as restarted.
        assertFalse(OpAction.REBOOTING.isSettled)
        assertFalse(OpAction.SLEEPING.isSettled)
        assertFalse(OpAction.ACCEPTED.isSettled)
        assertFalse(OpAction.QUEUED.isSettled)
        assertTrue(OpAction.REBOOTED.isSettled)
        assertTrue(OpAction.SLEPT.isSettled)
        assertTrue(OpAction.UPDATED.isSettled)
        assertTrue(OpAction.FAILED.isSettled)
    }

    @Test
    fun `only the outcomes a machine can be seen in are observable`() {
        // A restart leaves nothing that can be told apart from a service that was already running,
        // so it is never inferred from watching. A boot, a sleep and an install can be.
        assertTrue(OpKind.BOOT.isObservable)
        assertTrue(OpKind.SLEEP.isObservable)
        assertTrue(OpKind.UPDATE.isObservable)
        assertFalse(OpKind.RESTART.isObservable)
        assertFalse(OpKind.RUN.isObservable)
    }
}
