package com.x1f4r.legioncontrol.data

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject

/**
 * Taking two versions of a setup apart into pieces somebody can choose between.
 *
 * The diff is per machine, per address, per system, per wake block and per site rather than per
 * line. That granularity is the point: a JSON diff would show
 * a person forty changed lines when what actually happened is "they renamed the tower and I added an
 * address to the Pi", and those two edits do not conflict at all.
 *
 * Everything here works on the raw tree, so a key this build has never heard of travels with the
 * entry it belongs to instead of being dropped.
 */
object SetupMerge {

    /** One thing that can be chosen independently of the others. */
    data class Entry(
        val path: String,
        /** What to call it on screen. */
        val label: String,
        val value: JsonElement?,
    )

    /** How one entry differs between two documents. */
    data class Difference(
        val path: String,
        val label: String,
        val mine: JsonElement?,
        val theirs: JsonElement?,
        val base: JsonElement?,
        /**
         * Whether there is a common ancestor to compare against at all.
         *
         * Kept apart from `base != null`, because those are two different facts: a base document
         * that simply does not contain this entry is exactly the case of an entry one side added,
         * and treating it as "no base" would turn every addition into a question.
         */
        val hasBase: Boolean,
    ) {
        val onlyIChanged: Boolean get() = hasBase && theirs == base && mine != base
        val onlyTheyChanged: Boolean get() = hasBase && mine == base && theirs != base

        /** Both sides moved it, so nobody but a person can say which is right. */
        val bothChanged: Boolean get() = !onlyIChanged && !onlyTheyChanged

        val summary: String
            get() = when {
                mine == null -> "only on the other side"
                theirs == null -> "only here"
                onlyIChanged -> "changed here"
                onlyTheyChanged -> "changed on the other side"
                else -> "changed on both sides"
            }
    }

    /** Which side to take for one entry. */
    enum class Choice { MINE, THEIRS }

    /**
     * The pieces of one document, keyed by a stable path.
     *
     * `machines[x].info` is the machine's own scalar fields with its three lists taken out, so
     * renaming a machine and adding an address to it are two separate decisions.
     */
    fun entriesOf(root: JsonObject): Map<String, Entry> = buildMap {
        (root["appUpdates"] as? JsonObject)?.let {
            put("appUpdates", Entry("appUpdates", "App updates", it))
        }
        for (site in root.objects("sites")) {
            val id = site.text("id") ?: continue
            val name = site.text("name") ?: id
            put("sites[$id]", Entry("sites[$id]", "Site $name", site))
        }
        for (machine in root.objects("machines")) {
            val id = machine.text("id") ?: continue
            val name = machine.text("name") ?: id
            val info = JsonObject(machine - "endpoints" - "systems" - "wake")
            put("machines[$id].info", Entry("machines[$id].info", "$name", info))
            (machine["wake"] as? JsonObject)?.let {
                put("machines[$id].wake", Entry("machines[$id].wake", "$name, waking", it))
            }
            for (endpoint in machine.objects("endpoints")) {
                val eid = endpoint.text("id") ?: continue
                val label = endpoint.text("label") ?: eid
                put(
                    "machines[$id].endpoints[$eid]",
                    Entry("machines[$id].endpoints[$eid]", "$name, address $label", endpoint),
                )
            }
            for (system in machine.objects("systems")) {
                val sid = system.text("id") ?: continue
                val label = system.text("name") ?: sid
                put(
                    "machines[$id].systems[$sid]",
                    Entry("machines[$id].systems[$sid]", "$name, system $label", system),
                )
            }
        }
    }

