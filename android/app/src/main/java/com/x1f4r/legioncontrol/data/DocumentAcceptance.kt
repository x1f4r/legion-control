package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.agent.Addresses
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.net.LanPrefix

/**
 * Where a setup document comes from, which decides what happens to one that fails (contract section
 * 9.7): a document read from a machine, a kept revision or a merge result is refused, and so is one
 * typed or pasted here once it has an identity; the document this device already runs on is kept as
 * long as section 12 accepts its bytes.
 */
enum class Intake(val wire: String) {
    PEER("peer"),
    LOCAL("local"),
    APPLIED("applied"),
}

/** One thing section 9.7 says about a document: its code, where it is, and a sentence about it. */
data class DocumentProblem(val code: String, val path: String, val message: String)

/**
 * Which setup documents this app accepts, and what it cannot use in the ones it does.
 *
 * Exactly the documents `node contract/validate.mjs` accepts, no more and no fewer. This app used to
 * refuse documents the schema allows, a machine with no systems yet or an endpoint with no id, and a
 * client that refuses its peer's document stops replicating without anybody noticing (C3). So a
 * document is refused for a broken schema constraint or one of the ten relationship rules, and for
 * nothing else; everything this app cannot use is a warning with a code, so a machine can be
 * explained rather than the whole setup turned away.
 *
 * A port of `checkDocument` in `contract/tools/document-rules.mjs`, held to
 * `contract/vectors/setup-acceptance.json`. The schema constraints are written out by hand, as the
 * reference writes them, and the string grammars are the schema's own patterns: a test holds the
 * ones below to `contract/schemas`, character for character.
 */
object DocumentAcceptance {

    /**
     * The setup id local intake checks a document under when it has none. A device then gives it a
     * fresh `setup-<uuid>`; any valid setup id gives the same verdict, so the check uses one fixed
     * value and never depends on a random source.
     */
    const val ASSIGNED_SETUP_ID = "setup-00000000-0000-4000-8000-000000000000"

    /**
     * What section 9.7 says of one document under one intake.
     *
     * [keep] is whether this device goes on using it: a refused peer or local document is not used,
     * an applied one is used with its errors shown, and only a text section 12 refuses is not even
     * that. [document] is the document as `JSON.parse` reads its canonical text, before any identity
     * local intake would give it, or null when section 12 refuses the text.
     */
    data class Verdict(
        val valid: Boolean,
        val errors: List<DocumentProblem>,
        val warnings: List<DocumentProblem>,
        val assignsIdentity: Boolean,
        val keep: Boolean,
        val document: SetupJson.Object?,
    ) {
        /**
         * The machines an error is inside, by their index in `machines`, with the first such error.
         * Under applied intake each of them is disabled: nothing is dialled or run on it.
         */
        val machineErrors: Map<Int, DocumentProblem>
            get() = buildMap {
                for (problem in errors) {
                    val index = MACHINE_PATH.find(problem.path)?.groupValues?.get(1)?.toIntOrNull() ?: continue
                    if (index !in this) put(index, problem)
                }
            }
    }

    /** Section 9.7 for a document as text. */
    fun check(text: String, intake: Intake): Verdict {
        val canonical = CanonicalSetup.canonicalText(text)
        val rejection = CanonicalSetup.rejectionOf(text)
        return checkCanonical(canonical, rejection, intake)
    }

    /** Section 9.7 for a document as the bytes it came in. */
    fun check(bytes: ByteArray, intake: Intake): Verdict {
        val rejection = CanonicalSetup.rejectionOf(bytes)
        val canonical = if (rejection == null) CanonicalSetup.canonicalTextOrNull(bytes) else null
        return checkCanonical(canonical, rejection, intake)
    }

    /**
     * The order is fixed. Section 12 comes first: a text it refuses is no document at all, so it
     * breaks the schema before any rule can be read, under every intake, and even applied intake
     * does not keep it. Then a repeated member name, which when it applies is the only thing
     * reported. Then local intake's identity, then the schema and the rules.
     */
    private fun checkCanonical(canonical: String?, rejection: String?, intake: Intake): Verdict {
        if (rejection != null || canonical == null) {
            val problem = DocumentProblem("schema", "", sectionTwelve(rejection ?: "not-a-json-object"))
            return Verdict(false, listOf(problem), emptyList(), assignsIdentity = false, keep = false, document = null)
        }
        val parsed = SetupJsonParser.parseAllowingRepeats(canonical)
        val document = parsed.root as SetupJson.Object
        parsed.repeated?.let { path ->
            val problem = DocumentProblem(
                "duplicate-member",
                path,
                "$path is written twice in one object, and parsers disagree about which copy counts.",
            )
            return Verdict(false, listOf(problem), emptyList(), false, keep = intake == Intake.APPLIED, document = document)
        }
        val assignsIdentity = intake == Intake.LOCAL && needsIdentity(document)
        val checked = if (assignsIdentity) withAssignedIdentity(document) else document
        val errors = unique(schemaProblems(checked) + ruleProblems(checked))
        val warnings = if (errors.isEmpty()) unique(warningProblems(checked)) else emptyList()
        val valid = errors.isEmpty()
        return Verdict(valid, errors, warnings, assignsIdentity, keep = intake == Intake.APPLIED || valid, document = document)
    }

