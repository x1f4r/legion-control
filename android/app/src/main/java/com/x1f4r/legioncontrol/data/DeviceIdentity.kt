package com.x1f4r.legioncontrol.data

import android.content.Context
import android.os.Build
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import net.i2p.crypto.eddsa.EdDSAPrivateKey
import net.i2p.crypto.eddsa.EdDSAPublicKey
import net.i2p.crypto.eddsa.spec.EdDSANamedCurveTable
import net.i2p.crypto.eddsa.spec.EdDSAPrivateKeySpec
import net.i2p.crypto.eddsa.spec.EdDSAPublicKeySpec
import net.schmizz.sshj.common.KeyType
import net.schmizz.sshj.userauth.keyprovider.KeyProvider
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.PublicKey
import java.security.SecureRandom
import java.util.Base64

/** This phone's ssh identity, in the two forms anything needs it in. */
class SshIdentity internal constructor(
    private val publicKey: EdDSAPublicKey,
    private val privateKey: EdDSAPrivateKey,
    private val comment: String,
) : KeyProvider {

    /** The line to paste into ~/.ssh/authorized_keys, exactly as ssh-keygen would have written it. */
    val authorizedKeysLine: String by lazy {
        "ssh-ed25519 " +
            Base64.getEncoder().encodeToString(wireBlob) +
            " " + comment
    }

    /** SHA256:... , the same fingerprint ssh-keygen -lf prints, so the two can be compared by eye. */
    val fingerprint: String by lazy {
        val digest = MessageDigest.getInstance("SHA-256").digest(wireBlob)
        "SHA256:" + Base64.getEncoder().withoutPadding().encodeToString(digest)
    }

    private val wireBlob: ByteArray by lazy { opensshBlob(publicKey.abyte) }

    override fun getPublic(): PublicKey = publicKey
    override fun getPrivate(): PrivateKey = privateKey
    override fun getType(): KeyType = KeyType.ED25519
}

/**
 * The ed25519 key this app authenticates with, generated on the phone on first run.
 *
 * It is generated here rather than shipped in the APK or copied off the Mac, and the reason is the
 * phone. A phone is the most losable thing the user owns, and a key that only this app has means
 * revoking it is deleting one line from authorized_keys on each system, instead of re-keying every
 * machine that trusted the Mac's key.
 *
 * The private half never leaves app private storage and is wrapped by a key in the Android keystore,
 * so a copy of the file on its own is not usable. There is deliberately no way to export it.
 */