    /**
     * Every entry the two sides disagree about.
     *
     * [base] is the newest common ancestor when one is available, which is what turns "both
     * documents differ" into "they changed this and I changed that". Without it every difference is
     * a two-way choice, which is worse but still correct.
     */
    fun differences(mine: JsonObject, theirs: JsonObject, base: JsonObject?): List<Difference> {
        val a = entriesOf(mine)
        val b = entriesOf(theirs)
        val c = base?.let { entriesOf(it) }
        return (a.keys + b.keys).sorted().mapNotNull { path ->
            val left = a[path]?.value
            val right = b[path]?.value
            if (left == right) return@mapNotNull null
            Difference(
                path = path,
                label = a[path]?.label ?: b[path]?.label ?: path,
                mine = left,
                theirs = right,
                // Absent from the base is a real answer: it means whoever has it now added it.
                base = c?.get(path)?.value,
                hasBase = c != null,
            )
        }
    }

    /**
     * The default answer for each difference.
     *
     * An entry only one side touched takes that side, which is the whole reason a base is worth
     * having: two people editing different machines merge with nothing to decide. An entry both
     * sides touched defaults to theirs and is listed for a choice.
     */
    fun defaults(differences: List<Difference>): Map<String, Choice> =
        differences.associate { difference ->
            difference.path to when {
                difference.onlyIChanged -> Choice.MINE
                else -> Choice.THEIRS
            }
        }

    /** Whether a difference needs somebody to look at it, or merges on its own. */
    fun needsDecision(differences: List<Difference>): List<Difference> =
        differences.filter { it.bothChanged }

    /**
     * One document out of two, plus a decision per differing entry.
     *
     * Built by starting from [mine] and applying only the entries chosen from [theirs], so every key
     * this build does not understand survives on whichever side it came from.
     */
    fun merge(
        mine: JsonObject,
        theirs: JsonObject,
        choices: Map<String, Choice>,
    ): JsonObject {
        val theirEntries = entriesOf(theirs)
        val myEntries = entriesOf(mine)
        var result = mine

        // Top-level pieces first.
        choices.forEach { (path, choice) ->
            if (path == "appUpdates") {
                val value = if (choice == Choice.THEIRS) theirEntries[path]?.value else myEntries[path]?.value
                result = if (value == null) JsonObject(result - "appUpdates") else JsonObject(result + ("appUpdates" to value))
            }
        }

        result = mergeList(
            root = result,
            key = "sites",
            mine = mine,
            theirs = theirs,
            choices = choices,
            pathOf = { id -> "sites[$id]" },
        )

        result = mergeMachines(result, mine, theirs, choices)
        return result
    }

    private fun mergeList(
        root: JsonObject,
        key: String,
        mine: JsonObject,
        theirs: JsonObject,
        choices: Map<String, Choice>,
        pathOf: (String) -> String,
    ): JsonObject {
        val myById = mine.objects(key).mapNotNull { obj -> obj.text("id")?.let { it to obj } }.toMap()
        val theirById = theirs.objects(key).mapNotNull { obj -> obj.text("id")?.let { it to obj } }.toMap()
        val order = (mine.objects(key).mapNotNull { it.text("id") } +
            theirs.objects(key).mapNotNull { it.text("id") }).distinct()

        val merged = order.mapNotNull { id ->
            when (choices[pathOf(id)]) {
                Choice.THEIRS -> theirById[id]
                Choice.MINE -> myById[id]
                null -> myById[id] ?: theirById[id]
            }
        }
        if (merged.isEmpty() && root[key] == null) return root
        return JsonObject(root + (key to JsonArray(merged)))
    }