    /**
     * Whether local intake gives this document an identity before it is checked: when it has no
     * `controller` block, or a block without an id. A `controller.id` of null is no id.
     */
    fun needsIdentity(document: SetupJson.Object): Boolean {
        val block = document["controller"] ?: return true
        if (block !is SetupJson.Object) return false
        val id = block["id"]
        return id == null || id == SetupJson.Null
    }

    private fun withAssignedIdentity(document: SetupJson.Object): SetupJson.Object {
        val copy = document.copy() as SetupJson.Object
        val block = (copy["controller"] as? SetupJson.Object) ?: SetupJson.Object()
        block["id"] = SetupJson.Text(ASSIGNED_SETUP_ID)
        block["revision"] = SetupJson.Number("1")
        block["lineage"] = SetupJson.Array()
        copy["controller"] = block
        return copy
    }

    private fun sectionTwelve(code: String): String = when (code) {
        "too-large" -> "The setup is larger than 1 MiB, which is more than any machine stores."
        "invalid-utf8" -> "The setup is not valid UTF-8 text."
        "empty" -> "There is nothing to apply. Paste a configuration, or insert the example."
        else -> "That is not a configuration this app can read: a setup is one JSON object with a version and a list of machines."
    }

    // MARK: - The string grammars of the schemas

    /** `common.schema.json#/$defs/sshHostGuard`, 1 to 253 characters. */
    internal const val HOST_GUARD_PATTERN =
        "^(?:[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?(?:\\.[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?)*\\.?" +
            "|[0-9A-Fa-f.]*:[0-9A-Fa-f:.]*(?:%[A-Za-z0-9._-]{1,32})?)$"

    /** `common.schema.json#/$defs/sshUser`. */
    internal const val USER_GUARD_PATTERN =
        "^(?=[^\\n]{1,64}$)(?!.* [ -])[A-Za-z0-9_](?:[A-Za-z0-9._@\\\\ -]*[A-Za-z0-9._-])?$"

    /** `common.schema.json#/$defs/agentArgItem`, 1 to 4096 characters and at most 4096 UTF-8 bytes. */
    internal const val AGENT_ARG_PATTERN = "^[^\\u0000-\\u001f\\u007f]+$"

    /** `common.schema.json#/$defs/githubRepo`. */
    internal const val GITHUB_REPO_PATTERN =
        "^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?/(?!\\.\\.?$)[A-Za-z0-9._-]{1,100}$"

    /** `controller-document.schema.json#/$defs/wake/properties/mac`. */
    internal const val MAC_PATTERN = "^[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}$"

    private val HOST_GUARD = Regex(HOST_GUARD_PATTERN)
    private val USER_GUARD = Regex(USER_GUARD_PATTERN)
    private val AGENT_ARG = Regex(AGENT_ARG_PATTERN)
    private val GITHUB_REPO = Regex(GITHUB_REPO_PATTERN)
    private val MAC = Regex(MAC_PATTERN)

    /** The argv-item limit a schema cannot count. */
    private const val ARG_ITEM_BYTES = 4096

    private val SOURCES = listOf("mac", "desktop", "phone", "cli", "legacy")
    private val PLATFORMS = listOf("linux", "windows", "mac")
    private val SHELLS = listOf("posix", "powershell", "cmd")
    private val KINDS = listOf("lan", "remote")

    private val MACHINE_PATH = Regex("^machines\\[([0-9]+)](?:[.\\[]|$)")

    /**
     * The matching rule every implementation shares: the whole value, ASCII classes, a line feed or
     * carriage return refused before any pattern runs, and lengths counted in code points.
     */
    private fun matches(pattern: Regex, value: String, minLength: Int = 0, maxLength: Int = Int.MAX_VALUE): Boolean {
        if ('\n' in value || '\r' in value) return false
        val length = value.codePointCount(0, value.length)
        return length in minLength..maxLength && pattern.matches(value)
    }

    private fun isHostGuard(value: String) = matches(HOST_GUARD, value, 1, 253)
    private fun isUserGuard(value: String) = matches(USER_GUARD, value)
    private fun isArgItem(value: String) = matches(AGENT_ARG, value, 1, 4096)
    private fun isRepo(value: String) = matches(GITHUB_REPO, value)
    private fun isMac(value: String) = matches(MAC, value)

    // MARK: - Reading a tree the way the reference reads a parsed document

    private fun textOf(node: SetupJson?): String? = (node as? SetupJson.Text)?.value

    private fun isText(node: SetupJson?): Boolean = textOf(node)?.isNotEmpty() == true

