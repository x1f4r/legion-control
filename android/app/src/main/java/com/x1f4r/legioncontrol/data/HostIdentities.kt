package com.x1f4r.legioncontrol.data

import kotlinx.serialization.Serializable
import java.nio.ByteBuffer
import java.util.Base64

@Serializable
data class HostIdentityKey(val algorithm: String, val blob: String) {
    companion object {
        fun fromBlob(blob: String): HostIdentityKey {
            val bytes = Base64.getDecoder().decode(blob)
            require(bytes.size >= 8) { "Malformed SSH public key." }
            val length = ByteBuffer.wrap(bytes).int
            require(length in 1..128 && length < bytes.size - 4) { "Malformed SSH key algorithm." }
            val algorithm = String(bytes, 4, length, Charsets.US_ASCII)
            require(algorithm.matches(Regex("[a-zA-Z0-9@._+-]+"))) { "Malformed SSH key algorithm." }
            return HostIdentityKey(algorithm, blob)
        }
    }
}

@Serializable
data class HostIdentitySystem(val id: String, val name: String = id, val keys: List<HostIdentityKey> = emptyList())
@Serializable
data class HostIdentityEndpoint(val address: String, val systems: List<HostIdentitySystem> = emptyList())
@Serializable
data class HostIdentities(val version: Int = 1, val revision: Long = 0, val endpoints: List<HostIdentityEndpoint> = emptyList())

data class HostTrustSnapshot(val revision: Long, val systems: List<HostIdentitySystem>, val legacy: List<String>) {
    val unassigned: List<String> get() = legacy.filterNot { blob -> systems.any { s -> s.keys.any { it.blob == blob } } }
    val needsSetup: Boolean get() = systems.isEmpty() || unassigned.isNotEmpty()
    val canApprove: Boolean get() = needsSetup || systems.any { it.keys.isEmpty() }
    val trusted: List<String> get() = (legacy + systems.flatMap { it.keys }.map { it.blob }).distinct()
}

/** Explicit local choices made while viewing the exact offered and existing fingerprints. */
data class HostTrustApproval(
    val revision: Long,
    val systems: List<HostIdentitySystem>,
    val selectedSystemId: String,
    val legacyAssignments: Map<String, String> = emptyMap(),
)

fun HostIdentities.snapshot(address: String, legacy: List<String>) =
    HostTrustSnapshot(revision, endpoints.firstOrNull { it.address == address }?.systems.orEmpty(), legacy)

/** Pure enrollment transaction. Shared topology never participates in this decision. */
fun HostIdentities.approve(address: String, offeredBlob: String, legacy: List<String>, approval: HostTrustApproval): HostIdentities {
    val before = snapshot(address, legacy)
    val offered = HostIdentityKey.fromBlob(offeredBlob)
    if (before.systems.any { it.id == approval.selectedSystemId && offered in it.keys }) return this
    require(revision == approval.revision) { "Host trust changed. Review the fingerprints again." }
    require(approval.systems.isNotEmpty() && approval.systems.map { it.id }.distinct().size == approval.systems.size)
    require(approval.systems.all { it.id.matches(Regex("[A-Za-z0-9][A-Za-z0-9._-]{0,63}")) && it.keys.isEmpty() })
    val proposed = if (before.needsSetup) {
        require(before.systems.all { old -> approval.systems.any { it.id == old.id } }) { "Existing identities must stay." }
        approval.systems.map { label -> before.systems.firstOrNull { it.id == label.id } ?: label }
    } else {
        require(approval.systems.map { it.id }.toSet() == before.systems.map { it.id }.toSet()) { "Manage local identities separately before adding systems." }
        before.systems
    }
    require(before.unassigned.all { it in approval.legacyAssignments }) { "Assign every existing fingerprint to an operating system first." }
    val assigned = proposed.map { system ->
        val keys = (system.keys + before.unassigned.filter { approval.legacyAssignments[it] == system.id }.map(HostIdentityKey::fromBlob)).distinct()
        require(keys.map { it.algorithm }.distinct().size == keys.size) { "Different keys of one algorithm need separate operating-system identities." }
        system.copy(keys = keys)
    }
    require(before.unassigned.all { blob -> assigned.any { s -> s.keys.any { it.blob == blob } } })
    val selected = assigned.firstOrNull { it.id == approval.selectedSystemId } ?: error("Select the offered key's operating system.")
    require(selected.keys.isEmpty()) { "That operating system already has keys. No replacement was made." }
    require(assigned.none { s -> s.keys.any { it.blob == offeredBlob } }) { "This key is already assigned to another operating system." }
    val endpoint = HostIdentityEndpoint(address, assigned.map { if (it.id == selected.id) it.copy(keys = listOf(offered)) else it })
    return copy(revision = revision + 1, endpoints = endpoints.filterNot { it.address == address } + endpoint)
}

/** A separate local settings action reserves OS identities without approving any offered key. */
fun HostIdentities.configureSystems(address: String, systems: List<HostIdentitySystem>, expectedRevision: Long): HostIdentities {
    require(revision == expectedRevision) { "Host trust changed. Reopen local trust settings." }
    require(systems.isNotEmpty() && systems.map { it.id }.distinct().size == systems.size)
    require(systems.all { it.keys.isEmpty() && it.id.matches(Regex("[A-Za-z0-9][A-Za-z0-9._-]{0,63}")) })
    val old = endpoints.firstOrNull { it.address == address }?.systems.orEmpty()
    require(old.all { existing -> systems.any { it.id == existing.id } }) { "Existing operating-system identities must stay." }
    val updated = systems.map { label -> old.firstOrNull { it.id == label.id } ?: label }
    if (old == updated) return this
    return copy(revision = revision + 1, endpoints = endpoints.filterNot { it.address == address } + HostIdentityEndpoint(address, updated))
}