class DeviceIdentity internal constructor(
    private val keyFile: File,
    private val vault: KeyVault,
    /** The comment at the end of the authorized_keys line. */
    private val comment: () -> String,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    constructor(context: Context) : this(
        keyFile = File(File(context.applicationContext.filesDir, "ssh"), "identity.ed25519"),
        vault = KeyVault.android,
        comment = ::deviceComment,
    )

    private val changeFile = File(keyFile.parentFile, "${keyFile.name}.changed")
    private val mutex = Mutex()

    @Volatile
    private var cached: SshIdentity? = null

    /**
     * Loads the key, generating it the first time. Cheap after the first call.
     *
     * Throws [DeviceKeyUnavailable] when the key is there and could not be read this time. Nothing
     * is cached then, so the next call reads it again.
     */
    suspend fun identity(): SshIdentity {
        cached?.let { return it }
        return mutex.withLock {
            cached ?: withContext(Dispatchers.IO) { loadOrCreate() }.also { cached = it }
        }
    }

    /**
     * When the key was last replaced because the old one could no longer be opened, until somebody
     * has said they saw it. Null when it never was.
     */
    suspend fun change(): KeyChange? = withContext(Dispatchers.IO) { synchronized(CREATION) { readChange() } }

    /** Stops reporting [change]. The key itself is not touched. */
    suspend fun acknowledgeChange() {
        withContext(Dispatchers.IO) { changeFile.delete() }
    }

    /**
     * Replaces this phone's key because a person asked for it, after it could not be read.
     *
     * The way out of a key that fails every time for a reason no retry changes: a wrapping key the
     * keystore damaged and still hands over, or one it lists and never hands over. This app never
     * calls it on its own, because it cuts the phone off from every machine that trusted the old key,
     * and that waits for somebody who has been told so. The file is read once more first, and a key
     * that reads now is kept. Otherwise the old file is set aside, as any replacement does, and the
     * new key is sealed with a new wrapping key, because the one there is may be the thing that broke.
     */
    suspend fun replaceUnreadable(): SshIdentity = mutex.withLock {
        cached ?: withContext(Dispatchers.IO) {
            synchronized(CREATION) { identityFrom(readOrReplace()) }
        }.also { cached = it }
    }

    private fun readOrReplace(): ByteArray {
        try {
            return seed()
        } catch (_: Exception) {
            // Still unreadable, which is what the person was told before they asked.
        }
        return try {
            if (!vault.isAbsent(keyFile)) {
                replace("reading it kept failing, and a new one was asked for", newWrappingKey = true)
            } else {
                // Nothing to set aside: none was ever written, or a replacement before this one
                // moved it and its note still says so.
                newSeed().also { vault.write(keyFile, it, newKey = true) }
            }
        } catch (failure: Exception) {
            throw DeviceKeyUnavailable(failure, replacing = true)
        }
    }

    /**
     * The seed on disk, or a new one, and only ever a new one on a finding that cannot change.
     *
     * Two findings make a new key. There is no file, which is the first run. Or there is a file and
     * it can never be opened again: the keystore has no key to open it with, or the key it has does
     * not authenticate it or can never be used again, which is what a keystore reset or storage
     * restored underneath the app leaves behind. The old private key is unrecoverable in all of
     * these, and failing forever would fail the one screen that shows the public key, which is the
     * screen the user needs to recover.
     *
     * Everything else is an error that may not happen next time: a read that failed, a keystore that
     * did not answer. That is thrown, and the file stays exactly as it was. The previous version
     * caught every exception here and wrote a new key over the old one, so a keystore that was busy
     * for a moment cut the phone off from every machine that trusted it.
     *
     * Held under one lock for the whole process, because a phone has one key file and the screen,
     * the debug bridge and a recreated activity each build their own [DeviceIdentity]. Two of them
     * finding no key at once would otherwise each write one, and the first would go on using a key
     * that is no longer the one on disk.
     */
    private fun loadOrCreate(): SshIdentity = synchronized(CREATION) {
        val seed = try {
            seed()
        } catch (failure: Exception) {
            throw DeviceKeyUnavailable(failure)
        }
        identityFrom(seed)
    }

    private fun seed(): ByteArray {
        var stored = vault.read(keyFile)
        if (stored is Unsealed.Lost && stored.keyMissing && vault.hasKey()) {
            // The keystore said a few times over that it had no key, and has one now. It was only
            // slow, and the file is read again with the key rather than set aside.
            stored = vault.read(keyFile)
        }
        return when (stored) {
            is Unsealed.Opened -> stored.secret.takeIf { it.size == SEED_BYTES }
                ?: replace("the file that holds it does not hold an ed25519 key")

            Unsealed.Absent -> newSeed().also { vault.write(keyFile, it) }
            is Unsealed.Lost -> replace(stored.reason)
        }
    }

    private fun identityFrom(seed: ByteArray): SshIdentity {
        val curve = EdDSANamedCurveTable.ED_25519_CURVE_SPEC
        val privateSpec = EdDSAPrivateKeySpec(seed, curve)
        val publicSpec = EdDSAPublicKeySpec(privateSpec.a, curve)
        return SshIdentity(
            publicKey = EdDSAPublicKey(publicSpec),
            privateKey = EdDSAPrivateKey(privateSpec),
            comment = comment(),
        )
    }

    /**
     * A new key in place of one that can never be opened again.
     *
     * The change is written down first, so a process that dies anywhere in here still tells the
     * user next time; the old file is moved aside rather than written over, under the time the note
     * carries; and only then is a new key written where it was. A note whose file was never moved is
     * not a change, and [readChange] drops it. The note is on the disk before the move, so a phone
     * that loses power just after it does not keep the move and lose what it was about.
     */
    private fun replace(reason: String, newWrappingKey: Boolean = false): ByteArray {
        val at = clock()
        changeFile.parentFile?.mkdirs()
        FileOutputStream(changeFile).use { out ->
            out.write("$at\n$reason\n".toByteArray())
            out.fd.sync()
        }
        vault.setAside(keyFile, at)
        return newSeed().also { vault.write(keyFile, it, newKey = newWrappingKey) }
    }

    /**
     * The note [replace] left, when the file it was about was in fact moved aside.
     *
     * The note is written before the move, so a move that failed or never happened leaves a note
     * about a key that is still the key. Reported, that would send the user to replace a line in
     * every authorized_keys for nothing; it is deleted instead.
     */
    private fun readChange(): KeyChange? {
        if (!changeFile.exists()) return null
        val lines = changeFile.readText().lines()
        val at = lines.firstOrNull()?.trim()?.toLongOrNull()
        // A note cut short is still a note that the key changed, and that is the part that matters:
        // it counts as long as anything was ever moved aside.
        val moved = if (at != null) {
            File(keyFile.parentFile, "${keyFile.name}.lost-$at").exists()
        } else {
            keyFile.parentFile?.list()?.any { it.startsWith("${keyFile.name}.lost-") } == true
        }
        if (!moved) {
            changeFile.delete()
            return null
        }
        return KeyChange(
            at = at ?: changeFile.lastModified(),
            reason = lines.getOrNull(1)?.trim().orEmpty(),
        )
    }

    /**
     * An ed25519 private key is 32 random bytes and the rest is derived, so there is no key pair
     * generator to go through and no parameter to get wrong.
     */
    private fun newSeed(): ByteArray = ByteArray(SEED_BYTES).also { SecureRandom().nextBytes(it) }

    private companion object {
        const val SEED_BYTES = 32

        /** One per process: see [loadOrCreate]. */
        val CREATION = Any()
    }
}

