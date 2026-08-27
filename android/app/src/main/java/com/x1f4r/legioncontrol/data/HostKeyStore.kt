package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
import java.security.MessageDigest
import java.util.Base64

/**
 * Trust on first use for ssh host keys, kept per address.
 *
 * Per address rather than per machine, and each address holds the last few keys it was trusted
 * with, not just the last one. A machine that dual boots presents a different host key per system
 * and answers on the same LAN address for all of them, so that one address legitimately alternates
 * between keys with every boot switch. Holding all of them means each system is a question once and
 * never again, while a key nobody has seen before is still reported.
 *
 * The caller says how many keys an address may hold, because that is knowledge about the address:
 * a remote address is one system, a LAN address is one per system on the machine. Trusting a key
 * beyond that evicts the oldest, so a reinstalled system's stale key cleans itself up instead of
 * sitting in the store as one more key an attacker would be allowed to present.
 */
class HostKeyStore(context: Context) {
    private val prefs: SharedPreferences =
        context.applicationContext.getSharedPreferences("legion-host-keys", Context.MODE_PRIVATE)

    /**
     * Every trusted key blob for [address], base64 of the wire encoding, oldest first. Empty if the
     * address has never been seen. Entries written by a build that kept one key per address are a
     * plain blob, which reads as a list of one, so nothing has to be migrated.
     */
    fun trusted(address: String): List<String> =
        (prefs.all[address] as? String)?.split('\n')?.filter { it.isNotBlank() } ?: emptyList()

    /**
     * Adds [keyBlobBase64] as the newest key trusted for [address], keeping at most [keep] keys and
     * dropping the oldest beyond that. Trusting a key that is already there refreshes its age.
     */
    fun trust(address: String, keyBlobBase64: String, keep: Int) {
        val keys = trusted(address) - keyBlobBase64 + keyBlobBase64
        prefs.edit().putString(address, keys.takeLast(keep).joinToString("\n")).apply()
    }

    /** Drops every key for [address], so the next connection trusts whatever is offered instead. */
    fun forget(address: String) {
        prefs.edit().remove(address).apply()
    }

    fun forgetAll() {
        prefs.edit().clear().apply()
    }

    /**
     * Drops every address that is not in [addresses]. Called whenever the configuration changes,
     * with the addresses it still names, so keys for addresses that no longer exist do not sit in
     * the store forever.
     */
    fun retainOnly(addresses: Set<String>) {
        val editor = prefs.edit()
        prefs.all.keys.filterNot { it in addresses }.forEach(editor::remove)
        editor.apply()
    }

    /** Every trusted key, as address to fingerprints, for a screen that shows what the app trusts. */
    fun fingerprints(): Map<String, List<String>> =
        prefs.all.keys.associateWith { address -> trusted(address).map(::fingerprintOf) }

    companion object {
        /** The OpenSSH style fingerprint of a base64 key blob: SHA256 and no padding. */
        fun fingerprintOf(keyBlobBase64: String): String = try {
            val digest = MessageDigest.getInstance("SHA-256")
                .digest(Base64.getDecoder().decode(keyBlobBase64))
            "SHA256:" + Base64.getEncoder().withoutPadding().encodeToString(digest)
        } catch (_: Exception) {
            "unreadable"
        }
    }
}
