package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.HostKeyStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Base64

/**
 * The host key policy, which is three lines of logic that decide whether somebody in the middle of
 * the connection gets through.
 *
 * The change from the previous release: trust on first use is gone. An address with no key is
 * refused exactly as one with the wrong key is, because the very first connection is the only one an
 * attacker has a free hand on, and accepting whatever it offers is accepting them.
 */
class HostKeyTest {

    private fun blob(seed: Byte): String =
        Base64.getEncoder().encodeToString(ByteArray(32) { seed })

    @Test
    fun `a key that has been trusted before is accepted`() {
        val key = blob(1)
        assertEquals(HostKeyDecision.Trusted, decideHostKey(listOf(key), key))
    }

    @Test
    fun `an address with nothing trusted is a question, not an acceptance`() {
        val decision = decideHostKey(emptyList(), blob(1), canApprove = true)
        assertTrue(decision is HostKeyDecision.Refused)
        assertTrue(
            "nothing has changed, so it has to read differently from a changed key",
            (decision as HostKeyDecision.Refused).isFirstContact,
        )
        assertTrue(decision.trustedFingerprints.isEmpty())
        assertTrue(decision.canApprove)
        assertTrue(decision.offeredFingerprint.startsWith("SHA256:"))
    }

    @Test
    fun `a key that is not one of the trusted ones is refused and named`() {
        val decision = decideHostKey(listOf(blob(1), blob(2)), blob(3))
        assertTrue(decision is HostKeyDecision.Refused)
        val refused = decision as HostKeyDecision.Refused
        assertFalse(refused.isFirstContact)
        assertEquals(2, refused.trustedFingerprints.size)
        assertEquals(HostKeyStore.fingerprintOf(blob(3)), refused.offeredFingerprint)
    }

    @Test
    fun `any of several trusted keys is accepted, which is what a dual boot machine needs`() {
        // One LAN address, two systems, one host key each. Each of them is a question once, and
        // after that a boot switch is not an event.
        val trusted = listOf(blob(1), blob(2))
        assertEquals(HostKeyDecision.Trusted, decideHostKey(trusted, blob(1), canApprove = false))
        assertEquals(HostKeyDecision.Trusted, decideHostKey(trusted, blob(2), canApprove = false))
    }

    @Test
    fun `a third unknown key cannot evict either dual boot pin`() {
        val trusted = listOf(blob(1), blob(2))
        val decision = decideHostKey(trusted, blob(3), canApprove = false)
        assertTrue(decision is HostKeyDecision.Refused)
        assertFalse((decision as HostKeyDecision.Refused).canApprove)
        assertEquals(trusted.map(HostKeyStore::fingerprintOf), decision.trustedFingerprints)
    }

    @Test
    fun `a fingerprint is the one ssh-keygen prints`() {
        // SHA256, base64, no padding. Comparing it by eye against the machine is the whole point,
        // so the format has to be the one the machine's own tools produce.
        val fingerprint = HostKeyStore.fingerprintOf(blob(7))
        assertTrue(fingerprint.startsWith("SHA256:"))
        assertFalse("OpenSSH prints these without padding", fingerprint.endsWith("="))
    }

    @Test
    fun `an unreadable stored key does not crash the comparison`() {
        assertEquals("unreadable", HostKeyStore.fingerprintOf("not base64 !!"))
    }
}
