package com.x1f4r.legioncontrol.data

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.nio.channels.FileChannel
import java.nio.file.StandardOpenOption
import java.security.KeyStore
import java.security.KeyStoreException
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** What opening a sealed file found. */
internal sealed interface Unsealed {
    /** The secret, unwrapped and authenticated. */
    class Opened(val secret: ByteArray) : Unsealed

    /** There is no file: nothing was ever written here. */
    data object Absent : Unsealed

    /**
     * There is a file, and it can never be opened again. [reason] says why, for a person.
     *
     * Only findings that are permanent get here: the keystore has no key to open it with, the key it
     * has does not authenticate it or can never be used again, or the file is too short to have been
     * written by [KeyVault]. A keystore that fails to answer, or a read that fails, is none of these
     * and is thrown instead, because the file is still good and the next attempt may well open it.
     *
     * [keyMissing] is the first of those, and the one a moment can still prove wrong: the keystore
     * only said, a few times over, that it has no key. A caller asks [KeyVault.hasKey] once more
     * before it moves anything, and reads the file again when the key has turned up.
     */
    data class Lost(val reason: String, val keyMissing: Boolean = false) : Unsealed
}

/**
 * Where the key that wraps every sealed file lives. The Android keystore, except in tests.
 *
 * Two questions and nothing else, because the difference between them is the whole of the safety
 * here: a key that is not there may be made, and a key that could not be looked up may not.
 */
internal interface WrappingKeys {
    /**
     * The wrapping key, or null when the keystore has no entry for it.
     *
     * Throws when the keystore cannot say. That is never the same answer as no.
     */
    fun find(): SecretKey?

    /**
     * Makes the wrapping key, in place of whatever is under its name.
     *
     * Only ever called when the keystore has said firmly that there is none, when the one it has can
     * never be used again, or when a person asked for a new key.
     */
    fun create(): SecretKey
}

/**
 * The wrapping key, as an AES-GCM key in the Android keystore that never leaves it.
 *
 * Not bound to user authentication on purpose, because the app has to be able to reach a machine
 * from a widget or a background refresh without a fingerprint first.
 */
internal class KeystoreWrappingKeys(
    private val open: () -> KeyStore = { KeyStore.getInstance(PROVIDER).apply { load(null) } },
    private val generate: () -> SecretKey = ::generateInKeystore,
) : WrappingKeys {

    override fun find(): SecretKey? {
        val store = open()
        if (!store.containsAlias(ALIAS)) return null
        // Listed and then not handed over is the keystore failing, not the key being gone. Making a
        // key here would put a new one under the alias every sealed file was written with, which is
        // the one thing in this file that can never be undone, so it is an error and the caller
        // tries again later.
        val entry = store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry
            ?: throw KeyStoreException("the keystore lists $ALIAS and did not hand it over")
        return entry.secretKey
    }

    override fun create(): SecretKey = generate()

    private companion object {
        const val PROVIDER = "AndroidKeyStore"
        const val ALIAS = "legion-control-secret-wrap"

        fun generateInKeystore(): SecretKey {
            val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER)
            generator.init(
                KeyGenParameterSpec.Builder(
                    ALIAS,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(256)
                    .setRandomizedEncryptionRequired(true)
                    .build(),
            )
            return generator.generateKey()
        }
    }
}

/**
 * Wraps a small secret in a key that lives somewhere a copy of the file does not reach.
 *
 * App private storage already keeps other apps out. This adds the case app private storage does not
 * cover: a phone whose storage is read without the phone being unlocked, and a copy of the file that
 * ends up somewhere the wrapping key does not exist.
 *
 * The rule this class is built around is that an error is never evidence. A secret is only ever
 * reported [Unsealed.Lost] on a finding that cannot change, and a wrapping key is only ever made when
 * the keystore said there is none, said the one it has can never be used again, or a person asked
 * for one. Everything else is thrown, the file stays exactly as it was, and the next attempt reads
 * it again. The previous version caught every exception on the way in and
 * made a new key on the way out, so one keystore hiccup replaced the phone's ssh key for good.
 */