    /** `Number.isInteger`: a number whose value as a double is whole. */
    private fun isInteger(node: SetupJson?): Boolean {
        val value = (node as? SetupJson.Number)?.lexeme?.toDoubleOrNull() ?: return false
        return value.isFinite() && value == Math.floor(value)
    }

    private fun numberValue(node: SetupJson?): Double? = (node as? SetupJson.Number)?.lexeme?.toDoubleOrNull()

    private fun isPortNumber(node: SetupJson?): Boolean =
        isInteger(node) && numberValue(node)!! in 1.0..65535.0

    private fun arrayOf(node: SetupJson?): List<SetupJson> = (node as? SetupJson.Array)?.items.orEmpty()

    private fun objectsOf(node: SetupJson?): List<SetupJson.Object> = arrayOf(node).filterIsInstance<SetupJson.Object>()

    /** A value for a sentence: a string in quotes, a number as written, anything else by its kind. */
    private fun show(node: SetupJson?): String = when (node) {
        null -> "nothing"
        is SetupJson.Text -> SetupJson.quote(node.value)
        is SetupJson.Number -> node.lexeme
        is SetupJson.Bool -> node.value.toString()
        SetupJson.Null -> "null"
        is SetupJson.Object -> "an object"
        is SetupJson.Array -> "a list"
    }

    /** JavaScript's `===` between two values of a document: by value for scalars, never for two containers. */
    private fun strictlyEqual(a: SetupJson?, b: SetupJson?): Boolean = when {
        a == null || b == null -> a == null && b == null
        a is SetupJson.Text && b is SetupJson.Text -> a.value == b.value
        a is SetupJson.Number && b is SetupJson.Number -> numberValue(a) == numberValue(b)
        a is SetupJson.Bool && b is SetupJson.Bool -> a.value == b.value
        a == SetupJson.Null && b == SetupJson.Null -> true
        else -> false
    }

    private fun join(base: String, key: String) = SetupJsonParser.join(base, key)

    /** Each problem once, in the order found. Two rules can describe the same fault from two ends. */
    private fun unique(problems: List<DocumentProblem>): List<DocumentProblem> = problems.distinct()

    /**
     * Every wake helper of a machine: the entries of `wake.helpers` and the alias `wake.helper`. Both
     * are checked, because clients disagree about which one counts when `helpers` is empty, and a
     * helper some client would run must name a machine that exists and must not lead back to the
     * machine it wakes.
     */
    private fun helpersOf(machine: SetupJson?): List<SetupJson.Object> {
        val wake = (machine as? SetupJson.Object)?.get("wake") as? SetupJson.Object ?: return emptyList()
        val list = arrayOf(wake["helpers"]).toMutableList()
        wake["helper"]?.let(list::add)
        return list.filterIsInstance<SetupJson.Object>().filter { it["machine"] is SetupJson.Text }
    }

    // MARK: - The schema, written out (code `schema`)

    private class Problems(private val code: String) {
        val list = mutableListOf<DocumentProblem>()
        fun add(path: String, message: String) {
            list += DocumentProblem(code, path, message)
        }
    }

