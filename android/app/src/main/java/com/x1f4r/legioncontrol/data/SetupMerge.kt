package com.x1f4r.legioncontrol.data

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive

/**
 * Taking two versions of a setup apart into pieces somebody can choose between, and putting one
 * document back together from the choices (contract section 9.8).
 *
 * The diff is per machine, per address, per system, per wake block and per site rather than per
 * line. That granularity is the point: a JSON diff would show a person forty changed lines when what
 * actually happened is "they renamed the tower and I added an address to the Pi", and those two
 * edits do not conflict at all.
 *
 * This used to lose things. A deletion beat an edit without a question, an endpoint without an id
 * vanished, an unknown key at the top of the document was kept from this side only, and every merged
 * list was rebuilt in a new order (C6). Now a deletion against an edit is a question like any other
 * change both sides made, the id-less endpoints are one entry, every unknown key is an entry of its
 * own or travels with the part that holds it, and the result starts from this device's document and
 * changes only what was taken from the other side, in place, with every number spelled as it was.
 *
 * A port of `entries`, `differences`, `merge`, `commonBase` and `mergedLineage` in
 * `contract/tools/setup-sync.mjs`, held to `contract/vectors/setup-merge.json`.
 */
object SetupMerge {

    /** The longest lineage a document carries (section 9.3). */
    private const val LINEAGE_CAP = MAX_LINEAGE

    /** Which side changed an entry the two disagree about. */
    enum class Kind(val wire: String) {
        /** There is a base and theirs still has it as the base had it: take mine. */
        MINE("mine"),

        /** There is a base and mine still has it as the base had it: take theirs. */
        THEIRS("theirs"),

        /** Both changed it since the base, a deletion against an edit included. */
        BOTH("both"),

        /** There is no base, so nothing says who changed what. */
        NO_BASE("noBase"),
    }

    /** Which side to take for one entry. */
    enum class Choice { MINE, THEIRS }

    /** One entry the two sides disagree about, with both values, absent being null. */
    data class Difference(
        /** The entry's key, `machines[tower].endpoints[lan]`, which is also what a choice names. */
        val entry: String,
        val kind: Kind,
        val mine: SetupJson?,
        val theirs: SetupJson?,
        /** What to call it on screen. */
        val label: String,
    ) {
        /** Answered without a question: only one side touched it. */
        val settles: Boolean get() = kind == Kind.MINE || kind == Kind.THEIRS

        /** The side taken when nobody chooses: `both` and `noBase` are preselected theirs and shown. */
        val defaultChoice: Choice get() = if (kind == Kind.MINE) Choice.MINE else Choice.THEIRS

        val summary: String
            get() = when {
                mine == null && kind == Kind.BOTH -> "removed here and changed on the other side"
                theirs == null && kind == Kind.BOTH -> "changed here and removed on the other side"
                mine == null -> "only on the other side"
                theirs == null -> "only here"
                kind == Kind.MINE -> "changed here"
                kind == Kind.THEIRS -> "changed on the other side"
                kind == Kind.BOTH -> "changed on both sides"
                else -> "different on the two sides"
            }
    }

    /** Who writes the merged revision: their `source`, `device` and the time, as `updatedAt`. */
    data class Writer(val source: String, val device: String, val now: String)

    /**
     * What a merge made: every difference, and the merged text, or the first rule of section 9.7 the
     * result breaks, in which case nothing is to be written.
     */
    data class Outcome(val differences: List<Difference>, val resultText: String?, val error: String?)

    /**
     * A side that cannot be merged: theirs or the base fails peer intake, or mine has an error that
     * leaves its entries undefined. Take theirs still works; Merge and Keep mine wait until it is
     * fixed.
     */
    class Unmergeable(message: String) : IllegalArgumentException(message)

    /**
     * The section 9.7 errors mine may carry into a merge. Each is about how parts that are well
     * formed on their own refer to each other, a site or a helper that names nothing, a helper that
     * is its own, a loop, an alias that disagrees, and the result is checked like any other, so
     * taking theirs' side of the part at fault repairs it. Every other error leaves no single answer
     * to what the document's entries are.
     */
    val MERGEABLE_ERRORS: Set<String> = setOf("unknown-site", "helper-self", "helper-unknown", "helper-alias", "helper-cycle")

