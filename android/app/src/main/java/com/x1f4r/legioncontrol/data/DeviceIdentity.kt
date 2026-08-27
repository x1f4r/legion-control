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
class DeviceIdentity(context: Context) {
    private val appContext = context.applicationContext
    private val keyFile = File(File(appContext.filesDir, "ssh"), "identity.ed25519")
    private val mutex = Mutex()

    @Volatile
    private var cached: SshIdentity? = null

    /** Loads the key, generating it the first time. Cheap after the first call. */
    suspend fun identity(): SshIdentity {
        cached?.let { return it }
        return mutex.withLock {
            cached ?: withContext(Dispatchers.IO) { loadOrCreate() }.also { cached = it }
        }
    }

    private fun loadOrCreate(): SshIdentity {
        val seed = storedSeed() ?: newSeed().also { KeyVault.write(keyFile, it) }

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
     * The stored seed, or null if there is nothing usable on disk.
     *
     * Null covers "never generated" and "there is a file and it cannot be turned back into a key",
     * and they are deliberately the same answer. The second case happens when the Android keystore
     * entry that wraps the file is gone: a restore onto a new phone, storage cleared underneath the
     * app, a keystore reset. The old private key is unrecoverable in every one of those, so the only
     * question is whether the app generates a new one or refuses to work forever.
     *
     * It has to generate. Failing here does not fail one call, it fails every call including the
     * screen that shows the public key, which is the one screen the user needs in order to recover.
     * A fresh key lands the app in "this key is not authorised yet", which is a state it already
     * knows how to explain and the user already knows how to fix.
     */
    private fun storedSeed(): ByteArray? {
        val seed = try {
            KeyVault.read(keyFile)
        } catch (_: Exception) {
            null
        } ?: return null

        if (seed.size != SEED_BYTES) return null
        return seed
    }

    /**
     * An ed25519 private key is 32 random bytes and the rest is derived, so there is no key pair
     * generator to go through and no parameter to get wrong.
     */
    private fun newSeed(): ByteArray = ByteArray(SEED_BYTES).also { SecureRandom().nextBytes(it) }

    private fun comment(): String {
        val device = "${Build.MANUFACTURER} ${Build.MODEL}"
            .trim()
            .replace(Regex("[^A-Za-z0-9._-]+"), "-")
            .trim('-')
            .ifEmpty { "android" }
        return "legion-control@$device"
    }

    private companion object {
        const val SEED_BYTES = 32
    }
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
