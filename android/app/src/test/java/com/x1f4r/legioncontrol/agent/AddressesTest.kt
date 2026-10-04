package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.HostIdentities
import com.x1f4r.legioncontrol.data.HostIdentitySystem
import com.x1f4r.legioncontrol.data.HostTrustApproval
import com.x1f4r.legioncontrol.data.HostTrustSnapshot
import com.x1f4r.legioncontrol.data.approve
import com.x1f4r.legioncontrol.data.configureSystems
import com.x1f4r.legioncontrol.net.PinnedHostKeyVerifier
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.security.KeyPairGenerator
import java.util.Base64

/**
 * The dial grammar where it guards a host key (T5).
 *
 * The shared vectors pin which hosts, users and ports pass. What is checked here is that a value
 * that fails can never become a pin: the key store refuses to write one, and the verifier refuses
 * the key without offering it for approval.
 */
class AddressesTest {

    private fun blob(seed: Int): String {
        val bytes = ByteArrayOutputStream()
        DataOutputStream(bytes).use { out ->
            out.writeInt("ssh-ed25519".length)
            out.writeBytes("ssh-ed25519")
            out.writeInt(seed)
        }
        return Base64.getEncoder().encodeToString(bytes.toByteArray())
    }

    @Test
    fun `a pin address is a checked host and a port`() {
        listOf("192.0.2.10:22", "desk-tower.local:2222", "2001:db8::10:22", "::ffff:192.0.2.1:22").forEach {
            assertTrue(it, Addresses.isPinnable(it))
        }
        listOf(
            "victim.example,*:22", "*.example:22", "desk-tower.local", "desk-tower.local:", ":22",
            "desk-tower.local:022", "desk-tower.local:0", "desk-tower.local:65536", "1234:22", "[::1]:22",
            "host name:22",
        ).forEach { assertFalse(it, Addresses.isPinnable(it)) }
    }

    @Test
    fun `an endpoint is dialled only when host, user and port all pass`() {
        assertTrue(Addresses.isDialable("desk-tower.local", "me", 22))
        assertTrue(Addresses.isDialable("100.64.0.10", "AzureAD\\me", 22))
        assertFalse(Addresses.isDialable("desk-tower.local", "-oProxyCommand=x", 22))
        assertFalse(Addresses.isDialable("desk-tower.local.", "me", 22))
        assertFalse(Addresses.isDialable("desk-tower.local", "me", 0))
    }

    @Test
    fun `no key is ever written down for an address that was not checked`() {
        val systems = listOf(HostIdentitySystem("linux"))
        val approval = HostTrustApproval(0, systems, "linux")
        assertThrows(IllegalArgumentException::class.java) {
            HostIdentities().approve("victim.example,*:22", blob(1), emptyList(), approval)
        }
        assertThrows(IllegalArgumentException::class.java) {
            HostIdentities().configureSystems("*.example:22", systems, 0)
        }
        val pinned = HostIdentities().approve("192.0.2.10:22", blob(1), emptyList(), approval)
        assertEquals(listOf("192.0.2.10:22"), pinned.endpoints.map { it.address })
    }

    @Test
    fun `the verifier refuses an unchecked address without offering its key for approval`() {
        val key = KeyPairGenerator.getInstance("RSA").apply { initialize(1024) }.generateKeyPair().public
        val nothingTrusted = { _: String -> HostTrustSnapshot(0, emptyList(), emptyList()) }

        val unchecked = PinnedHostKeyVerifier(nothingTrusted, "victim.example,*:22")
        assertFalse(unchecked.verify("victim.example,*", 22, key))
        assertNull("nothing to approve, so no question is asked", unchecked.mismatch)

        val checked = PinnedHostKeyVerifier(nothingTrusted, "192.0.2.10:22")
        assertFalse(checked.verify("192.0.2.10", 22, key))
        assertTrue(checked.mismatch!!.isFirstContact)
    }
}