    /** The controller keys that are identity, which a merge writes itself and never offers as entries. */
    private val IDENTITY_KEYS = setOf("id", "revision", "updatedAt", "source", "device", "lineage")

    /** The top-level keys whose contents are entries of their own. */
    private val STRUCTURE_KEYS = setOf("controller", "machines", "sites")

    /** The members of a machine that are entries of their own when both sides have the machine. */
    private val MACHINE_PARTS = setOf("endpoints", "systems", "wake")

    /** How the keys of the other entries begin, and the quote that marks a top-level key written in quotes. */
    private val QUOTED_BEGINNINGS = listOf("\"", "controller.", "sites[", "machines[")

    /**
     * The entry key of a top-level member: its name, unless it begins the way another entry's key
     * does. Then it is written inside double quotes with nothing escaped, and so is a name that
     * already begins with a quote, which keeps the quoting reversible. Without it, a peer document
     * with a member called `sites[home]` beside a site `home` would give one key to two entries.
     */
    fun topLevelKey(name: String): String =
        if (QUOTED_BEGINNINGS.any { name.startsWith(it) }) "\"$name\"" else name

    /**
     * Whether mine can take part in Merge or Keep mine at all: null when it can, or why not. A text
     * that only breaks the rules [MERGEABLE_ERRORS] names still merges.
     */
    fun mineBlocks(mineText: String): String? = try {
        intake(mineText, Side.MINE)
        null
    } catch (refused: Unmergeable) {
        refused.message
    }

    /** Every difference between mine and theirs, in code-point order of the key. */
    fun differences(mineText: String, theirsText: String, baseText: String?): List<Difference> {
        val mine = intake(mineText, Side.MINE)
        val theirs = intake(theirsText, Side.THEIRS)
        val base = baseText?.let { intake(it, Side.BASE) }
        return differing(mine, theirs, base).map { it.difference }
    }

    /**
     * What an edit in progress changed, entry by entry, as differences of [after] (mine) from
     * [before] (theirs) with no base. Neither is held to section 9.7, because an edit is checked
     * when it is applied; a document whose entries are not well defined yet lists nothing.
     */
    fun changesBetween(before: JsonObject, after: JsonObject): List<Difference> = try {
        differing(SetupJson.of(after) as SetupJson.Object, SetupJson.of(before) as SetupJson.Object, null).map { it.difference }
    } catch (_: IllegalStateException) {
        emptyList()
    }

