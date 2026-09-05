package com.x1f4r.legioncontrol.data

import net.i2p.crypto.eddsa.EdDSAEngine
import net.i2p.crypto.eddsa.EdDSAPublicKey
import net.i2p.crypto.eddsa.spec.EdDSANamedCurveTable
import java.security.MessageDigest
import java.security.spec.X509EncodedKeySpec
import java.util.Base64

/**
 * The one key this app trusts to have signed a release.
 *
 * Pinned, verbatim, from `contract/release-public-key.pem`. There is deliberately no way to
 * configure it, no second key, and no path that skips the check. One pinned release public key and
 * never a per-configuration trust bypass: a downloaded key is not a key anybody trusts. A fork that ships its own builds replaces the constant below and rebuilds; that is
 * the whole of the story, and it is the only honest one, because a key an attacker can supply is not
 * a signature check, it is a signature-shaped ritual.
 *
 * Ed25519 verification goes through net.i2p.crypto:eddsa, which is already a dependency of the ssh
 * layer, rather than through `java.security.Signature.getInstance("Ed25519")`. That name exists only
 * from API 33 and this app's minimum is 30; a phone on 30 or 32 would otherwise be unable to check a
 * signature, which would mean either refusing every update or skipping the check on old phones, and
 * neither is acceptable.
 */
object Trust {

    /**
     * The release signing key, SPKI DER, base64.
     *
     * The body of `contract/release-public-key.pem` with the armour removed. Kept in this form
     * rather than as raw key bytes so that it can be compared against the file by eye.
     */
    private const val RELEASE_PUBLIC_KEY_SPKI_BASE64 =
        "MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg="

    /** How many bytes an Ed25519 signature is. Anything else is refused before any maths happens. */
    private const val SIGNATURE_BYTES = 64

    private val spkiDer: ByteArray by lazy {
        Base64.getDecoder().decode(RELEASE_PUBLIC_KEY_SPKI_BASE64)
    }

    private val publicKey: EdDSAPublicKey by lazy {
        EdDSAPublicKey(X509EncodedKeySpec(spkiDer))
    }

    /**
     * The fingerprint of the trusted key, for the diagnostics page.
     *
     * sha256 of the SPKI DER, which is the same bytes the PEM carries, so the value on screen can be
     * checked against `openssl pkey -pubin -in release-public-key.pem -outform DER | shasum -a 256`.
     */
    val keyFingerprint: String by lazy {
        MessageDigest.getInstance("SHA-256").digest(spkiDer)
            .joinToString("") { "%02x".format(it) }
    }

    /** A short form for a line of prose. Never used for a comparison. */
    val shortFingerprint: String get() = keyFingerprint.take(16)

    /**
     * Whether [signature] is this key's signature over exactly [content].
     *
     * [signature] is the detached `.sig` file's contents: base64 of the 64 raw signature bytes,
     * followed by one LF. Whitespace around it is tolerated because a file that has been through a
     * text editor is still the same signature; anything else is not.
     *
     * Returns false rather than throwing on every malformed input. A caller that gets false refuses
     * the artefact, which is the same thing it would do with an exception, and a verification
     * routine that can throw is a verification routine somebody eventually wraps in a try that
     * swallows it.
     */
    fun verifyDetachedSignature(content: ByteArray, signature: String): Boolean {
        val raw = decodeSignature(signature) ?: return false
        if (raw.size != SIGNATURE_BYTES) return false
        return try {
            val engine = EdDSAEngine(MessageDigest.getInstance(EdDSANamedCurveTable.ED_25519_CURVE_SPEC.hashAlgorithm))
            engine.initVerify(publicKey)
            engine.update(content)
            engine.verify(raw)
        } catch (_: Exception) {
            // A signature that cannot be checked is a signature that did not check out. There is no
            // third answer here and pretending there is would be the whole vulnerability.
            false
        }
    }

    /** The same, for a signature already in bytes. */
    fun verifyDetachedSignature(content: ByteArray, signature: ByteArray): Boolean =
        verifyDetachedSignature(content, String(signature, Charsets.UTF_8))

    private fun decodeSignature(signature: String): ByteArray? {
        val trimmed = signature.trim()
        if (trimmed.isEmpty()) return null
        return try {
            Base64.getDecoder().decode(trimmed)
        } catch (_: IllegalArgumentException) {
            // Some tools wrap base64 at 64 columns. Joining the lines is not being lenient about the
            // signature, only about the file it arrived in.
            try {
                Base64.getDecoder().decode(trimmed.filterNot { it == '\n' || it == '\r' || it == ' ' })
            } catch (_: IllegalArgumentException) {
                null
            }
        }
    }
}

/** Lowercase hex sha256 of some bytes, which is the form every hash in the contract takes. */
internal fun sha256Hex(bytes: ByteArray): String =
    MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }
