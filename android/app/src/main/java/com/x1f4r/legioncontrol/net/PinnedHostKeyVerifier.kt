package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.HostKeyStore
import net.schmizz.sshj.common.Buffer
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import java.security.PublicKey
import java.util.Base64

/**
 * Pinned host keys, with no trust on first use.
 *
 * This used to write down whatever the first connection was offered and carry on. That is the
 * `accept-new` behaviour, and it is not good enough here: an unknown host requires an explicit trust
 * decision, not a silent accept. The difference matters most on exactly the connection where it used
 * to be silent, the very first one, which is the only connection an attacker in the middle has a
 * free hand on: after that the pin is doing its job.
 *
 * So an address with no keys and an address with the wrong key are both refused here, and both come
 * back to the user as a question with the fingerprint in it. They are told apart by
 * [Mismatch.isFirstContact], because the two questions read differently: the first is "this is what
 * that machine offered, is it right", and the second is "this is not what it offered last time".
 *
 * A mismatch is recorded rather than thrown, because sshj wants a boolean here and turns a false into
 * a transport exception of its own. The caller reads [mismatch] afterwards to tell that failure apart
 * from every other reason a connection can go wrong.
 */
class PinnedHostKeyVerifier(
    private val store: HostKeyStore,
    private val address: String,
) : HostKeyVerifier {

    data class Mismatch(
        val address: String,
        val trustedFingerprints: List<String>,
        val offeredFingerprint: String,
        /** The offered key itself, so that trusting it stores exactly what was seen, not a rescan. */
        val offeredKeyBlob: String,
        /** True when this address has never been trusted with anything, so nothing changed. */
        val isFirstContact: Boolean,
        /** False once every configured system already has an approved slot at this address. */
        val canApprove: Boolean,
    )

    @Volatile
    var mismatch: Mismatch? = null
        private set

    override fun verify(hostname: String, port: Int, key: PublicKey): Boolean {
        // The wire encoding, not the JCE encoding: it is what ssh itself hashes, so the fingerprint
        // this produces is the one ssh-keygen -lf prints for the same host key.
        val offered = Base64.getEncoder()
            .encodeToString(Buffer.PlainBuffer().putPublicKey(key).compactData)

        return when (val decision = decideHostKey(store.trusted(address), offered, canApprove = store.snapshot(address).canApprove)) {
            HostKeyDecision.Trusted -> true
            is HostKeyDecision.Refused -> {
                mismatch = Mismatch(
                    address = address,
                    trustedFingerprints = decision.trustedFingerprints,
                    offeredFingerprint = decision.offeredFingerprint,
                    offeredKeyBlob = offered,
                    isFirstContact = decision.isFirstContact,
                    canApprove = decision.canApprove,
                )
                false
            }
        }
    }

    /** Prefer already approved algorithms so algorithm negotiation cannot consume another OS group. */
    override fun findExistingAlgorithms(hostname: String, port: Int): List<String> =
        store.trusted(address).mapNotNull { runCatching { com.x1f4r.legioncontrol.data.HostIdentityKey.fromBlob(it).algorithm }.getOrNull() }.distinct()

}


/**
 * What to do about a host key, as a pure decision.
 *
 * Pulled out of the verifier so it can be checked without a keystore, an Android context or a
 * server: it is three lines of logic that decide whether an attacker in the middle gets through, and
 * "it looked right when I ran the app" is not a way to be sure of them.
 */
sealed interface HostKeyDecision {
    /** The offered key is one this address has been trusted with before. */
    data object Trusted : HostKeyDecision

    /** It is not, and the user has to be asked. Never resolved automatically, in either case. */
    data class Refused(
        val trustedFingerprints: List<String>,
        val offeredFingerprint: String,
        /** True when nothing has ever been trusted here, so nothing has changed. */
        val isFirstContact: Boolean,
        /** Whether the configured systems leave room for this additional explicit approval. */
        val canApprove: Boolean,
    ) : HostKeyDecision
}

/**
 * The whole of the host key policy.
 *
 * An address with no keys is refused exactly as one with the wrong key is. That is the difference
 * from `accept-new`, and it matters on the one connection where it used to be silent: the first,
 * which is the only one an attacker in the middle has a free hand on.
 */
fun decideHostKey(trusted: List<String>, offered: String, canApprove: Boolean = false): HostKeyDecision {
    if (offered in trusted) return HostKeyDecision.Trusted
    return HostKeyDecision.Refused(
        trustedFingerprints = trusted.map(HostKeyStore::fingerprintOf),
        offeredFingerprint = HostKeyStore.fingerprintOf(offered),
        isFirstContact = trusted.isEmpty(),
        canApprove = canApprove,
    )
}