    private fun schemaProblems(document: SetupJson.Object): List<DocumentProblem> {
        val problems = Problems("schema")
        fun add(path: String, message: String) = problems.add(path, message)
        fun subject(path: String) = if (path.isEmpty()) "The document" else path

        /** An optional string key held to [rule]. */
        fun string(holder: SetupJson.Object, key: String, path: String, what: String, rule: (String) -> Boolean) {
            if (!holder.has(key)) return
            val value = holder[key]
            if (textOf(value)?.let(rule) != true) add(join(path, key), "${join(path, key)} is ${show(value)}, which is not $what.")
        }

        /** An optional array key: its type and limits, then [each] on every item. */
        fun list(holder: SetupJson.Object, key: String, path: String, min: Int = 0, max: Int, each: (SetupJson, String) -> Unit) {
            if (!holder.has(key)) return
            val value = holder[key]
            val where = join(path, key)
            if (value !is SetupJson.Array) {
                add(where, "$where is not a list.")
                return
            }
            if (value.items.size < min) add(where, "$where has ${value.items.size} entries and needs at least $min.")
            if (value.items.size > max) add(where, "$where has ${value.items.size} entries, and at most $max are allowed.")
            value.items.forEachIndexed { index, item -> each(item, "$where[$index]") }
        }

        fun required(holder: SetupJson.Object, keys: List<String>, path: String) {
            for (key in keys) if (!holder.has(key)) add(join(path, key), "${join(path, key)} is required.")
        }

        fun objectAt(value: SetupJson?, path: String): SetupJson.Object? {
            if (value is SetupJson.Object) return value
            add(path, "${subject(path)} is not an object.")
            return null
        }

        fun port(holder: SetupJson.Object, key: String, path: String) {
            if (holder.has(key) && !isPortNumber(holder[key])) {
                add(join(path, key), "${join(path, key)} is ${show(holder[key])}, which is not a port: a whole number from 1 to 65535.")
            }
        }

        fun nonEmptyString(value: SetupJson, path: String, what: String) {
            if (!isText(value)) add(path, "$path is ${show(value)}, which is not $what.")
        }

        fun oneOf(holder: SetupJson.Object, key: String, path: String, allowed: List<String>) {
            if (holder.has(key) && textOf(holder[key]) !in allowed) {
                add(join(path, key), "${join(path, key)} is ${show(holder[key])}; it has to be one of ${allowed.joinToString(", ")}.")
            }
        }

        fun boolean(holder: SetupJson.Object, key: String, path: String) {
            if (holder.has(key) && holder[key] !is SetupJson.Bool) add(join(path, key), "${join(path, key)} is not true or false.")
        }

        if (!document.has("version")) {
            add("version", "version is required, and it is 1.")
        } else if (numberValue(document["version"]) != 1.0) {
            add("version", "This setup says version ${show(document["version"])}, and every client reads version 1.")
        }
        if (!document.has("machines")) add("machines", "machines is required.")

        if (document.has("controller")) {
            objectAt(document["controller"], "controller")?.let { block ->
                required(block, listOf("id", "revision"), "controller")
                string(
                    block, "id", "controller",
                    "a setup id: letters, digits and . _ : -, starting with a letter or a digit, at most 64 characters",
                    Contract::isSetupId,
                )
                string(block, "name", "controller", "a name") { it.isNotEmpty() }
                if (block.has("revision") && !(isInteger(block["revision"]) && numberValue(block["revision"])!! >= 0)) {
                    add(
                        "controller.revision",
                        "The setup revision is ${show(block["revision"])}. It is a whole number that counts up from zero.",
                    )
                }
                if (block.has("updatedAt") && block["updatedAt"] !is SetupJson.Text) {
                    add("controller.updatedAt", "controller.updatedAt is not text.")
                }
                if (block.has("source") && textOf(block["source"]) !in SOURCES) {
                    add(
                        "controller.source",
                        "The setup says it was written by ${show(block["source"])}. That has to be one of " +
                            "${SOURCES.joinToString(", ")}; the device itself goes in device.",
                    )
                }
                string(block, "device", "controller", "a device name") { it.isNotEmpty() }
                list(block, "lineage", "controller", max = MAX_LINEAGE) { hash, path ->
                    if (textOf(hash)?.let(Contract::isSha256Hex) != true) {
                        add(path, "${show(hash)} in the setup's ancestry is not a sha256 hash.")
                    }
                }
                (block["lineage"] as? SetupJson.Array)?.let { lineage ->
                    val seen = mutableSetOf<String>()
                    lineage.items.forEachIndexed { index, hash ->
                        val key = textOf(hash)?.let { "text:$it" }
                            ?: (hash as? SetupJson.Number)?.let { "number:${JsonEquality.decimalKey(it.lexeme)}" }
                            ?: show(hash)
                        if (!seen.add(key)) {
                            add(
                                "controller.lineage[$index]",
                                "The setup's ancestry lists ${show(hash)} twice; a document cannot descend from one ancestor twice.",
                            )
                        }
                    }
                }
            }
        }

        list(document, "sites", "", max = 20) { value, path ->
            val site = objectAt(value, path) ?: return@list
            required(site, listOf("id"), path)
            string(site, "id", path, "an identifier", Contract::isToken)
            string(site, "name", path, "a name") { it.isNotEmpty() }
            list(site, "lanPrefixes", path, min = 1, max = 10) { prefix, at -> nonEmptyString(prefix, at, "a prefix") }
            list(site, "broadcast", path, max = 10) { address, at -> nonEmptyString(address, at, "a broadcast address") }
        }

        fun helper(value: SetupJson, path: String) {
            val entry = objectAt(value, path) ?: return
            required(entry, listOf("machine", "action"), path)
            string(entry, "machine", path, "a machine id", Contract::isToken)
            string(
                entry, "action", path,
                "an action id: letters, digits and . _ -, starting with a letter or a digit",
                Contract::isConfiguredId,
            )
        }

        list(document, "machines", "", max = 50) { value, path ->
            val machine = objectAt(value, path) ?: return@list
            required(machine, listOf("id"), path)
            string(machine, "id", path, "an identifier", Contract::isToken)
            string(machine, "name", path, "a name") { it.isNotEmpty() }
            string(machine, "site", path, "a site id", Contract::isToken)
            boolean(machine, "alwaysOn", path)

            list(machine, "endpoints", path, max = 20) { item, at ->
                val endpoint = objectAt(item, at) ?: return@list
                required(endpoint, listOf("host"), at)
                string(endpoint, "id", at, "an identifier", Contract::isToken)
                oneOf(endpoint, "kind", at, KINDS)
                string(endpoint, "host", at, "a host name or address", ::isHostGuard)
                port(endpoint, "port", at)
                string(endpoint, "user", at, "a login name", ::isUserGuard)
                string(endpoint, "system", at, "a system id", Contract::isToken)
                string(endpoint, "label", at, "a label") { it.isNotEmpty() }
            }

            if (machine.has("wake")) {
                val where = join(path, "wake")
                objectAt(machine["wake"], where)?.let { wake ->
                    string(wake, "mac", where, "a hardware address: six hex pairs, separated by colons", ::isMac)
                    list(wake, "broadcast", where, max = 10) { address, at -> nonEmptyString(address, at, "a broadcast address") }
                    list(wake, "ports", where, max = 5) { item, at ->
                        if (!isPortNumber(item)) add(at, "$at is ${show(item)}, which is not a port: a whole number from 1 to 65535.")
                    }
                    if (wake.has("probe")) {
                        val probePath = join(where, "probe")
                        objectAt(wake["probe"], probePath)?.let { probe ->
                            required(probe, listOf("host"), probePath)
                            string(probe, "host", probePath, "a host name or address", ::isHostGuard)
                            port(probe, "port", probePath)
                        }
                    }
                    string(wake, "lanPrefix", where, "a prefix") { it.isNotEmpty() }
                    if (wake.has("helper")) helper(wake["helper"]!!, join(where, "helper"))
                    list(wake, "helpers", where, max = 5) { item, at -> helper(item, at) }
                }
            }

            list(machine, "systems", path, max = 10) { item, at ->
                val system = objectAt(item, at) ?: return@list
                required(system, listOf("id"), at)
                string(system, "id", at, "an identifier", Contract::isToken)
                string(system, "name", at, "a name") { it.isNotEmpty() }
                oneOf(system, "platform", at, PLATFORMS)
                oneOf(system, "shell", at, SHELLS)
                boolean(system, "restricted", at)
                list(system, "agent", at, max = 20) { argument, where ->
                    val text = textOf(argument)
                    if (text == null || !isArgItem(text)) {
                        add(
                            where,
                            "$where is ${show(argument)}, which is not an argv item: one that is not empty and holds no control character.",
                        )
                    } else if (text.toByteArray(Charsets.UTF_8).size > ARG_ITEM_BYTES) {
                        add(
                            where,
                            "$where is ${text.toByteArray(Charsets.UTF_8).size} UTF-8 bytes, over the $ARG_ITEM_BYTES every " +
                                "dispatcher and client allows.",
                        )
                    }
                }
            }

            if (machine.has("ssh")) {
                val where = join(path, "ssh")
                objectAt(machine["ssh"], where)?.let { ssh ->
                    string(ssh, "host", where, "a host name or address", ::isHostGuard)
                    string(ssh, "user", where, "a login name", ::isUserGuard)
                    port(ssh, "port", where)
                    string(ssh, "identityFile", where, "a path") { it.isNotEmpty() }
                }
            }
        }

        if (document.has("appUpdates")) {
            objectAt(document["appUpdates"], "appUpdates")?.let { updates ->
                required(updates, listOf("githubRepo"), "appUpdates")
                string(updates, "githubRepo", "appUpdates", "owner/name as GitHub spells them", ::isRepo)
            }
        }
        for (key in listOf("local", "ssh")) {
            val value = document[key]
            if (document.has(key) && value != SetupJson.Null && value !is SetupJson.Object) add(key, "$key is neither an object nor null.")
        }
        return problems.list
    }

