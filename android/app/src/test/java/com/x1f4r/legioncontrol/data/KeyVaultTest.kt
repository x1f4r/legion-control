package com.x1f4r.legioncontrol.data

import android.security.keystore.KeyPermanentlyInvalidatedException
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.security.Key
import java.security.KeyStore
import java.security.KeyStoreException
import java.security.KeyStoreSpi
import java.security.cert.Certificate
import java.util.Collections
import java.util.Date
import java.util.Enumeration
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey

/**
 * What the vault calls a lost secret, and what it calls an error.
 *
 * The line between the two is the whole of the vault's safety. A lost secret is replaced; an error is
 * thrown and the file is left exactly as it was. The previous version put every exception on the
 * first side of that line, and made a fresh wrapping key whenever the keystore did not hand one
 * over, so a keystore that was busy for a moment replaced the phone's key for good.
 */
class KeyVaultTest {

    @get:Rule
    val folder = TemporaryFolder()

    /** A keystore whose answers each test sets, with a real AES key behind it. */
    internal class FakeKeys : WrappingKeys {
        var key: SecretKey? = null
        var failure: Exception? = null
        var creationFailure: Exception? = null

        /** Answers given before the real one, for a keystore that says "none" while it is busy. */
        val script = ArrayDeque<SecretKey?>()
        var finds = 0
        var created = 0

        override fun find(): SecretKey? {
            finds += 1
            failure?.let { throw it }
            if (script.isNotEmpty()) return script.removeFirst()
            return key
        }

        override fun create(): SecretKey {
            creationFailure?.let { throw it }
            created += 1
            return aesKey().also { key = it }
        }
    }

    /**
     * A key the keystore still hands over and will never use again, which Android reports when a
     * cipher is initialised with it.
     */
    internal class InvalidatedKey : SecretKey {
        override fun getAlgorithm(): String = "AES"
        override fun getFormat(): String = "RAW"
        override fun getEncoded(): ByteArray = throw KeyPermanentlyInvalidatedException()
    }

    private fun vault(keys: WrappingKeys, pauses: MutableList<Long> = mutableListOf()) =
        KeyVault(keys) { pauses += it }

    private fun file() = File(folder.root, "ssh/identity.ed25519")

    private val secret = ByteArray(32) { it.toByte() }

    @Test
    fun `a secret written is the secret read back, and nothing is left beside it`() {
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)

