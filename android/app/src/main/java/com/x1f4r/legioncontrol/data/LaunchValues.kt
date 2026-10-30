package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.agent.GrammarChoice
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.agent.UNKNOWN_SHELL
import com.x1f4r.legioncontrol.agent.grammarFor
import com.x1f4r.legioncontrol.net.UpdateSource
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

/**
 * Launch values, and which of them this device runs: contract section 9.9.
 *
 * The shared setup carries the argv this phone runs over ssh with its own key, and lineage only shows
 * that a document claims an ancestor, not who wrote it. Any key that may run `config set`, a
 * restricted one included, could publish a descendant whose `agent` was `sh -c …`, and this app
 * adopted it and ran it on every machine with a key that is not restricted (T1). The document still
 * replicates and applies unchanged. What changed is that a device runs only the launch values it
 * approved itself, and everything else waits for the person holding it.
 *
 * A port of `contract/tools/launch.mjs`, held to it by `contract/vectors/launch.json`: which values
 * are launch values and how they are read (the projection), which offered values are accepted
 * without asking, which wait and whether they could be approved at all, and what this device runs
 * meanwhile. Values are compared as JSON: arrays in order, strings by code point, with no case
 * folding and no path or Unicode normalisation, because two spellings a person would call the same
 * path are two different files to a file system that does not normalise.
 *
 * Everything here reads the parsed JSON and never [ControllerConfig]. The typed model fills in
 * defaults and decodes leniently, so `"agent": ["node", 5]` is a runnable command to it and nothing
 * at all to every other client reading the same bytes.
 */
object LaunchValues {

    const val REPO_PATH = "appUpdates.githubRepo"
    const val LOCAL_PATH = "local"

    /** `seededFrom` for a record that was not seeded from a document. */
    const val NONE = "none"

    /** A record that approves nothing: a fresh device, or a new one started after an unreadable one. */
    val EMPTY_RECORD: JsonObject = buildJsonObject {
        put("schema", 1)
        put("seededFrom", NONE)
    }

    // Paths. Ids are written as they are, inside the brackets, and never parsed back out: an id may
    // contain a slash or a dot, which is why every entry also carries its ids and the record nests
    // them as keys (common.schema.json#/$defs/launchPath).

    fun systemPath(machine: String, system: String): String = "machines[$machine].systems[$system]"

    fun identityFilePath(machine: String): String = "machines[$machine].ssh.identityFile"

    // MARK: - What can be approved at all

    /**
     * Whether an argv item, an identityFile or local.agent can be shown to a person and approved.
     *
     * Control characters, C1 controls, zero-width and bidirectional formatting characters, word
     * joiners and the byte order mark can each make the text a person approves look different from
     * the text that runs, so a value holding one can only be reverted or left. A line break is
     * refused before the pattern is applied, as every pattern in the contract is.
     */
    fun isLaunchText(text: String?): Boolean {
        if (text == null || '\n' in text || '\r' in text) return false
        return utf8Length(text) in 1..MAX_LAUNCH_TEXT_BYTES && LAUNCH_TEXT.matches(text)
    }

    /** One owner/name GitHub could have, by the guard every implementation shares. */
    fun isRepo(value: String?): Boolean = value != null && UpdateSource.isRepository(value)

    // MARK: - The projection