    // MARK: - The relationship rules (errors)

    private fun ruleProblems(document: SetupJson.Object): List<DocumentProblem> {
        val problems = mutableListOf<DocumentProblem>()
        fun add(code: String, path: String, message: String) {
            problems += DocumentProblem(code, path, message)
        }
        val machines = arrayOf(document["machines"])
        val sites = arrayOf(document["sites"])

        val machineIds = mutableSetOf<String>()
        machines.forEachIndexed { index, machine ->
            if (machine !is SetupJson.Object) return@forEachIndexed
            val id = textOf(machine["id"]) ?: return@forEachIndexed
            if (!machineIds.add(id)) {
                add("duplicate-machine", "machines[$index].id", "Two machines share the id \"$id\". Ids have to be unique.")
            }

            // Endpoints without an id are never duplicates: they cannot be remembered as a route,
            // and that is a warning, not a reason to refuse the setup.
            val endpointIds = mutableSetOf<String>()
            arrayOf(machine["endpoints"]).forEachIndexed { k, endpoint ->
                val endpointId = textOf((endpoint as? SetupJson.Object)?.get("id")) ?: return@forEachIndexed
                if (!endpointIds.add(endpointId)) {
                    add(
                        "duplicate-endpoint",
                        "machines[$index].endpoints[$k].id",
                        "$id lists the endpoint \"$endpointId\" twice, so a remembered route is ambiguous.",
                    )
                }
            }

            val systemIds = mutableSetOf<String>()
            arrayOf(machine["systems"]).forEachIndexed { k, system ->
                val systemId = textOf((system as? SetupJson.Object)?.get("id")) ?: return@forEachIndexed
                if (!systemIds.add(systemId)) {
                    add("duplicate-system", "machines[$index].systems[$k].id", "$id lists the system \"$systemId\" twice.")
                }
            }
        }

        val siteIds = mutableSetOf<String>()
        sites.forEachIndexed { index, site ->
            val id = textOf((site as? SetupJson.Object)?.get("id")) ?: return@forEachIndexed
            if (!siteIds.add(id)) add("duplicate-site", "sites[$index].id", "Two sites share the id \"$id\". Ids have to be unique.")
        }

        machines.forEachIndexed { index, value ->
            val machine = value as? SetupJson.Object ?: return@forEachIndexed
            val id = machine["id"]
            val name = textOf(id) ?: "machines[$index]"
            // A setup without sites places nobody, so a site name is only checked against a list
            // that exists.
            val site = textOf(machine["site"])
            if (site != null && sites.isNotEmpty() && site !in siteIds) {
                add(
                    "unknown-site",
                    "machines[$index].site",
                    "$name says it is at the site \"$site\", and this setup has no site with that id, so it drops out of " +
                        "every on-site decision.",
                )
            }
            for (helper in helpersOf(machine)) {
                val target = textOf(helper["machine"])!!
                if (strictlyEqual(helper["machine"], id)) {
                    add(
                        "helper-self",
                        "machines[$index].wake",
                        "$name names itself as its own wake helper. A machine that is asleep cannot send its own magic packet.",
                    )
                } else if (target !in machineIds) {
                    add(
                        "helper-unknown",
                        "machines[$index].wake",
                        "$name names \"$target\" as a wake helper, and there is no machine with that id in this setup.",
                    )
                }
            }
            // The singular key is what a 1.2 client reads. If the two disagree, an old and a new
            // client try different machines from the same document.
            val wake = machine["wake"] as? SetupJson.Object
            val alias = wake?.get("helper") as? SetupJson.Object
            val first = (wake?.get("helpers") as? SetupJson.Array)?.items?.firstOrNull() as? SetupJson.Object
            if (alias != null && first != null &&
                (!strictlyEqual(first["machine"], alias["machine"]) || !strictlyEqual(first["action"], alias["action"]))
            ) {
                add(
                    "helper-alias",
                    "machines[$index].wake.helper",
                    "$name lists wake helpers and also a single \"helper\" that is not the first of them. The single one " +
                        "exists so an older client can still read the list, so it has to repeat the first.",
                )
            }
        }

        // A ring of helpers is the failure that looks fine one machine at a time: every machine in it
        // waits for another to be awake first.
        val byId = linkedMapOf<String, SetupJson.Object>()
        for (value in machines) {
            val machine = value as? SetupJson.Object ?: continue
            val id = textOf(machine["id"]) ?: continue
            byId.putIfAbsent(id, machine)
        }
        machines.forEachIndexed { index, value ->
            val machine = value as? SetupJson.Object ?: return@forEachIndexed
            val id = textOf(machine["id"]) ?: return@forEachIndexed
            val seen = mutableSetOf(id)
            fun walk(current: String, trail: List<String>) {
                for (helper in helpersOf(byId[current])) {
                    val next = textOf(helper["machine"])!!
                    if (next in seen) {
                        if (next == id && trail.isNotEmpty()) {
                            add(
                                "helper-cycle",
                                "machines[$index].wake",
                                "These machines wake each other in a circle: ${(trail + current + next).joinToString(" needs ")}. " +
                                    "One of them has to be reachable without a helper.",
                            )
                        }
                        continue
                    }
                    if (next !in byId) continue
                    seen += next
                    walk(next, trail + current)
                }
            }
            walk(id, emptyList())
        }
        return problems
    }