    /**
     * Merges two documents.
     *
     * [choices] maps an entry key to a side. An entry it does not name takes the side its kind says,
     * so `both` and `noBase` are theirs; Keep mine is every entry answered [Choice.MINE]. The result
     * starts from mine: every entry taken from theirs is removed where theirs lacks it and replaced in
     * place where both have it, then theirs' arrays are walked to insert what theirs adds. The
     * identity is mine's id, one more than the higher revision, the merged lineage, and the writer's
     * `updatedAt`, `source` and `device`. The result is held to section 9.7 under peer intake, and one
     * it refuses is not written: [Outcome.error] is the first of its error codes in code-point order.
     *
     * Throws [Unmergeable] when a side cannot be merged at all.
     */
    fun merge(
        baseText: String?,
        mineText: String,
        theirsText: String,
        mineHash: String,
        theirsHash: String,
        writer: Writer,
        choices: Map<String, Choice> = emptyMap(),
    ): Outcome {
        val mine = intake(mineText, Side.MINE)
        val theirs = intake(theirsText, Side.THEIRS)
        val base = baseText?.let { intake(it, Side.BASE) }
        if ((mine.obj("controller")?.get("id")) !is SetupJson.Text) {
            throw Unmergeable("This device's setup has no setup id yet; it gets one before it takes part in a merge.")
        }

        val found = differing(mine, theirs, base)
        val taken = found
            .filter { (choices[it.difference.entry] ?: it.difference.defaultChoice) == Choice.THEIRS }
            .map { it.difference.entry }
            .toSet()

        val result = mine.copy() as SetupJson.Object
        for (difference in found) if (difference.difference.entry in taken) takeTheirs(result, difference)

        walkTheirs(result, "sites", theirs["sites"], { id -> "sites[$id]" }, taken)
        walkTheirs(result, "machines", theirs["machines"], { id -> "machines[$id]" }, taken)
        val paired = pairedMachines(mine, theirs)
        for (machine in objectsOf(theirs["machines"])) {
            val id = idOf(machine) ?: continue
            if (id !in paired) continue
            val at = "machines[$id]"
            val holder = machineIn(result, id) ?: continue
            walkTheirs(holder, "systems", machine["systems"], { system -> "$at.systems[$system]" }, taken)
            walkTheirs(holder, "endpoints", machine["endpoints"], { endpoint -> "$at.endpoints[${endpoint.orEmpty()}]" }, taken)
        }

        // The identity. Mine's id stays where it is; the rest is written in place, or appended in
        // this order when mine's block lacks it.
        val block = result.obj("controller")!!
        val higher = maxOf(revisionOf(mine), revisionOf(theirs))
        block["revision"] = SetupJson.Number((higher + java.math.BigInteger.ONE).toString())
        block["updatedAt"] = SetupJson.Text(writer.now)
        block["source"] = SetupJson.Text(writer.source)
        block["device"] = SetupJson.Text(writer.device)
        block["lineage"] = SetupJson.Array(
            mergedLineage(mineHash, theirsHash, lineageOf(mine), lineageOf(theirs)).mapTo(mutableListOf()) { SetupJson.Text(it) },
        )

        val text = result.print()
        val verdict = DocumentAcceptance.check(text, Intake.PEER)
        val error = verdict.errors.map { it.code }.distinct().sortedWith(::compareCodePoints).firstOrNull()
        return Outcome(found.map { it.difference }, if (verdict.valid) text else null, error)
    }

    /**
     * The base of a merge: the first hash of mine's lineage, newest first, that is also in theirs'
     * lineage, whose kept bytes still hash to it and which peer intake accepts; null when there is
     * none. A kept revision this build refuses, kept by an older one, is skipped like one that is
     * not kept (section 9.8). The heads themselves are never a base: a merge whose base was one side
     * would only ever take the other side.
     */
    fun commonBase(
        mineHash: String?,
        theirsHash: String?,
        mineLineage: List<String>,
        theirsLineage: List<String>,
        kept: (String) -> String?,
    ): String? {
        for (hash in mineLineage) {
            if (hash == mineHash || hash == theirsHash || hash !in theirsLineage) continue
            val text = kept(hash) ?: continue
            if (CanonicalSetup.hashOf(text) == hash && DocumentAcceptance.check(text, Intake.PEER).errors.isEmpty()) return hash
        }
        return null
    }

    /**
     * The lineage of a merge result: mine's hash, theirs' hash, then the two lineages interleaved,
     * mine first, each hash once, cut at 32. Both heads are in it, so both branches fast-forward onto
     * the result.
     */
    fun mergedLineage(mineHash: String?, theirsHash: String?, mineLineage: List<String>, theirsLineage: List<String>): List<String> {
        val lineage = mutableListOf<String>()
        fun add(hash: String?) {
            if (hash != null && hash !in lineage) lineage += hash
        }
        add(mineHash)
        add(theirsHash)
        for (index in 0 until maxOf(mineLineage.size, theirsLineage.size)) {
            if (index < mineLineage.size) add(mineLineage[index])
            if (index < theirsLineage.size) add(theirsLineage[index])
        }
        return lineage.take(LINEAGE_CAP)
    }

    /**
     * Strings in code-point order. Kotlin's own string order compares UTF-16 units, which puts
     * U+FFFD after U+1F600; differences are listed in the same order on every platform.
     */
    fun compareCodePoints(a: String, b: String): Int {
        var i = 0
        var j = 0
        while (i < a.length && j < b.length) {
            val x = a.codePointAt(i)
            val y = b.codePointAt(j)
            if (x != y) return x.compareTo(y)
            i += Character.charCount(x)
            j += Character.charCount(y)
        }
        return a.codePointCount(i, a.length) - b.codePointCount(j, b.length)
    }