    /**
     * The launch values of a parsed document (section 9.9, "Reading them").
     *
     * Machines and systems are walked in order; a non-object, an item without a string id and a
     * repeated id after its first occurrence are skipped, which is also what every other client
     * does with the same bytes.
     */
    fun projection(document: JsonObject?): LaunchProjection {
        if (document == null) return LaunchProjection.EMPTY
        val machines = mutableListOf<LaunchProjection.Machine>()
        val seenMachines = mutableSetOf<String>()
        for (machine in (document["machines"] as? JsonArray).orEmpty()) {
            if (machine !is JsonObject) continue
            val id = machine["id"].stringOrNull() ?: continue
            if (!seenMachines.add(id)) continue
            val systems = mutableListOf<LaunchProjection.System>()
            val seenSystems = mutableSetOf<String>()
            for (system in (machine["systems"] as? JsonArray).orEmpty()) {
                if (system !is JsonObject) continue
                val systemId = system["id"].stringOrNull() ?: continue
                if (!seenSystems.add(systemId)) continue
                systems += LaunchProjection.System(systemId, tupleOf(system))
            }
            val identityFile = (machine["ssh"] as? JsonObject)?.get("identityFile").stringOrNull()
            machines += LaunchProjection.Machine(id, systems, identityFile?.takeIf { it.isNotEmpty() })
        }
        val githubRepo = (document["appUpdates"] as? JsonObject)?.get("githubRepo").stringOrNull()
        val local = (document["local"] as? JsonObject)?.takeUnless { it["enabled"].isFalse() }
        return LaunchProjection(
            machines = machines,
            repo = githubRepo?.takeIf { it.isNotEmpty() && it != DEFAULT_GITHUB_REPO },
            local = local?.let { LaunchProjection.Local(it["agent"].stringOrNull()?.takeIf(String::isNotEmpty)) },
        )
    }

    private fun tupleOf(system: JsonObject): LaunchTuple {
        // Not an array, empty, or holding anything but strings: nothing runs on this system, so the
        // tuple is inert and accepted wherever it appears.
        val items = (system["agent"] as? JsonArray)?.takeIf { it.isNotEmpty() } ?: return LaunchTuple.INERT
        val agent = items.map { it.stringOrNull() ?: return LaunchTuple.INERT }
        return LaunchTuple(agent, grammarOf(system, agent.size))
    }

    /**
     * Section 15.1 on a system's own fields, exactly as the document has them, written
     * `refused:<code>` when it refuses.
     *
     * The grammar and not the raw platform, shell and restricted fields is what a tuple records:
     * it is what decides how the line is quoted, and an editor that writes `shell: "cmd"` where it
     * was absent on Windows changes nothing that runs and asks nobody anything.
     */
    internal fun grammarOf(system: JsonObject, agentLength: Int): String {
        val restricted = system["restricted"].isTrue()
        val shell = system["shell"]
        // A shell that is there and is not a string is no shell section 15.1 knows. Restricted wins
        // over every shell, so it is only asked about when the key is not restricted.
        val choice = if (!restricted && shell != null && shell !is JsonNull && shell.stringOrNull() == null) {
            GrammarChoice.refused(UNKNOWN_SHELL)
        } else {
            grammarFor(
                platform = system["platform"].stringOrNull(),
                shell = shell.stringOrNull(),
                restricted = restricted,
                restrictedSession = false,
                agentLength = agentLength,
            )
        }
        return choice.grammar?.wire ?: "refused:${choice.refusal}"
    }

    // MARK: - The record

    /**
     * A record (contract section 11) approving every launch value of [document].
     *
     * What this device writes when it seeds itself from the document it already applies, and what
     * agent 3.1.0 compares a restricted push with. An inert tuple runs nothing and is accepted
     * anyway, so it is not recorded.
     */
    fun approvalsFrom(document: JsonObject?, seededFrom: String = NONE): JsonObject {
        val projection = projection(document)
        val machines = buildJsonObject {
            for (machine in projection.machines) {
                val systems = buildJsonObject {
                    machine.systems.filterNot { it.tuple.isInert }.forEach { put(it.id, it.tuple.toJson()) }
                }
                val entry = buildJsonObject {
                    if (systems.isNotEmpty()) put("systems", systems)
                    machine.identityFile?.let { put("identityFile", it) }
                }
                if (entry.isNotEmpty()) put(machine.id, entry)
            }
        }
        return buildJsonObject {
            put("schema", 1)
            put("seededFrom", seededFrom)
            if (machines.isNotEmpty()) put("machines", machines)
            projection.repo?.let { put("githubRepo", it) }
            projection.local?.let { put("local", it.toJson()) }
        }
    }