    // MARK: - What this app cannot use (warnings), only read for a document with no error

    private fun warningProblems(document: SetupJson.Object): List<DocumentProblem> {
        val warnings = mutableListOf<DocumentProblem>()
        fun add(code: String, path: String, message: String) {
            warnings += DocumentProblem(code, path, message)
        }
        val machines = objectsOf(document["machines"])
        val siteIds = objectsOf(document["sites"]).mapNotNull { textOf(it["id"]) }.toSet()
        val byId = linkedMapOf<String, SetupJson.Object>()
        for (machine in machines) textOf(machine["id"])?.let { byId.putIfAbsent(it, machine) }

        if (machines.isEmpty()) add("no-machines", "machines", "The setup has no machines yet, so there is nothing to drive.")

        /** The dial grammar of section 15.6, on a value the guard already passed. */
        fun dialable(holder: SetupJson.Object, path: String, owner: String) {
            textOf(holder["host"])?.let { host ->
                if (Addresses.hostForm(host) == null) {
                    add(
                        "endpoint-undialable",
                        join(path, "host"),
                        "$owner: \"$host\" is not a host this app dials, scans or pins a key for, so that address is skipped.",
                    )
                }
            }
            textOf(holder["user"])?.let { user ->
                if (!Addresses.isUser(user)) {
                    add(
                        "endpoint-undialable",
                        join(path, "user"),
                        "$owner: \"$user\" is not a user this app logs in as, so that address is skipped.",
                    )
                }
            }
            if (holder.has("port") && !isPortNumber(holder["port"])) {
                add(
                    "endpoint-undialable",
                    join(path, "port"),
                    "$owner: ${show(holder["port"])} is not a port this app dials, so that address is skipped.",
                )
            }
        }

        machines.forEachIndexed { index, machine ->
            val path = "machines[$index]"
            val id = textOf(machine["id"]).orEmpty()
            val name = textOf(machine["name"])?.takeIf { it.isNotBlank() } ?: id
            val systems = objectsOf(machine["systems"])
            val endpoints = objectsOf(machine["endpoints"])
            val systemIds = systems.mapNotNull { textOf(it["id"]) }.toSet()
            val legacyHost = (machine["ssh"] as? SetupJson.Object)?.get("host") is SetupJson.Text

            if (systems.isEmpty()) add("machine-no-systems", path, "$name has no systems, so nothing on it can be driven yet.")
            if (endpoints.isEmpty() && !legacyHost) add("machine-no-endpoints", path, "$name has no addresses, so nothing can reach it.")

            systems.forEachIndexed { k, system ->
                val agent = system["agent"]
                if (agent !is SetupJson.Array || agent.items.isEmpty()) {
                    val systemName = textOf(system["name"]) ?: textOf(system["id"]).orEmpty()
                    add("system-no-agent", "$path.systems[$k]", "$systemName on $name has no agent command, so it is not driven.")
                }
            }

            endpoints.forEachIndexed { k, endpoint ->
                val at = "$path.endpoints[$k]"
                if (!endpoint.has("id")) add("endpoint-no-id", at, "An address on $name has no id, so it cannot be remembered as a route.")
                if (!endpoint.has("user")) add("endpoint-no-user", at, "An address on $name has no user to log in as.")
                textOf(endpoint["system"])?.let { hint ->
                    if (hint !in systemIds) {
                        add(
                            "endpoint-unknown-system",
                            join(at, "system"),
                            "An address on $name points at the system \"$hint\", which $name does not have, so the hint is ignored.",
                        )
                    }
                }
                dialable(endpoint, at, name)
            }
            (machine["ssh"] as? SetupJson.Object)?.let { dialable(it, "$path.ssh", name) }

            val wake = machine["wake"] as? SetupJson.Object ?: return@forEachIndexed
            val helpers = helpersOf(machine)
            if (!wake.has("mac")) add("wake-no-mac", "$path.wake", "$name has no hardware address to wake, so only a helper can wake it.")
            val site = textOf(machine["site"])
            val namesSite = site != null && site in siteIds
            if (helpers.isEmpty() && !namesSite && LanPrefix.parse(textOf(wake["lanPrefix"])) == null) {
                add(
                    "wake-unplaced",
                    "$path.wake",
                    "$name can be woken, and nothing says which network it is on and no helper is configured, so no device " +
                        "can tell when it is on its network.",
                )
            }
            if (wake["helper"] is SetupJson.Object && (wake["helpers"] as? SetupJson.Array)?.items?.isEmpty() == true) {
                add(
                    "helper-alias-unused",
                    "$path.wake.helper",
                    "$name has a single wake \"helper\" beside an empty helpers list, which some clients read as no helper " +
                        "at all. Write it into helpers as well.",
                )
            }
            for (helper in helpers) {
                val helperId = textOf(helper["machine"])!!
                val target = byId[helperId] ?: continue
                val targetName = textOf(target["name"])?.takeIf { it.isNotBlank() } ?: helperId
                val targetSite = textOf(target["site"])
                if (site != null && targetSite != null && site != targetSite) {
                    add(
                        "helper-other-site",
                        "$path.wake",
                        "$targetName is at a different site than $name, so its action only wakes $name if it reaches the " +
                            "other network another way, through a router or a VPN.",
                    )
                }
                if ((target["alwaysOn"] as? SetupJson.Bool)?.value != true) {
                    add(
                        "helper-not-always-on",
                        "$path.wake",
                        "$targetName is not marked as always on, so waking $name may need $targetName woken first.",
                    )
                }
            }
        }
        return warnings + addressWarnings(document)
    }

