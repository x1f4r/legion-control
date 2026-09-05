package com.x1f4r.legioncontrol.data

import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.util.Base64
import org.junit.Assert.*
import org.junit.Test

class HostIdentitiesTest {
    private val address = "192.168.178.20:2222"
    private val systems = listOf(HostIdentitySystem("windows"), HostIdentitySystem("linux"))
    private fun key(algorithm: String, seed: Int): String {
        val bytes = ByteArrayOutputStream()
        DataOutputStream(bytes).use { out -> out.writeInt(algorithm.length); out.writeBytes(algorithm); out.writeInt(seed) }
        return Base64.getEncoder().encodeToString(bytes.toByteArray())
    }
    private fun approve(state: HostIdentities, blob: String, id: String) = state.approve(address, blob, emptyList(), HostTrustApproval(state.revision, systems, id))

    @Test fun `two OS groups with disjoint algorithms refuse a third unknown key`() {
        val windows = key("ssh-ed25519", 1)
        val linux = key("ecdsa-sha2-nistp256", 2)
        val first = approve(HostIdentities(), windows, "windows")
        assertTrue(first.snapshot(address, emptyList()).canApprove)
        val second = approve(first, linux, "linux")
        val snapshot = second.snapshot(address, emptyList())
        assertFalse(snapshot.canApprove)
        assertEquals(setOf(windows, linux), snapshot.trusted.toSet())
        assertThrows(IllegalArgumentException::class.java) { approve(second, key("ssh-rsa", 3), "linux") }
        assertEquals(2, snapshot.systems.size)
        assertTrue(second.snapshot("192.168.178.20:22", emptyList()).trusted.isEmpty())
    }

    @Test fun `legacy multiple algorithms belong to one explicitly assigned OS`() {
        val a = key("ssh-ed25519", 1)
        val b = key("ssh-rsa", 2)
        val offered = key("ssh-ed25519", 3)
        val legacy = listOf(a, b)
        assertEquals(legacy, HostIdentities().snapshot(address, legacy).trusted)
        assertThrows(IllegalArgumentException::class.java) {
            HostIdentities().approve(address, offered, legacy, HostTrustApproval(0, systems, "linux"))
        }
        val approved = HostIdentities().approve(address, offered, legacy,
            HostTrustApproval(0, systems, "linux", mapOf(a to "windows", b to "windows")))
        assertEquals(2, approved.snapshot(address, legacy).systems.first().keys.size)
        assertFalse(approved.snapshot(address, legacy).canApprove)
        assertEquals(3, approved.snapshot(address, legacy).trusted.size)
    }

    @Test fun `stale approvals cannot fill an occupied group or lose unrelated pins`() {
        val first = approve(HostIdentities(), key("ssh-ed25519", 1), "windows")
        assertThrows(IllegalArgumentException::class.java) {
            first.approve(address, key("ssh-ed25519", 2), emptyList(), HostTrustApproval(0, systems, "linux"))
        }
        assertEquals(first, first.approve(address, key("ssh-ed25519", 1), emptyList(), HostTrustApproval(0, systems, "windows")))
        assertEquals(1, first.snapshot(address, emptyList()).trusted.size)
    }

    @Test fun `shared systems changes do not grant groups or erase keys`() {
        val first = approve(HostIdentities(), key("ssh-ed25519", 1), "windows")
        assertThrows(IllegalArgumentException::class.java) {
            first.approve(address, key("ssh-ed25519", 2), emptyList(), HostTrustApproval(first.revision, systems + HostIdentitySystem("third"), "third"))
        }
        assertThrows(IllegalArgumentException::class.java) {
            first.approve(address, key("ssh-ed25519", 2), emptyList(), HostTrustApproval(first.revision, listOf(systems.last()), "linux"))
        }
        assertEquals(1, first.snapshot(address, emptyList()).trusted.size)
    }

    @Test fun `unknown additional algorithm cannot silently join occupied OS`() {
        val first = approve(HostIdentities(), key("ssh-ed25519", 1), "windows")
        assertThrows(IllegalArgumentException::class.java) { approve(first, key("ssh-rsa", 2), "windows") }
        assertEquals(1, first.snapshot(address, emptyList()).systems.first().keys.size)
    }
    @Test fun `separate local management can reserve another OS without approving a key`() {
        val first = approve(HostIdentities(), key("ssh-ed25519", 1), "windows")
        val second = approve(first, key("ssh-rsa", 2), "linux")
        val configured = second.configureSystems(address, systems + HostIdentitySystem("recovery"), second.revision)
        assertEquals(second.snapshot(address, emptyList()).trusted, configured.snapshot(address, emptyList()).trusted)
        assertTrue(configured.snapshot(address, emptyList()).canApprove)
        assertEquals(emptyList<HostIdentityKey>(), configured.snapshot(address, emptyList()).systems.last().keys)
        assertThrows(IllegalArgumentException::class.java) { configured.configureSystems(address, systems, configured.revision) }
    }

}