    /** [record] with each of [changes] written at its place, and everything else exactly as it was. */
    fun recording(record: JsonObject, changes: List<LaunchChange>): JsonObject =
        changes.fold(record) { current, change -> current.with(change) }

    /**
     * What an edit made on this device approves: each launch value that differs between [before]
     * and [after], at its new value, and nothing else.
     *
     * With no [before], which is enrolment and an edit on a device that held nothing, that is
     * every launch value [after] has. A value the edit took away, or made inert, approves nothing:
     * nothing runs there, and the record keeps what it had.
     */
    fun changes(before: LaunchProjection?, after: LaunchProjection): List<LaunchChange> = buildList {
        for (machine in after.machines) {
            val previous = before?.machine(machine.id)
            for (system in machine.systems) {
                if (system.tuple.isInert) continue
                if (previous?.system(system.id)?.tuple == system.tuple) continue
                add(LaunchChange(LaunchScope.SYSTEM, machine.id, system.id, system.tuple.toJson()))
            }
            val identityFile = machine.identityFile
            if (identityFile != null && (previous == null || previous.identityFile != identityFile)) {
                add(LaunchChange(LaunchScope.IDENTITY_FILE, machine.id, null, JsonPrimitive(identityFile)))
            }
        }
        after.repo?.let { repo ->
            if (before?.repo != repo) add(LaunchChange(LaunchScope.REPO, null, null, JsonPrimitive(repo)))
        }
        after.local?.let { local ->
            if (before == null || before.local != local) add(LaunchChange(LaunchScope.LOCAL, null, null, local.toJson()))
        }
    }

    /**
     * Each value the gate runs as offered at a place whose record is missing or different, recorded
     * there (section 9.9, "Recording on accept").
     *
     * That is a value accepted by reuse, approved at another place, or by the launcher migration of
     * an approved tuple. The record it replaces leaves the approved set, which is the only pruning
     * there is. Only values that run are recorded: one that is accepted and cannot run is not driven
     * anyway, and recording it would only push out an approval at that place that can.
     */
    fun accepted(record: JsonObject, projection: LaunchProjection): List<LaunchChange> =
        effective(record, projection).filter { it.state == EffectiveLaunch.State.OFFERED }.mapNotNull { entry ->
            val change = when (entry.scope) {
                LaunchScope.SYSTEM ->
                    LaunchChange(entry.scope, entry.machineId, entry.systemId, entry.tuple?.toJson() ?: return@mapNotNull null)

                LaunchScope.IDENTITY_FILE ->
                    LaunchChange(entry.scope, entry.machineId, null, JsonPrimitive(entry.text ?: return@mapNotNull null))

                // The repository and local are accepted only when they are approved as they are, or
                // when they need no approval at all, so there is never anything to record for them.
                LaunchScope.REPO, LaunchScope.LOCAL -> return@mapNotNull null
            }
            change.takeUnless { record.at(change) == change.value }
        }

    // MARK: - Acceptance

    /**
     * Whether [offered] is the launcher migration of [approved].
     *
     * The same grammar, the same argv except one item, which moves from `<base>/agent/src/index.mjs`
     * to `<base>/bin/launcher.mjs` under the same non-empty base with no `..` segment, compared after
     * replacing `\` with `/` because the desktop writes the new path with forward slashes into a
     * Windows argv. The 3.1 upgrade migrates every system this way, and asking every peer to review a
     * file the agent itself installs under an already approved base would add prompts and no
     * protection. The reverse direction is never a migration.
     */
    fun isLauncherMigration(approved: LaunchTuple?, offered: LaunchTuple?): Boolean {
        if (approved == null || offered == null) return false
        val from = approved.agent?.takeIf { it.isNotEmpty() } ?: return false
        val to = offered.agent?.takeIf { it.isNotEmpty() } ?: return false
        if (approved.grammar == null || approved.grammar != offered.grammar || from.size != to.size) return false
        val differing = from.indices.filter { from[it] != to[it] }
        if (differing.size != 1) return false
        val entry = from[differing.single()].replace('\\', '/')
        val launcher = to[differing.single()].replace('\\', '/')
        if (!entry.endsWith(ENTRY_SUFFIX) || !launcher.endsWith(LAUNCHER_SUFFIX)) return false
        val base = entry.dropLast(ENTRY_SUFFIX.length)
        return base.isNotEmpty() && base == launcher.dropLast(LAUNCHER_SUFFIX.length) && ".." !in base.split('/')
    }