/**
 * This phone's key was replaced, because the one before could no longer be opened.
 *
 * Worth telling the user in so many words. Every machine that accepted the old key refuses the new
 * one until its line is added, and "not authorised" on its own reads like something changed on the
 * machine rather than on the phone.
 */
data class KeyChange(
    /** Milliseconds since the epoch. */
    val at: Long,
    /** Why the old key could not be opened, as a clause: "the Android keystore no longer has ...". */
    val reason: String,
)

/**
 * This phone's key is on the phone and could not be read just now.
 *
 * Nothing was replaced. The next attempt reads the same file again, and an error from the keystore
 * or the disk is far more often a moment than a loss. With [replacing], it was a replacement a
 * person asked for that did not finish, and the sentence says that instead.
 */
class DeviceKeyUnavailable(cause: Throwable, replacing: Boolean = false) : IOException(
    if (replacing) {
        "A new key for this phone could not be made just now. (${briefly(cause)})"
    } else {
        "This phone's key could not be read just now. Nothing was replaced, and it is read again on " +
            "the next try. (${briefly(cause)})"
    },
    cause,
)

private fun briefly(cause: Throwable): String =
    cause.message?.takeIf { it.isNotBlank() } ?: cause::class.java.simpleName

/** "legion-control@<maker>-<model>", which is how the line is recognised in authorized_keys. */
private fun deviceComment(): String {
    val device = "${Build.MANUFACTURER} ${Build.MODEL}"
        .trim()
        .replace(Regex("[^A-Za-z0-9._-]+"), "-")
        .trim('-')
        .ifEmpty { "android" }
    return "legion-control@$device"
}

/**
 * The ssh wire encoding of an ed25519 public key: the algorithm name and the 32 key bytes, each as a
 * length-prefixed string. This is what goes inside the base64 of an authorized_keys line, and what
 * gets hashed to produce a fingerprint.
 */
internal fun opensshBlob(rawPublicKey: ByteArray): ByteArray {
    val out = ByteArrayOutputStream()
    fun putString(bytes: ByteArray) {
        out.write((bytes.size ushr 24) and 0xFF)
        out.write((bytes.size ushr 16) and 0xFF)
        out.write((bytes.size ushr 8) and 0xFF)
        out.write(bytes.size and 0xFF)
        out.write(bytes)
    }
    putString("ssh-ed25519".toByteArray(Charsets.US_ASCII))
    putString(rawPublicKey)
    return out.toByteArray()
}