internal class KeyVault(
    private val keys: WrappingKeys,
    /** Waits between two looks at the keystore. Replaced in tests, and only there. */
    private val pause: (Long) -> Unit = Thread::sleep,
) {

    /** Reads and unwraps [file]. Throws when the answer could still change: see [Unsealed.Lost]. */
    fun read(file: File): Unsealed {
        if (!file.exists()) return Unsealed.Absent
        val sealed = file.readBytes()
        // Every write is a complete file renamed into place, so a file too short to hold a nonce and
        // a tag was never written by this class, and no key will ever open it.
        if (sealed.size < IV_BYTES + TAG_BITS / 8) {
            return Unsealed.Lost("the file that holds it is too short to be a sealed key")
        }
        val key = confirmedKey()
            ?: return Unsealed.Lost("the Android keystore no longer has the key that protected it", keyMissing = true)

        val cipher = Cipher.getInstance(TRANSFORMATION)
        try {
            cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_BITS, sealed, 0, IV_BYTES))
        } catch (_: KeyPermanentlyInvalidatedException) {
            // The keystore's own word that this key will never be used again, for anything. That is
            // as final as a tag that does not verify, and asking again does not change it.
            return Unsealed.Lost("the Android keystore says the key that protected it can never be used again")
        }
        return try {
            Unsealed.Opened(cipher.doFinal(sealed, IV_BYTES, sealed.size - IV_BYTES))
        } catch (_: AEADBadTagException) {
            // The tag is computed over the file with the key. A key that does not verify it is not
            // the key that wrote it, and that does not get better by asking again.
            Unsealed.Lost("the key that protected it no longer opens it")
        }
    }

    /**
     * Whether the keystore has the wrapping key right now. One look, and it throws when the keystore
     * cannot say.
     *
     * For the last look before a secret that was [Unsealed.Lost] for want of a key is replaced: the
     * keystore said several times that there is none, and a key that answers now was only slow.
     */
    fun hasKey(): Boolean = keys.find() != null

    /**
     * Wraps [secret] and writes it to [file], creating the parent directory.
     *
     * Sealed with the wrapping key there is, and a new one is made only when the keystore has said
     * there is none as many times as [read] needs before it calls a secret lost. Making one replaces
     * whatever is under its name, so one look is not enough here either: a key that was only slow to
     * answer would be written over, and with it the only key that could open the file it seals. With
     * [newKey] a new wrapping key is made whatever the keystore says, which is for a person replacing
     * a key that cannot be read, and for nothing else.
     *
     * The bytes are on the disk before the name points at them, so a phone that loses power at any
     * moment is left with the old file or the new one and never with a name that points at nothing.
     */
    fun write(file: File, secret: ByteArray, newKey: Boolean = false) {
        val cipher = try {
            encrypting(if (newKey) newWrappingKey() else wrappingKey())
        } catch (_: KeyPermanentlyInvalidatedException) {
            // The key under the name can never be used again, for this or for anything else, so a new
            // one in its place takes nothing away that could still open a file.
            encrypting(newWrappingKey())
        }
        val body = cipher.doFinal(secret)
        // The IV is generated by the keystore and is required to be unique per encryption, so it is
        // written in front of the ciphertext rather than stored separately.
        val iv = cipher.iv
        if (iv.size != IV_BYTES) throw IOException("expected a $IV_BYTES byte GCM nonce, got ${iv.size}")

        val directory = file.parentFile ?: throw IOException("${file.name} has no directory")
        directory.mkdirs()
        // Written beside the target and moved into place, so a process that dies halfway through
        // cannot leave a truncated key file that looks valid until the first connection fails.
        val staging = File(directory, "${file.name}.new")
        FileOutputStream(staging).use { out ->
            out.write(iv + body)
            out.fd.sync()
        }
        if (!staging.renameTo(file)) throw IOException("could not move ${staging.name} into place")
        syncDirectory(directory)
    }

    /**
     * Moves a file that can no longer be opened out of the way, and keeps it.
     *
     * Kept rather than deleted so that replacing a key is never also destroying one: the moved file
     * shows what was there and when, for anyone working out why this phone's key changed. It is not
     * a way back. The key that could have opened it is gone, unusable or never matched it, and a
     * wrapping key never leaves the keystore to be tried anywhere else.
     */
    fun setAside(file: File, now: Long): File {
        val aside = File(file.parentFile, "${file.name}.lost-$now")
        if (!file.renameTo(aside)) throw IOException("could not move ${file.name} aside")
        syncDirectory(file.parentFile)
        return aside
    }

    /**
     * The wrapping key, or null once the keystore has said a few times, a moment apart, that it has
     * none.
     *
     * Asked more than once because the platform does not keep "no such key" and "could not look"
     * apart: on recent Android versions the keystore answers that an alias is absent whenever the
     * lookup itself fails. One look would turn a keystore that was briefly busy into a lost key.
     */
    private fun confirmedKey(): SecretKey? {
        repeat(ABSENCE_CHECKS) { check ->
            keys.find()?.let { return it }
            if (check < ABSENCE_CHECKS - 1) pause(ABSENCE_PAUSE_MILLIS)
        }
        return null
    }

    /** The wrapping key there is, or a new one when the keystore is sure there is none. */
    private fun wrappingKey(): SecretKey = synchronized(CREATION) { confirmedKey() ?: keys.create() }

    private fun newWrappingKey(): SecretKey = synchronized(CREATION) { keys.create() }

    private fun encrypting(key: SecretKey): Cipher =
        Cipher.getInstance(TRANSFORMATION).apply { init(Cipher.ENCRYPT_MODE, key) }

    /**
     * Makes a rename durable as well as atomic. Best effort: the rename has already happened, and a
     * directory that cannot be opened for syncing is no reason to report the write as failed.
     */
    private fun syncDirectory(directory: File?) {
        directory ?: return
        runCatching {
            FileChannel.open(directory.toPath(), StandardOpenOption.READ).use { it.force(true) }
        }
    }

    companion object {
        private const val TRANSFORMATION = "AES/GCM/NoPadding"
        private const val IV_BYTES = 12
        private const val TAG_BITS = 128

        /** How many times the keystore has to say a key is gone before that is believed. */
        internal const val ABSENCE_CHECKS = 3
        private const val ABSENCE_PAUSE_MILLIS = 500L

        /**
         * Held while a wrapping key is looked up and, when there is none, made. One per process, so
         * two callers that both find no key cannot both make one, and the second replace the first.
         */
        private val CREATION = Any()

        /** The vault every sealed file on the phone uses. */
        internal val android: KeyVault by lazy { KeyVault(KeystoreWrappingKeys()) }
    }
}