    /**
     * One entry for each offered launch value that is not accepted without asking, in this order:
     * for each machine its systems and then its identityFile; then the repository; then local.
     */
    fun review(record: JsonObject, projection: LaunchProjection): List<LaunchReviewEntry> {
        val approved = Approved.of(record)
        return buildList {
            for (machine in projection.machines) {
                for (system in machine.systems) {
                    if (approved.accepts(system.tuple)) continue
                    add(
                        LaunchReviewEntry(
                            path = systemPath(machine.id, system.id),
                            scope = LaunchScope.SYSTEM,
                            machineId = machine.id,
                            systemId = system.id,
                            approvable = system.tuple.canRun,
                            approved = record.recordedSystem(machine.id, system.id),
                            offered = system.tuple.toJson(),
                        ),
                    )
                }
                val identityFile = machine.identityFile
                if (identityFile != null && identityFile !in approved.identityFiles) {
                    add(
                        LaunchReviewEntry(
                            path = identityFilePath(machine.id),
                            scope = LaunchScope.IDENTITY_FILE,
                            machineId = machine.id,
                            systemId = null,
                            approvable = isLaunchText(identityFile),
                            approved = record.recordedIdentityFile(machine.id),
                            offered = JsonPrimitive(identityFile),
                        ),
                    )
                }
            }
            projection.repo?.takeIf { it != approved.repo }?.let { repo ->
                add(LaunchReviewEntry(REPO_PATH, LaunchScope.REPO, null, null, isRepo(repo), record["githubRepo"], JsonPrimitive(repo)))
            }
            projection.local?.takeUnless(approved::acceptsLocal)?.let { local ->
                add(LaunchReviewEntry(LOCAL_PATH, LaunchScope.LOCAL, null, null, local.canRun, record["local"], local.toJson()))
            }
        }
    }

    /**
     * What this device runs (section 9.9, "Everything else waits"): one entry per offered system,
     * then one identityFile entry per machine, then the repository, then local.
     *
     * An accepted value that can run is used as offered. Anything else falls back to what the record
     * holds at that same place, never at another one, and only when the recorded value can run
     * itself; with nothing usable there a system is not driven, an identityFile or local is none and
     * the repository is the built-in one. An inert tuple is not driven: nothing runs on it.
     */
    fun effective(record: JsonObject, projection: LaunchProjection): List<EffectiveLaunch> {
        val approved = Approved.of(record)
        return buildList {
            for (machine in projection.machines) {
                for (system in machine.systems) {
                    val tuple = system.tuple
                    val recorded = LaunchTuple.from(record.recordedSystem(machine.id, system.id))
                    val (state, used) = when {
                        tuple.isInert -> EffectiveLaunch.State.NOT_DRIVEN to null
                        approved.accepts(tuple) && tuple.canRun -> EffectiveLaunch.State.OFFERED to tuple
                        recorded?.canRun == true -> EffectiveLaunch.State.APPROVED to recorded
                        else -> EffectiveLaunch.State.NOT_DRIVEN to null
                    }
                    add(EffectiveLaunch(systemPath(machine.id, system.id), LaunchScope.SYSTEM, machine.id, system.id, state, used, null))
                }
            }
            for (machine in projection.machines) {
                val offered = machine.identityFile
                val recorded = record.recordedIdentityFile(machine.id).stringOrNull()
                val (state, text) = when {
                    offered != null && offered in approved.identityFiles && isLaunchText(offered) ->
                        EffectiveLaunch.State.OFFERED to offered

                    offered != null && isLaunchText(recorded) -> EffectiveLaunch.State.APPROVED to recorded
                    else -> EffectiveLaunch.State.NONE to null
                }
                add(EffectiveLaunch(identityFilePath(machine.id), LaunchScope.IDENTITY_FILE, machine.id, null, state, null, text))
            }
            run {
                val offered = projection.repo
                val recorded = record["githubRepo"].stringOrNull()
                val (state, text) = when {
                    offered != null && offered == approved.repo && isRepo(offered) -> EffectiveLaunch.State.OFFERED to offered
                    offered != null && isRepo(recorded) && recorded != DEFAULT_GITHUB_REPO -> EffectiveLaunch.State.APPROVED to recorded
                    else -> EffectiveLaunch.State.BUILT_IN to null
                }
                add(EffectiveLaunch(REPO_PATH, LaunchScope.REPO, null, null, state, null, text))
            }
            run {
                val offered = projection.local
                val recorded = record["local"] as? JsonObject
                val recordedAgent = recorded?.get("agent")
                // A recorded local runs when its agent is JSON null, the default path, or a value
                // that can be shown faithfully. A record without the key holds no agent at all.
                val recordedRuns = recordedAgent is JsonNull || isLaunchText(recordedAgent.stringOrNull())
                val (state, text) = when {
                    offered != null && approved.acceptsLocal(offered) && offered.canRun -> EffectiveLaunch.State.OFFERED to offered.agent
                    offered != null && recordedRuns -> EffectiveLaunch.State.APPROVED to recordedAgent.stringOrNull()
                    else -> EffectiveLaunch.State.NONE to null
                }
                add(EffectiveLaunch(LOCAL_PATH, LaunchScope.LOCAL, null, null, state, null, text))
            }
        }
    }

