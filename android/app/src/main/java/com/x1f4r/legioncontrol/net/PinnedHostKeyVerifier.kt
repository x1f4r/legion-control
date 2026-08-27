package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.HostKeyStore
import net.schmizz.sshj.common.Buffer
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import java.security.PublicKey
import java.util.Base64

/**
 * Trust on first use, checked against every key the address has ever been trusted with.
 *
 * The first key an address offers is written down, and every key after that has to match one that
 * has been trusted before. That is weaker than checking a fingerprint out of band, and it is the
 * honest ceiling for a phone app whose whole point is that it was set up in one tap: what it does
 * buy is that an address which has been connected to before cannot quietly become a different
 * machine. The set rather than a single pin is for the LAN address, which two systems share: each
 * one's key is a question once, and after that a boot switch is not an event.
 *
 * A mismatch is recorded rather than thrown, because sshj wants a boolean here and turns a false into
 * a transport exception of its own. The caller reads [mismatch] afterwards to tell that failure apart
 * from every other reason a connection can go wrong.
 */
class PinnedHostKeyVerifier(
    private val store: HostKeyStore,
    private val address: String,
    /** How many keys this address may hold, which is the endpoint's knowledge, not the store's. */
    private val keyCapacity: Int,
) : HostKeyVerifier {

    data class Mismatch(
        val address: String,
        val trustedFingerprints: List<String>,
        val offeredFingerprint: String,
        /** The offered key itself, so that trusting it stores exactly what was seen, not a rescan. */
        val offeredKeyBlob: String,
    )

    @Volatile
    var mismatch: Mismatch? = null
        private set

    /** True when this connection is the one that wrote the pin, which the UI may want to mention. */
    @Volatile
    var pinnedNow: Boolean = false
        private set

    override fun verify(hostname: String, port: Int, key: PublicKey): Boolean {
        // The wire encoding, not the JCE encoding: it is what ssh itself hashes, so the fingerprint
        // this produces is the one ssh-keygen -lf prints for the same host key.
        val offered = Base64.getEncoder()
            .encodeToString(Buffer.PlainBuffer().putPublicKey(key).compactData)

        val trusted = store.trusted(address)
        if (trusted.isEmpty()) {
            store.trust(address, offered, keyCapacity)
            pinnedNow = true
            return true
        }
        if (offered in trusted) return true

        mismatch = Mismatch(
            address = address,
            trustedFingerprints = trusted.map(HostKeyStore::fingerprintOf),
            offeredFingerprint = HostKeyStore.fingerprintOf(offered),
            offeredKeyBlob = offered,
        )
        return false
    }

    /**
     * sshj asks which host key algorithms are already known for this address so it can put them first
     * in the negotiation. We pin blobs and do not index them by algorithm, so there is nothing to
     * offer and the server's own preference order stands.
     */
    override fun findExistingAlgorithms(hostname: String, port: Int): List<String> = emptyList()
}
