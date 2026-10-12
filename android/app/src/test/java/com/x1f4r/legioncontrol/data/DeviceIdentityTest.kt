package com.x1f4r.legioncontrol.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
    fun `a key file that could not even be looked up is surfaced, and no key is made`() = runBlocking {
        // File.exists() answered false for a look that failed, and that was a first run: a new
        // wrapping key, and a new key file written where the old one may well still be.
        File(folder.root, "ssh").writeText("not a directory")
        try {
            identity().identity()
            fail("a look that failed has to be an error")
        } catch (expected: DeviceKeyUnavailable) {
            assertTrue(expected.message!!.contains("Nothing was replaced"))
        }
        try {
            identity().replaceUnreadable()
            fail("even a replacement somebody asked for needs to know whether there is a file to set aside")
        } catch (_: DeviceKeyUnavailable) {
        }
        assertEquals(0, keys.created)
        assertEquals(0, keys.finds)
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
    fun `a wrapping key the keystore says can never be used again makes a new key and says so`() = runBlocking {
        // The keystore hands the key over and refuses every use of it. Thrown as an error, it failed
        // every attempt for good, and the screen only ever offered to try again.
        val original = identity().identity().authorizedKeysLine
        keys.key = KeyVaultTest.InvalidatedKey()

        val replaced = identity(now = NOW + 1)
        assertNotEquals(original, replaced.identity().authorizedKeysLine)
        assertEquals(listOf("identity.ed25519.lost-${NOW + 1}"), lostFiles())
        assertTrue(replaced.change()!!.reason.contains("never be used again"))
        assertEquals("the dead key is replaced, once", 2, keys.created)
        assertEquals(replaced.identity().authorizedKeysLine, identity(now = NOW + 2).identity().authorizedKeysLine)
    }

    @Test
    fun `a keystore that finds its key at the last look keeps the key it has`() = runBlocking {
        // Three answers of "none" make the file lost, and the key is looked for once more before
        // anything is moved. Found then, the absence was the keystore being slow.
        val original = identity().identity().authorizedKeysLine
        repeat(KeyVault.ABSENCE_CHECKS) { keys.script.addLast(null) }

        val fresh = identity(now = NOW + 1)
        assertEquals(original, fresh.identity().authorizedKeysLine)
        assertTrue(lostFiles().isEmpty())
        assertNull(fresh.change())
        assertEquals(1, keys.created)
    }

    @Test
    fun `a note whose file was never moved aside is not a change`() = runBlocking {
        // The note is written before the move. A move that failed, or a process that died between
        // the two, leaves the old file in place and a note about a key that is still the key.
        val original = identity().identity().authorizedKeysLine
        File(keyFile.parentFile, "identity.ed25519.changed").writeText("${NOW + 5}\nthe keystore was slow\n")

        val fresh = identity()
        assertNull(fresh.change())
        assertFalse(File(keyFile.parentFile, "identity.ed25519.changed").exists())
        assertEquals(original, fresh.identity().authorizedKeysLine)
    }

    @Test
    fun `a replacement whose new key could not be written still says the key changed`() = runBlocking {
        identity().identity()
        keys.key = null
        keys.creationFailure = KeyStoreException("keystore is not ready")
        try {
            identity(now = NOW + 1).identity()
            fail("a new key that could not be sealed has to be an error")
        } catch (_: DeviceKeyUnavailable) {
        }

        // The old file is aside, so whatever key comes next is a new one, and the note stands.
        keys.creationFailure = null
        val next = identity(now = NOW + 2)
        next.identity()
        assertEquals(NOW + 1, next.change()!!.at)
    }

    @Test
    fun `a key that keeps failing to read is replaced when the user asks, with a new wrapping key`() = runBlocking {
        val original = identity().identity().authorizedKeysLine
        keys.failure = KeyStoreException("the keystore lists the key and did not hand it over")
        val stuck = identity(now = NOW + 1)
        try {
            stuck.identity()
            fail("an unreadable key is an error until somebody asks for a new one")
        } catch (_: DeviceKeyUnavailable) {
        }
        assertTrue("nothing is replaced on the app's own say", lostFiles().isEmpty())

        val line = stuck.replaceUnreadable().authorizedKeysLine
        assertNotEquals(original, line)
        assertEquals(line, stuck.identity().authorizedKeysLine)
        assertEquals(listOf("identity.ed25519.lost-${NOW + 1}"), lostFiles())
        assertEquals(2, keys.created)
        assertTrue(stuck.change()!!.reason.contains("asked for"))

        // A keystore that has the new key under its name hands that one over from now on.
        keys.failure = null
        assertEquals(line, identity(now = NOW + 2).identity().authorizedKeysLine)
    }

    @Test
    fun `asking for a new key when the old one reads again keeps the old one`() = runBlocking {
        val original = identity().identity().authorizedKeysLine
        val fresh = identity(now = NOW + 1)
        assertEquals(original, fresh.replaceUnreadable().authorizedKeysLine)
        assertTrue(lostFiles().isEmpty())
        assertNull(fresh.change())
        assertEquals(1, keys.created)
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