    // MARK: - Revert

    /**
     * [document] with the value [entry] offers replaced by the one this device approved there, or
     * null when there is no approved value to put back.
     *
     * Revert is an ordinary edit made on this device, which replicates, so a value is restored by
     * writing the document fields that give it, and the result is checked: a system's tuple is put
     * back only when its `platform`, `shell` and `restricted` give exactly the approved grammar.
     */
    fun reverted(document: JsonObject, entry: LaunchReviewEntry): JsonObject? = when (entry.scope) {
        LaunchScope.SYSTEM -> {
            val tuple = LaunchTuple.from(entry.approved)?.takeIf { it.canRun }
            val machineId = entry.machineId
            val systemId = entry.systemId
            if (tuple == null || machineId == null || systemId == null) {
                null
            } else {
                var restored = false
                val machines = (document["machines"] as? JsonArray).orEmpty().map { machine ->
                    if (restored || machine !is JsonObject || machine["id"].stringOrNull() != machineId) return@map machine
                    val systems = (machine["systems"] as? JsonArray).orEmpty().map { system ->
                        if (restored || system !is JsonObject || system["id"].stringOrNull() != systemId) return@map system
                        restoredSystem(system, tuple)?.also { restored = true } ?: system
                    }
                    if (restored) JsonObject(machine + ("systems" to JsonArray(systems))) else machine
                }
                if (restored) JsonObject(document + ("machines" to JsonArray(machines))) else null
            }
        }

        LaunchScope.IDENTITY_FILE -> {
            val path = entry.approved.stringOrNull()?.takeIf(::isLaunchText)
            val machineId = entry.machineId
            if (path == null || machineId == null) {
                null
            } else {
                var restored = false
                val machines = (document["machines"] as? JsonArray).orEmpty().map { machine ->
                    if (restored || machine !is JsonObject || machine["id"].stringOrNull() != machineId) return@map machine
                    restored = true
                    val ssh = machine["ssh"] as? JsonObject ?: JsonObject(emptyMap())
                    JsonObject(machine + ("ssh" to JsonObject(ssh + ("identityFile" to JsonPrimitive(path)))))
                }
                if (restored) JsonObject(document + ("machines" to JsonArray(machines))) else null
            }
        }

        LaunchScope.REPO -> entry.approved.stringOrNull()?.takeIf(::isRepo)?.let { repo ->
            val updates = document["appUpdates"] as? JsonObject ?: JsonObject(emptyMap())
            JsonObject(document + ("appUpdates" to JsonObject(updates + ("githubRepo" to JsonPrimitive(repo)))))
        }

        LaunchScope.LOCAL -> (entry.approved as? JsonObject)?.get("agent")?.let { agent ->
            val local = document["local"] as? JsonObject ?: return@let null
            when {
                agent is JsonNull -> JsonObject(document + ("local" to JsonObject(local - "agent")))
                isLaunchText(agent.stringOrNull()) -> JsonObject(document + ("local" to JsonObject(local + ("agent" to agent))))
                else -> null
            }
        }
    }