        val read = vault.read(file())
        assertTrue(read is Unsealed.Opened)
        assertArrayEquals(secret, (read as Unsealed.Opened).secret)
        assertEquals(listOf("identity.ed25519"), file().parentFile!!.list()!!.toList())
        assertEquals("the first write makes the one wrapping key", 1, keys.created)
    }

    @Test
    fun `no file is absent, and the keystore is not even asked`() {
        val keys = FakeKeys()
        assertSame(Unsealed.Absent, vault(keys).read(file()))
        assertEquals(0, keys.finds)
    }

    @Test
    fun `a wrapping key that is gone is believed only after the keystore said so more than once`() {
        val keys = FakeKeys()
        val pauses = mutableListOf<Long>()
        val vault = vault(keys, pauses)
        vault.write(file(), secret)
        keys.key = null
        keys.finds = 0
        pauses.clear()

        val read = vault.read(file())
        assertTrue(read is Unsealed.Lost)
        assertTrue((read as Unsealed.Lost).reason.contains("keystore"))
        assertTrue("the one finding a later look can still overturn", read.keyMissing)
        assertEquals(KeyVault.ABSENCE_CHECKS, keys.finds)
        assertEquals(KeyVault.ABSENCE_CHECKS - 1, pauses.size)
    }

    @Test
    fun `a write makes a wrapping key only after the keystore said more than once there is none`() {
        // The write after a lost secret looked once, and a keystore that was only slow to answer had
        // its key replaced under the same name, which is the only key that could open the old file.
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)
        val wrapping = keys.key
        keys.script.addLast(null)
        keys.script.addLast(null)

        vault.write(file(), ByteArray(32) { 7 })
        assertEquals("the key that answered third time is the key", 1, keys.created)
        assertSame(wrapping, keys.key)
        assertArrayEquals(ByteArray(32) { 7 }, (vault.read(file()) as Unsealed.Opened).secret)
    }

    @Test
    fun `a key the keystore says can never be used again is a lost secret`() {
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)
        keys.key = InvalidatedKey()

        val read = vault.read(file())
        assertTrue(read is Unsealed.Lost)
        assertTrue((read as Unsealed.Lost).reason.contains("never be used again"))
        assertFalse("the keystore had a key, and said it is dead", read.keyMissing)
    }

    @Test
    fun `a write over a key that can never be used again seals with a new one`() {
        // Otherwise the replacement of a secret lost this way fails on its own write, and so does
        // every first run after it: the phone would never have a key again.
        val keys = FakeKeys()
        val vault = vault(keys)
        keys.key = InvalidatedKey()

        vault.write(file(), secret)
        assertEquals(1, keys.created)
        assertArrayEquals(secret, (vault.read(file()) as Unsealed.Opened).secret)
    }

    @Test
    fun `a new wrapping key on request is made even while the keystore cannot answer`() {
        // The way out of a key that is listed and never handed over, and only when a person asked.
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)
        keys.failure = KeyStoreException("the keystore lists the key and did not hand it over")

        vault.write(file(), ByteArray(32) { 9 }, newKey = true)
        assertEquals(2, keys.created)
        keys.failure = null
        assertArrayEquals(ByteArray(32) { 9 }, (vault.read(file()) as Unsealed.Opened).secret)
    }

    @Test
    fun `a keystore that says none once and then finds the key has not lost it`() {
        // Recent Android answers "no such alias" for a lookup that failed. One look would have
        // replaced the phone's key over a keystore that was only busy.
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)
        keys.script.addLast(null)

        val read = vault.read(file())
        assertTrue(read is Unsealed.Opened)
        assertArrayEquals(secret, (read as Unsealed.Opened).secret)
    }

    @Test
    fun `a key that does not authenticate the file is a lost secret`() {
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)
        keys.key = aesKey()

        val read = vault.read(file())
        assertTrue(read is Unsealed.Lost)
    }

    @Test
    fun `a file too short to be sealed is a lost secret`() {
        file().parentFile!!.mkdirs()
        file().writeBytes(ByteArray(20))
        assertTrue(vault(FakeKeys()).read(file()) is Unsealed.Lost)
    }

    @Test
    fun `a keystore that cannot answer is an error, the file stays, and no key is made`() {
        val keys = FakeKeys()
        val vault = vault(keys)
        vault.write(file(), secret)
        val before = file().readBytes()
        keys.failure = KeyStoreException("keystore is not ready")

        try {
            vault.read(file())
            fail("a keystore that did not answer has to be an error")
        } catch (expected: KeyStoreException) {
            assertEquals("keystore is not ready", expected.message)
        }
        try {
            vault.write(file(), ByteArray(32))
            fail("a write may not go ahead without the keystore's answer")
        } catch (_: KeyStoreException) {
        }

        assertEquals("only the first write ever made a key", 1, keys.created)
        assertArrayEquals(before, file().readBytes())
    }

    @Test
    fun `a file that cannot be read is an error, not a lost secret`() {
        // A directory where the file should be: exists, and every read of it fails.
        file().mkdirs()
        try {
            vault(FakeKeys()).read(file())
            fail("a read that failed has to be an error")
        } catch (_: java.io.IOException) {
        }
        assertTrue(file().isDirectory)
    }

    @Test
    fun `setting a file aside keeps it whole under a new name`() {
        val vault = vault(FakeKeys())
        vault.write(file(), secret)
        val before = file().readBytes()

        val aside = vault.setAside(file(), now = 1_700_000_000_000L)
        assertEquals("identity.ed25519.lost-1700000000000", aside.name)
        assertFalse(file().exists())
        assertArrayEquals(before, aside.readBytes())
    }

    @Test
    fun `the keystore wrapper says none only when the alias is not listed`() {
        val spi = ScriptedSpi(listed = false, entry = null)
        var generated = 0
        val keys = KeystoreWrappingKeys(open = { ScriptedStore(spi) }, generate = { generated += 1; aesKey() })

        assertNull(keys.find())
        assertEquals("finding nothing makes nothing", 0, generated)

        val key = aesKey()
        spi.listed = true
        spi.entry = KeyStore.SecretKeyEntry(key)
        assertSame(key, keys.find())
    }

    @Test
    fun `a key that is listed and not handed over is an error, and no key is made over it`() {
        // The defect itself: getEntry answered null, and the old code generated a new key under the
        // alias every sealed file was written with.
        val spi = ScriptedSpi(listed = true, entry = null)
        var generated = 0
        val keys = KeystoreWrappingKeys(open = { ScriptedStore(spi) }, generate = { generated += 1; aesKey() })
        val vault = vault(keys)

        try {
            keys.find()
            fail("listed and not handed over has to be an error")
        } catch (_: KeyStoreException) {
        }
        try {
            vault.write(file(), secret)
            fail("a write may not make a key while one is listed")
        } catch (_: KeyStoreException) {
        }
        assertEquals(0, generated)
        assertFalse(file().exists())
    }

    /** A keystore that answers exactly what the test says. */
    private class ScriptedSpi(var listed: Boolean, var entry: KeyStore.Entry?) : KeyStoreSpi() {
        override fun engineContainsAlias(alias: String?): Boolean = listed
        override fun engineGetEntry(alias: String?, protParam: KeyStore.ProtectionParameter?): KeyStore.Entry? = entry
        override fun engineGetKey(alias: String?, password: CharArray?): Key? = (entry as? KeyStore.SecretKeyEntry)?.secretKey
        override fun engineIsKeyEntry(alias: String?): Boolean = listed
        override fun engineIsCertificateEntry(alias: String?): Boolean = false
        override fun engineSize(): Int = if (listed) 1 else 0
        override fun engineAliases(): Enumeration<String> = Collections.emptyEnumeration()
        override fun engineGetCertificateChain(alias: String?): Array<Certificate>? = null
        override fun engineGetCertificate(alias: String?): Certificate? = null
        override fun engineGetCreationDate(alias: String?): Date? = null
        override fun engineGetCertificateAlias(cert: Certificate?): String? = null
        override fun engineSetKeyEntry(alias: String?, key: Key?, password: CharArray?, chain: Array<out Certificate>?) = Unit
        override fun engineSetKeyEntry(alias: String?, key: ByteArray?, chain: Array<out Certificate>?) = Unit
        override fun engineSetCertificateEntry(alias: String?, cert: Certificate?) = Unit
        override fun engineDeleteEntry(alias: String?) = Unit
        override fun engineStore(stream: OutputStream?, password: CharArray?) = Unit
        override fun engineLoad(stream: InputStream?, password: CharArray?) = Unit
    }

    private class ScriptedStore(spi: KeyStoreSpi) : KeyStore(spi, null, "scripted") {
        init {
            load(null)
        }
    }

    companion object {
        fun aesKey(): SecretKey = KeyGenerator.getInstance("AES").apply { init(256) }.generateKey()
    }
}