    // MARK: - Sides

    private enum class Side { MINE, THEIRS, BASE }

    /**
     * One side, parsed. Theirs and the base are held to peer intake, as everything read from a
     * machine or kept is. Mine is the applied document, held to applied intake, and may only break
     * the rules [MERGEABLE_ERRORS] names.
     */
    private fun intake(text: String, side: Side): SetupJson.Object {
        val verdict = DocumentAcceptance.check(text, if (side == Side.MINE) Intake.APPLIED else Intake.PEER)
        val blocking = verdict.errors.filter { side != Side.MINE || it.code !in MERGEABLE_ERRORS }
        if (blocking.isNotEmpty() || verdict.document == null) {
            val first = blocking.firstOrNull()?.message ?: "it is not a setup"
            throw Unmergeable(
                when (side) {
                    Side.MINE -> "This device's setup has an error that has to be fixed in the editor before it can be merged: $first"
                    Side.THEIRS -> "The other copy breaks a rule every client holds a setup to, so it cannot be merged: $first"
                    Side.BASE -> "The revision both were made from breaks a rule every client holds a setup to: $first"
                },
            )
        }
        return SetupJsonParser.parse(CanonicalSetup.canonicalText(text)) as SetupJson.Object
    }

    // MARK: - Entries

    /** Where an entry lives in a document, so it can be taken from theirs. */
    private sealed interface Place {
        data class Top(val name: String) : Place
        data class Controller(val name: String) : Place
        data class Site(val id: String) : Place
        data class Machine(val id: String) : Place
        data class Fields(val machine: String) : Place
        data class Wake(val machine: String) : Place
        data class System(val machine: String, val id: String) : Place
        data class Endpoint(val machine: String, val id: String) : Place
        data class Idless(val machine: String) : Place
    }

    private class Entry(val place: Place, val value: SetupJson, val label: String)

    private class Found(val difference: Difference, val place: Place)

    /**
     * One document's entries by key. Keys are unique in every document a merge takes: section 9.7
     * refuses two machines, two sites, or two systems or endpoints of one machine with one id, and
     * [topLevelKey] keeps unknown names apart from the rest. So a second entry under one key is a
     * bug, and it throws rather than merge one of the two.
     */
    private fun entryMap(document: SetupJson.Object, paired: Set<String>): Map<String, Entry> {
        val map = linkedMapOf<String, Entry>()
        fun add(key: String, entry: Entry) {
            check(key !in map) { "two entries of one document are both called $key" }
            map[key] = entry
        }
        for ((name, value) in document.members) {
            if (name !in STRUCTURE_KEYS) add(topLevelKey(name), Entry(Place.Top(name), value, labelForTop(name)))
        }
        document.obj("controller")?.let { controller ->
            for ((name, value) in controller.members) {
                if (name !in IDENTITY_KEYS) add("controller.$name", Entry(Place.Controller(name), value, "The setup's $name"))
            }
        }
        for (site in objectsOf(document["sites"])) {
            val id = idOf(site) ?: continue
            add("sites[$id]", Entry(Place.Site(id), site, "Site ${nameOf(site) ?: id}"))
        }
        for (machine in objectsOf(document["machines"])) {
            val id = idOf(machine) ?: continue
            val name = nameOf(machine) ?: id
            val at = "machines[$id]"
            if (id !in paired) {
                add(at, Entry(Place.Machine(id), machine, name))
                continue
            }
            val fields = SetupJson.Object(machine.members.filterTo(mutableListOf()) { it.first !in MACHINE_PARTS })
            add("$at.fields", Entry(Place.Fields(id), fields, name))
            machine["wake"]?.let { add("$at.wake", Entry(Place.Wake(id), it, "$name, waking")) }
            for (system in objectsOf(machine["systems"])) {
                val systemId = idOf(system) ?: continue
                add("$at.systems[$systemId]", Entry(Place.System(id, systemId), system, "$name, system ${nameOf(system) ?: systemId}"))
            }
            // Endpoints without an id cannot be told apart, so together, in order, they are one
            // entry. None at all is absent.
            val idless = mutableListOf<SetupJson>()
            for (endpoint in objectsOf(machine["endpoints"])) {
                val endpointId = idOf(endpoint)
                if (endpointId == null) {
                    idless += endpoint
                } else {
                    val label = (endpoint["label"] as? SetupJson.Text)?.value ?: endpointId
                    add("$at.endpoints[$endpointId]", Entry(Place.Endpoint(id, endpointId), endpoint, "$name, address $label"))
                }
            }
            if (idless.isNotEmpty()) {
                add("$at.endpoints[]", Entry(Place.Idless(id), SetupJson.Array(idless), "$name, addresses without an id"))
            }
        }
        return map
    }