    /** [system] with the fields that give [tuple], or null when no setting of them does. */
    private fun restoredSystem(system: JsonObject, tuple: LaunchTuple): JsonObject? {
        val agent = tuple.agent ?: return null
        var restored = JsonObject(system + ("agent" to JsonArray(agent.map(::JsonPrimitive))))
        if (grammarOf(restored, agent.size) != tuple.grammar) {
            restored = if (tuple.grammar == RemoteShell.DISPATCH.wire) {
                JsonObject(restored + ("restricted" to JsonPrimitive(true)))
            } else {
                val unrestricted = if ("restricted" in restored) restored + ("restricted" to JsonPrimitive(false)) else restored
                JsonObject(unrestricted + ("shell" to JsonPrimitive(tuple.grammar)))
            }
        }
        return restored.takeIf { grammarOf(it, agent.size) == tuple.grammar }
    }

    // MARK: - Reading JSON as the contract does

    private const val ENTRY_SUFFIX = "/agent/src/index.mjs"
    private const val LAUNCHER_SUFFIX = "/bin/launcher.mjs"
    private const val MAX_LAUNCH_TEXT_BYTES = 4096

    /** The class of characters a launch value may hold, matched against the whole value. */
    private val LAUNCH_TEXT = Regex("[^\\u0000-\\u001F\\u007F-\\u009F\\u200B-\\u200F\\u202A-\\u202E\\u2060-\\u2064\\u2066-\\u2069\\uFEFF]+")

    /**
     * UTF-8 bytes, counted the way the reference counts them: a surrogate pair is one four byte code
     * point, and a lone surrogate is written as the three bytes of U+FFFD.
     */
    private fun utf8Length(text: String): Int {
        var size = 0
        var index = 0
        while (index < text.length) {
            val char = text[index]
            size += when {
                char.code < 0x80 -> 1
                char.code < 0x800 -> 2
                char.isHighSurrogate() && index + 1 < text.length && text[index + 1].isLowSurrogate() -> {
                    index += 1
                    4
                }

                else -> 3
            }
            index += 1
        }
        return size
    }

    /** Every value a record approves, wherever it sits: a value approved anywhere is accepted anywhere. */
    private class Approved(
        val tuples: List<LaunchTuple>,
        val identityFiles: List<String>,
        val repo: String?,
        val local: JsonObject?,
    ) {
        fun accepts(tuple: LaunchTuple): Boolean =
            tuple.isInert || tuples.any { it == tuple || LaunchValues.isLauncherMigration(it, tuple) }

        fun acceptsLocal(local: LaunchProjection.Local): Boolean =
            local.agent == null || this.local?.get("agent").stringOrNull() == local.agent

        companion object {
            fun of(record: JsonObject): Approved {
                val tuples = mutableListOf<LaunchTuple>()
                val identityFiles = mutableListOf<String>()
                (record["machines"] as? JsonObject)?.values?.forEach { machine ->
                    val entry = machine as? JsonObject ?: return@forEach
                    (entry["systems"] as? JsonObject)?.values?.forEach { LaunchTuple.from(it)?.let(tuples::add) }
                    entry["identityFile"].stringOrNull()?.let(identityFiles::add)
                }
                return Approved(tuples, identityFiles, record["githubRepo"].stringOrNull(), record["local"] as? JsonObject)
            }
        }
    }