    /**
     * The warnings that come from addresses: `shared-prefix`, `prefix-invalid`, `prefix-without-dot`,
     * `broadcast-invalid` and `wake-broadcast-defaulted`, in document order. A port of
     * `prefixWarnings` in `contract/tools/wake-plan.mjs`.
     */
    private fun addressWarnings(document: SetupJson.Object): List<DocumentProblem> {
        val warnings = mutableListOf<DocumentProblem>()
        fun add(code: String, path: String, message: String) {
            warnings += DocumentProblem(code, path, message)
        }
        val sites = arrayOf(document["sites"])
        val machines = arrayOf(document["machines"])

        fun checkPrefix(text: String?, path: String): LanPrefix? {
            val parsed = LanPrefix.parse(text)
            if (parsed == null) {
                add(
                    "prefix-invalid",
                    path,
                    "\"${text.orEmpty()}\" is not a network prefix such as 192.168.1. or 192.168.1.0/24, so it never matches an address.",
                )
            } else if (parsed.form == LanPrefix.Form.DOTTED && text?.endsWith('.') == false) {
                add("prefix-without-dot", path, withoutDot(text))
            }
            return parsed
        }

        fun checkBroadcasts(list: SetupJson?, path: String) {
            (list as? SetupJson.Array)?.items?.forEachIndexed { index, entry ->
                if (LanPrefix.parseIPv4(textOf(entry)) == null) {
                    add(
                        "broadcast-invalid",
                        "$path[$index]",
                        "${show(entry)} is not a dotted IPv4 address, so no packet is sent to it and it is never looked up as a name.",
                    )
                }
            }
        }

        fun validBroadcasts(list: SetupJson?): List<String> =
            arrayOf(list).mapNotNull { textOf(it) }.filter { LanPrefix.parseIPv4(it) != null }

        // Each site's prefixes and broadcasts, remembering the valid prefixes for the overlap check.
        val parsedBySite = sites.mapIndexed { index, value ->
            val site = value as? SetupJson.Object ?: return@mapIndexed emptyList<Pair<String, LanPrefix>>()
            val parsed = mutableListOf<Pair<String, LanPrefix>>()
            (site["lanPrefixes"] as? SetupJson.Array)?.items?.forEachIndexed { k, entry ->
                checkPrefix(textOf(entry), "sites[$index].lanPrefixes[$k]")?.let { parsed += textOf(entry)!! to it }
            }
            checkBroadcasts(site["broadcast"], "sites[$index].broadcast")
            parsed
        }

        // Overlapping prefixes on two sites are legal and expected: home routers ship the same
        // default range. Matching one then no longer says which site a device is on.
        for (j in 1 until sites.size) {
            for (i in 0 until j) {
                val pair = parsedBySite[i].flatMap { a -> parsedBySite[j].map { b -> a to b } }
                    .firstOrNull { (a, b) -> a.second.overlaps(b.second) } ?: continue
                val (a, b) = pair
                val first = textOf((sites[i] as? SetupJson.Object)?.get("id"))
                val second = textOf((sites[j] as? SetupJson.Object)?.get("id"))
                val what = if (a.first == b.first) {
                    "both use the prefix ${a.first}"
                } else {
                    "have the overlapping prefixes ${a.first} and ${b.first}"
                }
                add(
                    "shared-prefix",
                    "sites[$j].lanPrefixes",
                    "The sites $first and $second $what, so matching it does not say which one a device is at; choose the " +
                        "site on this device to wake from there.",
                )
            }
        }

        val siteById = linkedMapOf<String, SetupJson.Object>()
        for (value in sites) {
            val site = value as? SetupJson.Object ?: continue
            val id = textOf(site["id"]) ?: continue
            siteById.putIfAbsent(id, site)
        }

        machines.forEachIndexed { index, value ->
            val machine = value as? SetupJson.Object ?: return@forEachIndexed
            val wake = machine["wake"] as? SetupJson.Object ?: return@forEachIndexed
            val path = "machines[$index].wake"
            val site = textOf(machine["site"])?.let { siteById[it] }
            val lanPrefix = if (wake.has("lanPrefix")) checkPrefix(textOf(wake["lanPrefix"]), "$path.lanPrefix") else null
            checkBroadcasts(wake["broadcast"], "$path.broadcast")

            // A machine that can be woken directly: it has a hardware address and a place, a site in
            // sites[] or, only when it names none, a valid lanPrefix.
            val placed = site != null || lanPrefix != null
            if (wake["mac"] !is SetupJson.Text || !placed) return@forEachIndexed
            if (validBroadcasts(wake["broadcast"]).isEmpty() && (site == null || validBroadcasts(site["broadcast"]).isEmpty())) {
                val name = textOf(machine["name"])?.takeIf { it.isNotBlank() } ?: textOf(machine["id"]).orEmpty()
                add(
                    "wake-broadcast-defaulted",
                    path,
                    "$name can be woken directly and no broadcast address is given for it, so the packet goes to the " +
                        "broadcast address of this device's own network, or to 255.255.255.255.",
                )
            }
        }
        return warnings
    }

    /**
     * What a client that predates section 10.1 does with a dotted prefix written without its
     * trailing dot: it matches the prefix as text, so `192.168.1` also takes 192.168.10.x.
     */
    private fun withoutDot(text: String): String {
        val last = text.substringAfterLast('.').toIntOrNull() ?: 0
        val start = "\"$text\" has no trailing dot, and older clients match it as text"
        return if (last in 1..25) {
            "$start, so they also take ${text}0.x for $text.x."
        } else {
            "$start. Write \"$text.\" so every client reads it alike."
        }
    }
}
