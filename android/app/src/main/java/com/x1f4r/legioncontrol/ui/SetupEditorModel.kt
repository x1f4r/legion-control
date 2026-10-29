package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchReviewEntry
import com.x1f4r.legioncontrol.data.LaunchScope
import com.x1f4r.legioncontrol.data.LaunchValues
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
 *
 * Applying an edit approves every command it changes on this device (contract section 9.9). So the
 * command, shell and restricted key of a system whose command waits for approval cannot be edited
 * here at all: changing its shell would otherwise approve the unapproved argv sitting beside it. It
 * is approved or reverted as it stands, and an editor never saves an empty or unshowable argv.
 */
@Stable
class SetupEditorModel(
    private val services: ControlServices,
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

    val isValid: Boolean get() = validation?.isSuccess == true && commandProblem == null

    val problem: String?
        get() = validation?.exceptionOrNull()?.message ?: commandProblem

    /**
     * Why a command this edit writes cannot be saved, or null.
     *
     * Only the systems whose command the edit changes or adds are looked at: a command that came in
     * from a peer is the review's to settle, and refusing every other edit because of it would leave
     * nothing but the raw field to work in.
     */
    val commandProblem: String?
        get() {
            val edited = working?.root ?: return null
            val before = LaunchValues.projection(base?.root)
            for (machine in LaunchValues.projection(edited).machines) {
                for (system in machine.systems) {
                    if (before.machine(machine.id)?.system(system.id)?.tuple == system.tuple) continue
                    val argv = commandOf(edited, machine.id, system.id) ?: emptyList()
                    agentProblem(argv)?.let { return "${system.id} on ${machine.id}: $it" }
                }
            }
            return null
        }

    /**
     * The command of [systemId] on [machineId] waiting for this device's approval, or null.
     *
     * While one waits, the system's launch inputs are read-only and offer Approve and Revert.
     */
    fun pendingCommand(machineId: String, systemId: String): LaunchReviewEntry? =
        services.launch.value.review.firstOrNull {
            it.scope == LaunchScope.SYSTEM && it.machineId == machineId && it.systemId == systemId
        }

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

    /**
     * Adds a system with the command that runs its agent.
     *
     * The command is part of adding it: a system without one is never driven, and a document an
     * older client reads with an empty `agent` in it is one that client refuses.
     */
    fun addSystem(
        machineId: String,
        systemId: String,
        name: String,
        platform: String,
        agent: List<String>,
    ): String? {
        val trimmed = systemId.trim()
        if (!Contract.isToken(trimmed)) return "A system id has to be a plain identifier."
        agentProblem(agent)?.let { return it }
        change {
            it.editMachine(machineId) { machine ->
                machine.withList("systems") { existing ->
                    existing + buildJsonObject {
                        put("id", JsonPrimitive(trimmed))
                        put("name", JsonPrimitive(name.trim().ifBlank { trimmed }))
                        put("platform", JsonPrimitive(platform))
                        put("agent", JsonArray(agent.map(::JsonPrimitive)))
                    }.withWindowsShell()
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

    /** The argv the agent is run with, as a whitespace-separated line (see [argvOf]). */
    fun setSystemAgent(machineId: String, systemId: String, line: String) =
        editLaunch(machineId, systemId) { system ->
            JsonObject(system + ("agent" to buildJsonArray { argvOf(line).forEach { add(JsonPrimitive(it)) } }))
                .withWindowsShell()
        }

    fun setSystemShell(machineId: String, systemId: String, shell: String?) =
        editLaunch(machineId, systemId) { it.withShell(shell) }

    fun setSystemRestricted(machineId: String, systemId: String, restricted: Boolean) =
        editLaunch(machineId, systemId) { system ->
            JsonObject(system + ("restricted" to JsonPrimitive(restricted))).withWindowsShell()
        }

    /** An edit of what a system runs, which a system whose command waits for approval does not take. */
    private fun editLaunch(machineId: String, systemId: String, transform: (JsonObject) -> JsonObject) {
        if (pendingCommand(machineId, systemId) != null) return
        editSystem(machineId, systemId, transform)
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
     *
     * A command this edit writes that [agentProblem] refuses is refused here too, and not only by a
     * button that is off: the store's own checks let an invisible character through, and applying
     * the edit would approve it on this device.
     */
    fun apply(): Boolean {
        val document = working ?: return false
        commandProblem?.let { problem ->
            error = problem
            return false
        }
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

/**
 * A command as somebody writes it, one argument per whitespace-separated word.
 *
 * Whitespace separated because that is how somebody writes a command. Quoting is the transport's
 * business rather than the person's: each word is written for whichever shell the system says it
 * uses.
 */
internal fun argvOf(line: String): List<String> = line.trim().split(Regex("\\s+")).filter { it.isNotEmpty() }

/**
 * Why an editor will not save [agent] as a system's command, or null.
 *
 * Never an empty argv, and never an argument a person could not check by reading it: one holding a
 * control, invisible or bidirectional formatting character, or longer than the agent takes (contract
 * section 9.9). Such a command could only ever be reverted, so it is not written in the first place.
 */
internal fun agentProblem(agent: List<String>): String? = when {
    agent.isEmpty() ->
        "It needs the command that runs its agent, for example " +
            "/usr/bin/node /home/me/.legion-control/bin/launcher.mjs."

    agent.any { !LaunchValues.isLaunchText(it) } ->
        "An argument holds a control, invisible or bidirectional formatting character, or is longer " +
            "than 4096 bytes, so what it says cannot be checked by reading it."

    else -> null
}

/** The `agent` strings of one system in [document], or null when it has none to speak of. */
private fun commandOf(document: JsonObject, machineId: String, systemId: String): List<String>? =
    (document["machines"] as? JsonArray)?.filterIsInstance<JsonObject>()?.firstOrNull { it.text("id") == machineId }
        ?.let { machine -> (machine["systems"] as? JsonArray)?.filterIsInstance<JsonObject>() }
        ?.firstOrNull { it.text("id") == systemId }
        ?.let { system -> (system["agent"] as? JsonArray)?.map { (it as? JsonPrimitive)?.content.orEmpty() } }

/**
 * Contract section 15.1: an editor writes `shell` for every Windows system it creates or changes.
 *
 * The grammar a command is written in then sits in the document rather than in a default each client
 * reads for itself. cmd, because it is what a stock Windows OpenSSH runs and what an absent shell
 * already means on Windows, so writing it changes nothing that runs. A shell that is already there,
 * PowerShell included, is left alone.
 */
internal fun JsonObject.withWindowsShell(): JsonObject =
    if (text("platform") == "windows" && this["shell"] == null) {
        JsonObject(this + ("shell" to JsonPrimitive("cmd")))
    } else {
        this
    }

/**
 * A system with its `shell` set to [shell], or taken out when it is null.
 *
 * Taking it out of a Windows system writes cmd instead, by the same rule: a Windows system this
 * editor changes is never saved without a shell.
 */
internal fun JsonObject.withShell(shell: String?): JsonObject = withText("shell", shell).withWindowsShell()