    /** What the record holds at a system's place, exactly, or null when it holds nothing there. */
    private fun JsonObject.recordedSystem(machine: String, system: String): JsonElement? =
        (((this["machines"] as? JsonObject)?.get(machine) as? JsonObject)?.get("systems") as? JsonObject)?.get(system)

    private fun JsonObject.recordedIdentityFile(machine: String): JsonElement? =
        ((this["machines"] as? JsonObject)?.get(machine) as? JsonObject)?.get("identityFile")

    private fun JsonObject.at(change: LaunchChange): JsonElement? = when (change.scope) {
        LaunchScope.SYSTEM -> recordedSystem(change.machineId.orEmpty(), change.systemId.orEmpty())
        LaunchScope.IDENTITY_FILE -> recordedIdentityFile(change.machineId.orEmpty())
        LaunchScope.REPO -> this["githubRepo"]
        LaunchScope.LOCAL -> this["local"]
    }

    private fun JsonObject.with(change: LaunchChange): JsonObject = when (change.scope) {
        LaunchScope.SYSTEM -> withMachine(change.machineId.orEmpty()) { machine ->
            val systems = machine["systems"] as? JsonObject ?: JsonObject(emptyMap())
            machine + ("systems" to JsonObject(systems + (change.systemId.orEmpty() to change.value)))
        }

        LaunchScope.IDENTITY_FILE -> withMachine(change.machineId.orEmpty()) { machine ->
            machine + ("identityFile" to change.value)
        }

        LaunchScope.REPO -> JsonObject(this + ("githubRepo" to change.value))
        LaunchScope.LOCAL -> JsonObject(this + ("local" to change.value))
    }

    private fun JsonObject.withMachine(id: String, change: (JsonObject) -> Map<String, JsonElement>): JsonObject {
        val machines = this["machines"] as? JsonObject ?: JsonObject(emptyMap())
        val machine = machines[id] as? JsonObject ?: JsonObject(emptyMap())
        return JsonObject(this + ("machines" to JsonObject(machines + (id to JsonObject(change(machine))))))
    }
}

/** A JSON string as itself, and anything else, JSON null included, as nothing. */
internal fun JsonElement?.stringOrNull(): String? = (this as? JsonPrimitive)?.takeIf { it.isString }?.content

/** JSON `true`, and not the string "true". */
private fun JsonElement?.isTrue(): Boolean =
    (this as? JsonPrimitive)?.takeUnless { it.isString || it is JsonNull }?.booleanOrNull == true

/** JSON `false`, and not the string "false". */
private fun JsonElement?.isFalse(): Boolean =
    (this as? JsonPrimitive)?.takeUnless { it.isString || it is JsonNull }?.booleanOrNull == false

/** A system's launch value: the argv this device runs on it, and the grammar its line is written in. */
data class LaunchTuple(
    /** The argv, or null when the document names none, which makes the tuple inert. */
    val agent: List<String>?,
    /** Section 15.1's grammar, `refused:<code>` when it has none, and null for an inert tuple. */
    val grammar: String?,
) {
    val isInert: Boolean get() = agent == null

    /** The section 15.1 refusal code when [grammar] is `refused:<code>`, and null otherwise. */
    val grammarRefusal: String? get() = grammar?.takeIf { it.startsWith(REFUSED) }?.removePrefix(REFUSED)

    /**
     * Whether this tuple can be run: every argv item shown faithfully, and a grammar section 15.2
     * writes lines in. A tuple that cannot is never run, wherever it is recorded.
     */
    val canRun: Boolean
        get() = !agent.isNullOrEmpty() && agent.all(LaunchValues::isLaunchText) && RemoteShell.fromGrammar(grammar) != null

    /** The tuple as the record and the review hold it. */
    fun toJson(): JsonObject = buildJsonObject {
        put("agent", agent?.let { items -> JsonArray(items.map(::JsonPrimitive)) } ?: JsonNull)
        put("grammar", grammar?.let(::JsonPrimitive) ?: JsonNull)
    }

    companion object {
        val INERT = LaunchTuple(null, null)

        private const val REFUSED = "refused:"

        /** The tuple a record holds, or null when it is not one: a non-empty argv of strings and a grammar. */
        fun from(element: JsonElement?): LaunchTuple? {
            val tuple = element as? JsonObject ?: return null
            val items = (tuple["agent"] as? JsonArray)?.takeIf { it.isNotEmpty() } ?: return null
            val agent = items.map { it.stringOrNull() ?: return null }
            return LaunchTuple(agent, tuple["grammar"].stringOrNull() ?: return null)
        }
    }
}

