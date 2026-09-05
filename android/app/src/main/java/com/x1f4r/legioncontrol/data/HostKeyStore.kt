package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
import java.security.MessageDigest
import java.util.Base64

/** Private OS identities and retained legacy pins. Shared setup edits never change this store. */
class HostKeyStore(context: Context) {
    private val prefs: SharedPreferences = context.applicationContext.getSharedPreferences("legion-host-keys", Context.MODE_PRIVATE)
    private val groups: SharedPreferences = context.applicationContext.getSharedPreferences("legion-host-identities", Context.MODE_PRIVATE)

    private fun identities(): HostIdentities {
        check(!writeFailed) { "Host trust could not be saved. Restart the app before reviewing it again." }
        return groups.getString("document", null)?.let {
            kotlinx.serialization.json.Json.decodeFromString<HostIdentities>(it).also { document ->
                check(document.version == 1) { "Unsupported host identity store." }
            }
        } ?: HostIdentities()
    }

    private fun commit(editor: SharedPreferences.Editor) {
        if (!editor.commit()) {
            writeFailed = true
            error("Could not save host trust. Restart the app before reviewing it again.")
        }
    }

    private fun legacy(address: String): List<String> =
        (prefs.all[address] as? String)?.split('\n')?.filter { it.isNotBlank() }.orEmpty()

    fun snapshot(address: String): HostTrustSnapshot = synchronized(LOCK) { identities().snapshot(address, legacy(address)) }
    fun trusted(address: String): List<String> = snapshot(address).trusted

    fun approve(address: String, blob: String, approval: HostTrustApproval) = synchronized(LOCK) {
        val updated = identities().approve(address, blob, legacy(address), approval)
        commit(groups.edit().putString("document", kotlinx.serialization.json.Json.encodeToString(HostIdentities.serializer(), updated)))
    }

    fun configureSystems(address: String, systems: List<HostIdentitySystem>, revision: Long) = synchronized(LOCK) {
        val updated = identities().configureSystems(address, systems, revision)
        commit(groups.edit().putString("document", kotlinx.serialization.json.Json.encodeToString(HostIdentities.serializer(), updated)))
    }

    /** Explicit removal from the trust settings; never called during configuration reconciliation. */
    fun forget(address: String) = synchronized(LOCK) {
        val old = identities()
        commit(groups.edit().putString("document", kotlinx.serialization.json.Json.encodeToString(HostIdentities.serializer(), old.copy(revision = old.revision + 1, endpoints = old.endpoints.filterNot { it.address == address }))))
        commit(prefs.edit().remove(address))
    }

    fun forgetAll() = synchronized(LOCK) {
        val old = identities()
        commit(groups.edit().putString("document", kotlinx.serialization.json.Json.encodeToString(HostIdentities.serializer(), HostIdentities(revision = old.revision + 1))))
        commit(prefs.edit().clear())
    }

    /** Every trusted key, as address to fingerprints, for a screen that shows what the app trusts. */
    fun fingerprints(): Map<String, List<String>> =
        (prefs.all.keys + identities().endpoints.map { it.address }).associateWith { address -> trusted(address).map(::fingerprintOf) }

    companion object {
        private val LOCK = Any()
        @Volatile private var writeFailed = false
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