    private fun machineIds(document: SetupJson.Object): Set<String> = objectsOf(document["machines"]).mapNotNull(::idOf).toSet()

    /** The ids of the machines both documents have, which merge part by part. */
    private fun pairedMachines(mine: SetupJson.Object, theirs: SetupJson.Object): Set<String> =
        machineIds(mine) intersect machineIds(theirs)

    /**
     * Every entry where mine and theirs differ, in code-point order of its key, with its kind: with a
     * base, the side that left the entry as the base had it did not change it, so the other side's
     * value is taken; when both changed it, a deletion against an edit included, it is `both`.
     */
    private fun differing(mine: SetupJson.Object, theirs: SetupJson.Object, base: SetupJson.Object?): List<Found> {
        val paired = pairedMachines(mine, theirs)
        val ofMine = entryMap(mine, paired)
        val ofTheirs = entryMap(theirs, paired)
        val ofBase = base?.let { entryMap(it, paired) }.orEmpty()
        val keys = (ofMine.keys + ofTheirs.keys).distinct().sortedWith(::compareCodePoints)
        return keys.mapNotNull { key ->
            val left = ofMine[key]
            val right = ofTheirs[key]
            if (JsonEquality.equal(left?.value, right?.value)) return@mapNotNull null
            val baseValue = ofBase[key]?.value
            val kind = when {
                base == null -> Kind.NO_BASE
                JsonEquality.equal(right?.value, baseValue) -> Kind.MINE
                JsonEquality.equal(left?.value, baseValue) -> Kind.THEIRS
                else -> Kind.BOTH
            }
            val entry = left ?: right!!
            Found(Difference(key, kind, left?.value, right?.value, entry.label), entry.place)
        }
    }

    // MARK: - Building the result

    /** Take one entry from theirs: remove what theirs lacks, replace in place what both have. Insertions come later. */
    private fun takeTheirs(result: SetupJson.Object, found: Found) {
        val theirs = found.difference.theirs
        fun setOrRemove(holder: SetupJson.Object?, name: String) {
            if (holder == null) return
            if (theirs == null) holder.remove(name) else holder[name] = theirs.copy()
        }
        when (val place = found.place) {
            is Place.Top -> setOrRemove(result, place.name)
            is Place.Controller -> setOrRemove(result.obj("controller"), place.name)
            is Place.Site -> replaceById(result["sites"], place.id, theirs)
            is Place.Machine -> replaceById(result["machines"], place.id, theirs)
            is Place.Fields -> {
                // The machine's own members, other than its parts, become theirs: each where mine has
                // it, gone where theirs lacks it, appended where it is new.
                val machine = machineIn(result, place.machine) ?: return
                val fields = theirs as SetupJson.Object
                val kept = machine.members
                    .filter { (name, _) -> name in MACHINE_PARTS || fields.has(name) }
                    .map { (name, value) -> if (name in MACHINE_PARTS) name to value else name to fields[name]!!.copy() }
                machine.members.clear()
                machine.members += kept
                for ((name, value) in fields.members) if (!machine.has(name)) machine.members += name to value.copy()
            }

            is Place.Wake -> setOrRemove(machineIn(result, place.machine), "wake")
            is Place.System -> replaceById(machineIn(result, place.machine)?.get("systems"), place.id, theirs)
            is Place.Endpoint -> replaceById(machineIn(result, place.machine)?.get("endpoints"), place.id, theirs)
            is Place.Idless -> {
                val endpoints = machineIn(result, place.machine)?.get("endpoints") as? SetupJson.Array
                endpoints?.items?.removeAll { idOf(it) == null }
            }
        }
    }