/** The launch values of one document, as section 9.9 reads them. */
data class LaunchProjection(
    val machines: List<Machine>,
    /** The update repository, or null for the built-in one. */
    val repo: String?,
    /** The top-level `local`, or null for none. */
    val local: Local?,
) {
    data class Machine(val id: String, val systems: List<System>, val identityFile: String?) {
        fun system(id: String): System? = systems.firstOrNull { it.id == id }
    }

    data class System(val id: String, val tuple: LaunchTuple)

    /** `local`, whose [agent] is null for the default path. */
    data class Local(val agent: String?) {
        val canRun: Boolean get() = agent == null || LaunchValues.isLaunchText(agent)

        fun toJson(): JsonObject = buildJsonObject { put("agent", agent?.let(::JsonPrimitive) ?: JsonNull) }
    }

    fun machine(id: String): Machine? = machines.firstOrNull { it.id == id }

    companion object {
        val EMPTY = LaunchProjection(emptyList(), null, null)
    }
}

/** Which kind of launch value a path holds. */
enum class LaunchScope(val wire: String) {
    SYSTEM("system"),
    IDENTITY_FILE("identityFile"),
    REPO("repo"),
    LOCAL("local"),
}

/** One value written into the record at its place. */
data class LaunchChange(
    val scope: LaunchScope,
    val machineId: String?,
    val systemId: String?,
    /** Exactly what the record holds there afterwards. */
    val value: JsonElement,
)

/** One offered launch value that waits for this device's approval. */
data class LaunchReviewEntry(
    val path: String,
    val scope: LaunchScope,
    /** The machine the value sits on, for a system and an identityFile. Never parsed out of [path]. */
    val machineId: String?,
    /** The system, for a tuple. */
    val systemId: String?,
    /** False for a value that can only be reverted or left, never approved. */
    val approvable: Boolean,
    /** What the record holds at [path], exactly, or null when it holds nothing there. */
    val approved: JsonElement?,
    /** The offered value, exactly as approving it records it. */
    val offered: JsonElement,
) {
    /** `changed` when the record holds a value at [path], JSON null included, and `new` otherwise. */
    val kind: String get() = if (approved == null) "new" else "changed"

    val offeredTuple: LaunchTuple? get() = LaunchTuple.from(offered).takeIf { scope == LaunchScope.SYSTEM }

    val approvedTuple: LaunchTuple? get() = LaunchTuple.from(approved).takeIf { scope == LaunchScope.SYSTEM }

    /** Approving this entry: exactly the value it showed, at its place. */
    fun change(): LaunchChange = LaunchChange(scope, machineId, systemId, offered)
}

/** What this device runs at one place, and why. */
data class EffectiveLaunch(
    val path: String,
    val scope: LaunchScope,
    val machineId: String?,
    val systemId: String?,
    val state: State,
    /** The tuple a system runs, or null when it runs nothing. */
    val tuple: LaunchTuple?,
    /** The identityFile, the repository or local.agent; null for the built-in repository and local's default path. */
    val text: String?,
) {
    enum class State(val wire: String) {
        /** The offered value, accepted without asking. */
        OFFERED("offered"),

        /** The value this device approved at the same place, while the offered one waits. */
        APPROVED("approved"),

        /** A system with nothing it may run. */
        NOT_DRIVEN("not-driven"),

        /** The repository built into the app. */
        BUILT_IN("built-in"),

        /** No identityFile, or no local. */
        NONE("none"),
    }
}
