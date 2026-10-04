package com.x1f4r.legioncontrol.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.security.KeyStoreException

/**
 * When this phone's key is replaced, and when it is not.
 *
 * Replacing it cuts the phone off from every machine that trusted it, so it happens on exactly two
 * findings: no key file at all, or a file that can never be opened again. A keystore that did not
 * answer and a read that failed are neither, and each of these tests checks the file is untouched
 * afterwards, because "it threw" and "it threw after writing a new key" look the same from outside.
 */
class DeviceIdentityTest {

    @get:Rule
    val folder = TemporaryFolder()

    private val keys = KeyVaultTest.FakeKeys()
    private val vault = KeyVault(keys) {}
    private val keyFile get() = File(folder.root, "ssh/identity.ed25519")

    private fun identity(now: Long = NOW) = DeviceIdentity(keyFile, vault, { "legion-control@test" }, { now })

    private fun lostFiles(): List<String> =
        keyFile.parentFile?.list()?.filter { it.startsWith("identity.ed25519.lost-") }.orEmpty()

    @Test
    fun `the first run makes a key, and every later read is the same key`() = runBlocking {
        val first = identity().identity()
        assertTrue(first.authorizedKeysLine.startsWith("ssh-ed25519 "))
        assertTrue(first.authorizedKeysLine.endsWith(" legion-control@test"))

        val again = identity().identity()
        assertEquals(first.authorizedKeysLine, again.authorizedKeysLine)
        assertNull("a first key is not a changed key", identity().change())
        assertEquals(1, keys.created)
    }

    @Test
    fun `a keystore that did not answer is surfaced, retried, and replaces nothing`() = runBlocking {
        val original = identity().identity().authorizedKeysLine
        val before = keyFile.readBytes()

        keys.failure = KeyStoreException("keystore is not ready")
        val fresh = identity()
        try {
            fresh.identity()
            fail("an unanswered keystore has to be an error")
        } catch (expected: DeviceKeyUnavailable) {
            assertTrue(expected.message!!.contains("Nothing was replaced"))
        }
        assertArrayEquals(before, keyFile.readBytes())
        assertTrue(lostFiles().isEmpty())
        assertNull(fresh.change())

        // The same object asks again, and gets the key it always had.
        keys.failure = null
        assertEquals(original, fresh.identity().authorizedKeysLine)
        assertEquals(1, keys.created)
    }

    @Test
    fun `a key file that cannot be read is surfaced and left alone`() = runBlocking {
        keyFile.mkdirs()
        try {
            identity().identity()
            fail("a read that failed has to be an error")
        } catch (_: DeviceKeyUnavailable) {
        }
        assertTrue(keyFile.isDirectory)
        assertEquals(0, keys.created)
    }

    @Test
    fun `a wrapping key that is gone makes a new key, keeps the old file, and says so`() = runBlocking {
        val original = identity().identity().authorizedKeysLine
        val before = keyFile.readBytes()
        keys.key = null

        val replaced = identity(now = NOW + 1)
        val line = replaced.identity().authorizedKeysLine
        assertNotEquals(original, line)

        assertEquals(listOf("identity.ed25519.lost-${NOW + 1}"), lostFiles())
        assertArrayEquals("the old file is kept whole", before, File(keyFile.parentFile, lostFiles().single()).readBytes())

        val change = replaced.change()
        assertNotNull(change)
        assertEquals(NOW + 1, change!!.at)
        assertTrue(change.reason.contains("keystore"))

        // Read again, the new key is the key: the replacement happened once.
        assertEquals(line, identity(now = NOW + 2).identity().authorizedKeysLine)
        assertEquals(1, lostFiles().size)
    }

    @Test
    fun `a key that no longer authenticates the file makes a new key and says so`() = runBlocking {
        val original = identity().identity().authorizedKeysLine
        keys.key = KeyVaultTest.aesKey()

        val replaced = identity()
        assertNotEquals(original, replaced.identity().authorizedKeysLine)
        assertNotNull(replaced.change())
        assertEquals(1, lostFiles().size)
    }

    @Test
    fun `a change is reported until it is acknowledged`() = runBlocking {
        identity().identity()
        keys.key = null
        val replaced = identity()
        replaced.identity()
        assertNotNull(replaced.change())

        replaced.acknowledgeChange()
        assertNull(replaced.change())
        assertNull(identity().change())
    }

    @Test
    fun `two identities loading at once share one new key`() = runBlocking {
        // The screen, the debug bridge and a recreated activity each build their own. Without the
        // process-wide lock, two first runs each wrote a key and one of them kept using the one that
        // was no longer on disk.
        val identities = List(4) { identity() }
        val lines = identities.flatMap { identity ->
            List(4) { async(Dispatchers.IO) { identity.identity().authorizedKeysLine } }
        }.awaitAll()

        assertEquals(1, lines.toSet().size)
        assertEquals(1, keys.created)
        assertEquals(lines.first(), identity().identity().authorizedKeysLine)
    }

    private companion object {
        const val NOW = 1_700_000_000_000L
    }
}