    private fun mergeMachines(
        root: JsonObject,
        mine: JsonObject,
        theirs: JsonObject,
        choices: Map<String, Choice>,
    ): JsonObject {
        val myById = mine.objects("machines").mapNotNull { obj -> obj.text("id")?.let { it to obj } }.toMap()
        val theirById = theirs.objects("machines").mapNotNull { obj -> obj.text("id")?.let { it to obj } }.toMap()
        val order = (mine.objects("machines").mapNotNull { it.text("id") } +
            theirs.objects("machines").mapNotNull { it.text("id") }).distinct()

        val merged = order.mapNotNull { id ->
            val a = myById[id]
            val b = theirById[id]

            // The machine's own fields decide whether the machine exists at all: a machine only one
            // side has is an added or a removed machine, and the choice for its info entry is the
            // choice about the machine.
            val info = when (choices["machines[$id].info"]) {
                Choice.THEIRS -> b?.let { JsonObject(it - "endpoints" - "systems" - "wake") }
                Choice.MINE -> a?.let { JsonObject(it - "endpoints" - "systems" - "wake") }
                null -> (a ?: b)?.let { JsonObject(it - "endpoints" - "systems" - "wake") }
            } ?: return@mapNotNull null

            val wake = when (choices["machines[$id].wake"]) {
                Choice.THEIRS -> b?.get("wake")
                Choice.MINE -> a?.get("wake")
                null -> a?.get("wake") ?: b?.get("wake")
            }

            val endpoints = mergeChildren(a, b, "endpoints", choices) { eid -> "machines[$id].endpoints[$eid]" }
            val systems = mergeChildren(a, b, "systems", choices) { sid -> "machines[$id].systems[$sid]" }

            buildJsonObject {
                info.forEach { (key, value) -> put(key, value) }
                put("endpoints", JsonArray(endpoints))
                put("systems", JsonArray(systems))
                if (wake != null) put("wake", wake)
            }
        }
        return JsonObject(root + ("machines" to JsonArray(merged)))
    }

    private fun mergeChildren(
        mine: JsonObject?,
        theirs: JsonObject?,
        key: String,
        choices: Map<String, Choice>,
        pathOf: (String) -> String,
    ): List<JsonObject> {
        val myById = mine?.objects(key).orEmpty().mapNotNull { obj -> obj.text("id")?.let { it to obj } }.toMap()
        val theirById = theirs?.objects(key).orEmpty().mapNotNull { obj -> obj.text("id")?.let { it to obj } }.toMap()
        val order = (mine?.objects(key).orEmpty().mapNotNull { it.text("id") } +
            theirs?.objects(key).orEmpty().mapNotNull { it.text("id") }).distinct()
        return order.mapNotNull { id ->
            when (choices[pathOf(id)]) {
                Choice.THEIRS -> theirById[id]
                Choice.MINE -> myById[id]
                null -> myById[id] ?: theirById[id]
            }
        }
    }

    /**
     * The newest hash both sides descend from, or null.
     *
     * Newest first in my own ancestry, so the first of mine that they also know is the closest
     * common point and therefore the most useful base.
     */
    fun commonBase(myLineage: List<String>, theirLineage: List<String>): String? {
        val theirs = theirLineage.toSet()
        return myLineage.firstOrNull { it in theirs }
    }
}

private fun JsonObject.objects(key: String): List<JsonObject> =
    (this[key] as? JsonArray).orEmpty().filterIsInstance<JsonObject>()

/** How many addresses a document names, for the change preview. */
internal fun JsonObject.endpointCount(): Int =
    objects("machines").sumOf { it.objects("endpoints").size }

/** The machines a document names, by id. */
internal fun JsonObject.machineIds(): List<String> = objects("machines").mapNotNull { it.text("id") }

/** The name a document gives a machine, for a change preview that reads like prose. */
internal fun JsonObject.machineLabel(id: String): String =
    objects("machines").firstOrNull { it.text("id") == id }?.text("name") ?: id

/** Whether the value under a machine id differs between two documents. */
internal fun sameMachine(a: JsonObject, b: JsonObject, id: String): Boolean {
    val left = a.objects("machines").firstOrNull { it.text("id") == id }
    val right = b.objects("machines").firstOrNull { it.text("id") == id }
    return left == right
}

/** A primitive as text, for building small preview lines. */
internal fun JsonElement?.asText(): String? =
    (this as? JsonPrimitive)?.content?.takeIf { it.isNotBlank() }
