package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.data.BindingsStore
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.SetupMerge
import com.x1f4r.legioncontrol.data.WakeHelperConfig
import com.x1f4r.legioncontrol.data.withList
import com.x1f4r.legioncontrol.data.withText
import com.x1f4r.legioncontrol.data.text
import com.x1f4r.legioncontrol.data.withWake
import com.x1f4r.legioncontrol.data.advisories
import com.x1f4r.legioncontrol.data.readControllerConfig
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject

/**
 * Editing the shared setup, on a phone.
 *
 * Every device is a peer, so the phone edits and publishes exactly like the Mac and the desktop.
 * What it does not do is round-trip the document through a typed model: a key this build has never
 * heard of, written by a newer client or by somebody's hand, would be silently dropped on the first
 * edit made here and the document would diverge for everybody. So every change below is a transform
 * of the parsed tree, touching one field and leaving the rest exactly where it was.
 *
 * The edit is kept apart from what is in force until Apply. An edit is a real edit the moment it is
 * applied, offline or not, and it is published on the next poll of each machine; but a half-typed
 * hostname is not an edit and must not become one.
 */
@Stable
class SetupEditorModel(
    private val services: ControlServices,
    private val bindings: BindingsStore,
) {
    /** The document being edited. Null until there is something to edit. */
    var working: ControllerDocument? by mutableStateOf(null)
        private set

    /** What is in force, to compare against. */
    private var base: ControllerDocument? = null

    var error: String? by mutableStateOf(null)
        private set
    var note: String? by mutableStateOf(null)
        private set

    /** Which machine's rows are open, so a long setup is not a wall. */
    val expanded = mutableStateMapOf<String, Boolean>()

    val hasChanges: Boolean
        get() = working != null && working?.canonicalText != base?.canonicalText

    /** Everything the validator says, so Apply can be refused with a reason rather than silently. */
    val validation: Result<ControllerConfig>?
        get() = working?.let { readControllerConfig(it.canonicalText) }

    val isValid: Boolean get() = validation?.isSuccess == true

    val problem: String?
        get() = validation?.exceptionOrNull()?.message

    /** Warnings, which never block Apply. */
    val advisories: List<String>
        get() = validation?.getOrNull()?.advisories().orEmpty()

    val typed: ControllerConfig? get() = validation?.getOrNull()

    fun reset(document: ControllerDocument?) {
        base = document
        working = document
        error = null
        note = null
    }

    /** Starts editing from what is in force, discarding anything half-done. */
    fun startFresh() = reset(services.document.value)

    private fun change(transform: (ControllerDocument) -> ControllerDocument) {
        val current = working ?: return
        working = transform(current)
        error = null
    }

    // MARK: - Machines

    fun addMachine(id: String, name: String): String? {
        val trimmed = id.trim()
        if (!Contract.isToken(trimmed)) {
            return "A machine id is letters, digits and . _ : @ / \\ ~ = + -, starting with a letter " +
                "or a digit."
        }
        if (typed?.machines?.any { it.id == trimmed } == true) {
            return "There is already a machine called \"$trimmed\"."
        }
        change { it.addMachine(trimmed, name.trim().ifBlank { trimmed }) }
        expanded[trimmed] = true
        return null
    }

    fun removeMachine(id: String) = change { it.removeMachine(id) }

    fun setMachineName(id: String, name: String) = change {
        it.editMachine(id) { machine -> machine.withText("name", name) }
    }

    fun setMachineSite(id: String, siteId: String?) = change {
        it.editMachine(id) { machine -> machine.withText("site", siteId) }
    }

    fun setMachineAlwaysOn(id: String, alwaysOn: Boolean) = change {
        it.editMachine(id) { machine ->
            JsonObject(machine + ("alwaysOn" to JsonPrimitive(alwaysOn)))
        }
    }

    // MARK: - Endpoints

    fun addEndpoint(machineId: String, endpointId: String, host: String, user: String): String? {
        val trimmed = endpointId.trim()
        if (!Contract.isToken(trimmed)) return "An address id has to be a plain identifier."
        if (host.isBlank()) return "An address needs a host."
        if (user.isBlank()) return "An address needs a user to log in as."
        change {
            it.editMachine(machineId) { machine ->
                machine.withList("endpoints") { existing ->
                    existing + buildJsonObject {
                        put("id", JsonPrimitive(trimmed))
                        put("kind", JsonPrimitive("remote"))
                        put("host", JsonPrimitive(host.trim()))
                        put("port", JsonPrimitive(22))
                        put("user", JsonPrimitive(user.trim()))
                    }
                }
            }
        }
        return null
    }

    fun removeEndpoint(machineId: String, endpointId: String) = change {
        it.editMachine(machineId) { machine ->
            machine.withList("endpoints") { existing -> existing.filterNot { e -> e.text("id") == endpointId } }
        }
    }

    fun editEndpoint(
        machineId: String,
        endpointId: String,
        transform: (JsonObject) -> JsonObject,
    ) = change {
        it.editMachine(machineId) { machine ->
            machine.withList("endpoints") { existing ->
                existing.map { e -> if (e.text("id") == endpointId) transform(e) else e }
            }
        }
    }

    // MARK: - Systems

    fun addSystem(machineId: String, systemId: String, name: String, platform: String): String? {
        val trimmed = systemId.trim()
        if (!Contract.isToken(trimmed)) return "A system id has to be a plain identifier."
        change {
            it.editMachine(machineId) { machine ->
                machine.withList("systems") { existing ->
                    existing + buildJsonObject {
                        put("id", JsonPrimitive(trimmed))
                        put("name", JsonPrimitive(name.trim().ifBlank { trimmed }))
                        put("platform", JsonPrimitive(platform))
                        put("agent", JsonArray(emptyList()))
                    }
                }
            }
        }
        return null
    }

    fun removeSystem(machineId: String, systemId: String) = change {
        it.editMachine(machineId) { machine ->
            machine.withList("systems") { existing -> existing.filterNot { s -> s.text("id") == systemId } }
        }
    }

    fun editSystem(
        machineId: String,
        systemId: String,
        transform: (JsonObject) -> JsonObject,
    ) = change {
        it.editMachine(machineId) { machine ->
            machine.withList("systems") { existing ->
                existing.map { s -> if (s.text("id") == systemId) transform(s) else s }
            }
        }
    }

    /**
     * The argv the agent is run with, as a whitespace-separated line.
     *
     * Whitespace separated because that is how somebody writes a command, and quoting is handled by
     * the transport rather than by the person: a path with a space in it is written normally here
     * and quoted correctly for whichever shell the system says it uses.
     */
    fun setSystemAgent(machineId: String, systemId: String, line: String) =
        editSystem(machineId, systemId) { system ->
            JsonObject(
                system + (
                    "agent" to buildJsonArray {
                        line.trim().split(Regex("\\s+")).filter { it.isNotEmpty() }
                            .forEach { add(JsonPrimitive(it)) }
                    }
                    ),
            )
        }

    fun setSystemShell(machineId: String, systemId: String, shell: String?) =
        editSystem(machineId, systemId) { it.withText("shell", shell) }

    fun setSystemRestricted(machineId: String, systemId: String, restricted: Boolean) =
        editSystem(machineId, systemId) { system ->
            JsonObject(system + ("restricted" to JsonPrimitive(restricted)))
        }

    // MARK: - Waking

    fun setWakeMac(machineId: String, mac: String) = change {
        it.editMachine(machineId) { machine ->
            machine.withWake { wake -> wake.withText("mac", mac) }
        }
    }

    fun setWakeBroadcasts(machineId: String, addresses: List<String>) = change {
        it.editMachine(machineId) { machine ->
            machine.withWake { wake ->
                JsonObject(
                    wake + (
                        "broadcast" to buildJsonArray {
                            addresses.filter { a -> a.isNotBlank() }.forEach { a -> add(JsonPrimitive(a)) }
                        }
                        ),
                )
            }
        }
    }

    fun setWakeLanPrefix(machineId: String, prefix: String?) = change {
        it.editMachine(machineId) { machine ->
            machine.withWake { wake -> wake.withText("lanPrefix", prefix) }
        }
    }

    fun setHelpers(machineId: String, helpers: List<WakeHelperConfig>) = change {
        it.setHelpers(machineId, helpers)
    }

    fun addHelper(machineId: String, helperMachineId: String, actionId: String): String? {
        if (helperMachineId == machineId) {
            return "A machine cannot wake itself. One that is asleep cannot send its own packet."
        }
        if (!Contract.isToken(actionId.trim())) {
            return "A wake action id has to be a plain identifier."
        }
        val existing = typed?.machines?.firstOrNull { it.id == machineId }
            ?.wake?.effectiveHelpers.orEmpty()
        setHelpers(machineId, existing + WakeHelperConfig(helperMachineId, actionId.trim()))
        return null
    }

    fun removeHelper(machineId: String, index: Int) {
        val existing = typed?.machines?.firstOrNull { it.id == machineId }
            ?.wake?.effectiveHelpers.orEmpty()
        if (index !in existing.indices) return
        setHelpers(machineId, existing.filterIndexed { i, _ -> i != index })
    }

    /** Moves a helper one place earlier, because the order is the failover order. */
    fun promoteHelper(machineId: String, index: Int) {
        val existing = typed?.machines?.firstOrNull { it.id == machineId }
            ?.wake?.effectiveHelpers.orEmpty().toMutableList()
        if (index <= 0 || index >= existing.size) return
        val moved = existing.removeAt(index)
        existing.add(index - 1, moved)
        setHelpers(machineId, existing)
    }

    // MARK: - Sites

    fun addSite(id: String, name: String): String? {
        val trimmed = id.trim()
        if (!Contract.isToken(trimmed)) return "A site id has to be a plain identifier."
        if (typed?.sites?.any { it.id == trimmed } == true) {
            return "There is already a site called \"$trimmed\"."
        }
        change { it.addSite(trimmed, name.trim().ifBlank { trimmed }) }
        return null
    }

    fun removeSite(id: String) = change { it.removeSite(id) }

    fun setSitePrefixes(id: String, prefixes: List<String>) = change {
        it.editSite(id) { site ->
            JsonObject(
                site + (
                    "lanPrefixes" to buildJsonArray {
                        prefixes.filter { p -> p.isNotBlank() }.forEach { p -> add(JsonPrimitive(p)) }
                    }
                    ),
            )
        }
    }

    fun setSiteBroadcasts(id: String, addresses: List<String>) = change {
        it.editSite(id) { site ->
            JsonObject(
                site + (
                    "broadcast" to buildJsonArray {
                        addresses.filter { a -> a.isNotBlank() }.forEach { a -> add(JsonPrimitive(a)) }
                    }
                    ),
            )
        }
    }

    // MARK: - Applying

    /**
     * Puts the edit in force.
     *
     * It becomes this device's document at once and is published on the next poll of every machine
     * that will take it. Nothing is sent from here: publishing is reconciliation's job, and doing it
     * here would mean an edit made with no machine in reach was not an edit.
     */
    fun apply(): Boolean {
        val document = working ?: return false
        val failure = services.applyEdit(document, services.deviceName())
        if (failure != null) {
            error = failure
            return false
        }
        base = services.document.value
        working = base
        note = "The setup is in force. Every machine takes it on its next check."
        return true
    }

    fun discard() {
        working = base
        error = null
        note = null
    }

    /** What this edit changes, in the same terms a divergence is shown in. */
    fun changes(): List<SetupMerge.Difference> {
        val from = base?.root ?: return emptyList()
        val to = working?.root ?: return emptyList()
        return SetupMerge.differences(mine = to, theirs = from, base = null)
    }
}