    /** In an array of objects keyed by id: remove the element (value null) or replace it where it stands. */
    private fun replaceById(array: SetupJson?, id: String, value: SetupJson?) {
        val items = (array as? SetupJson.Array)?.items ?: return
        val index = items.indexOfFirst { idOf(it) == id }
        if (index < 0) return
        if (value == null) items.removeAt(index) else items[index] = value.copy()
    }

    /**
     * Walk one of theirs' arrays in order with a cursor. An element whose key is in the result moves
     * the cursor to it; an element taken from theirs that is not in the result is inserted after the
     * cursor, first when there is none, and becomes the cursor. Anything else is passed over. So an
     * element theirs added lands after its predecessor in theirs, and everything else keeps mine's
     * order.
     *
     * An element without an id has no key of its own. When the id-less entry is taken from theirs,
     * mine's id-less elements are already gone and each of theirs' is inserted; otherwise theirs' n-th
     * id-less element stands for the result's n-th, and only moves the cursor.
     */
    private fun walkTheirs(
        holder: SetupJson.Object,
        name: String,
        theirsArray: SetupJson?,
        entryOf: (String?) -> String,
        taken: Set<String>,
    ) {
        var array = holder[name] as? SetupJson.Array
        var cursor = -1
        var idless = 0
        for (element in objectsOf(theirsArray)) {
            val id = idOf(element)
            val entry = entryOf(id)
            val items = array?.items.orEmpty()
            val index = when {
                id != null -> items.indexOfFirst { idOf(it) == id }
                entry in taken -> -1
                else -> {
                    val positions = items.indices.filter { items[it] is SetupJson.Object && idOf(items[it]) == null }
                    (if (idless < positions.size) positions[idless] else -1).also { idless += 1 }
                }
            }
            if (index >= 0) {
                cursor = index
                continue
            }
            if (entry !in taken) continue
            // An array mine never had is created only for something that goes in it.
            val target = array ?: SetupJson.Array().also {
                array = it
                holder[name] = it
            }
            cursor += 1
            target.items.add(cursor, element.copy())
        }
    }

    // MARK: - Small readers

    private fun SetupJson.Object.obj(name: String): SetupJson.Object? = this[name] as? SetupJson.Object

    private fun objectsOf(node: SetupJson?): List<SetupJson.Object> =
        (node as? SetupJson.Array)?.items?.filterIsInstance<SetupJson.Object>().orEmpty()

    private fun idOf(node: SetupJson?): String? = ((node as? SetupJson.Object)?.get("id") as? SetupJson.Text)?.value

    private fun nameOf(node: SetupJson.Object): String? = (node["name"] as? SetupJson.Text)?.value?.takeIf { it.isNotBlank() }

    private fun machineIn(result: SetupJson.Object, id: String): SetupJson.Object? =
        objectsOf(result["machines"]).firstOrNull { idOf(it) == id }

    private fun revisionOf(document: SetupJson.Object): java.math.BigInteger =
        JsonEquality.integerOf(document.obj("controller")?.get("revision")) ?: java.math.BigInteger.ZERO

    private fun lineageOf(document: SetupJson.Object): List<String> =
        (document.obj("controller")?.get("lineage") as? SetupJson.Array)?.items?.mapNotNull { (it as? SetupJson.Text)?.value }.orEmpty()

    private fun labelForTop(name: String): String = when (name) {
        "appUpdates" -> "App updates"
        else -> "The setup's $name"
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
    return JsonEquality.equal(left?.let(SetupJson::of), right?.let(SetupJson::of))
}

/** A primitive as text, for building small preview lines. */
internal fun JsonElement?.asText(): String? =
    (this as? JsonPrimitive)?.content?.takeIf { it.isNotBlank() }
