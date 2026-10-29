package com.x1f4r.legioncontrol.data

import android.content.SharedPreferences
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.intOrNull

/**
 * The launch values this device approved: the `approved-launch` preference beside the setup text
 * (contract section 11).
 *
 * Its own key rather than part of the bindings, and never published or merged: it is this device's
 * decision about what this device runs, and no document anybody else writes can carry it in. Every
 * write that belongs to a document goes into the same `prefs.edit()` as that document, so the app
 * can never be stopped between storing an edit and recording what the person who made it approved,
 * and then show them a review of their own edit.
 *
 * A record that cannot be read approves nothing: everything waits and the problem is shown. It is
 * never seeded again on its own, because that would approve whatever the document holds now. The
 * next write that approves something keeps the old text aside under `approved-launch.corrupt-<unix
 * time>` and starts a new record.
 */
class LaunchApprovals(
    private val prefs: SharedPreferences,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    /** The record as it was last read. */
    data class Record(
        /** What the gate evaluates against. One that approves nothing while [problem] is set. */
        val values: JsonObject,
        /** Why the record could not be read, or null. */
        val problem: String?,
    )

    /** The text the record was last parsed from, so it is parsed again only when that changed. */
    private var parsed: Pair<String?, Record>? = null

    /**
     * The record, read again whenever its text changed since it was last read.
     *
     * Reading the preference costs a map lookup, so it is done before every evaluation: another
     * writer in the same process, such as the debug build's test bridge, is seen at once.
     */
    fun read(): Record {
        val raw = rawText()
        parsed?.takeIf { it.first == raw }?.let { return it.second }
        return parse(raw).also { parsed = raw to it }
    }

    /**
     * The first start of this build: every launch value of the document this device already
     * applies is approved, with `seededFrom` the hash of [text], so an upgrade drives exactly what it
     * drove before and asks nothing. With no document the record says so (`"none"`), and no later
     * adoption can count as a seed. A record that is there, readable or not, is left alone.
     */
    fun seedIfMissing(text: String, document: JsonObject?) {
        if (prefs.contains(KEY)) return
        val seed = if (document == null) {
            LaunchValues.EMPTY_RECORD
        } else {
            LaunchValues.approvalsFrom(document, seededFrom = CanonicalSetup.hashOf(text))
        }
        prefs.edit().putString(KEY, seed.toString()).apply()
    }

    /**
     * Writes into [editor] the record with [changes] approved, and with each value the gate then
     * accepts in [document] by reuse or migration recorded at its place. Returns the record that
     * results, which is the one read when nothing changed.
     *
     * Nothing is written over a record that cannot be read unless something is approved: such a
     * record approves nothing, so nothing in a document was accepted by reuse either.
     */
    fun update(editor: SharedPreferences.Editor, document: JsonObject?, changes: List<LaunchChange>): Record {
        val current = read()
        val unreadable = current.problem != null
        if (unreadable && changes.isEmpty()) return current
        val approved = LaunchValues.recording(if (unreadable) LaunchValues.EMPTY_RECORD else current.values, changes)
        val values = LaunchValues.recording(approved, LaunchValues.accepted(approved, LaunchValues.projection(document)))
        if (!unreadable && values == current.values) return current
        if (unreadable) rawText()?.let { editor.putString("$KEY.corrupt-${clock() / 1000}", it) }
        editor.putString(KEY, values.toString())
        return Record(values, null)
    }

    private fun rawText(): String? = try {
        prefs.getString(KEY, null)
    } catch (_: ClassCastException) {
        // Something that is not text under this key: unreadable, and kept aside as what it was.
        prefs.all[KEY]?.toString()
    }

    private fun parse(raw: String?): Record {
        // Only before the seed, which the store runs before anything reads the record.
        if (raw == null) return Record(LaunchValues.EMPTY_RECORD, null)
        val json = try {
            Json.parseToJsonElement(raw)
        } catch (_: Exception) {
            return unreadable("could not be read")
        }
        if (json !is JsonObject) return unreadable("is not a JSON object")
        val schema = (json["schema"] as? JsonPrimitive)?.takeUnless { it.isString }?.intOrNull
        if (schema != 1) return unreadable("is in a format this build does not know")
        return Record(json, null)
    }

    private fun unreadable(why: String) = Record(
        values = JsonObject(emptyMap()),
        problem = "This device's record of the commands it approved $why, so every command in the " +
            "shared setup waits for approval. Approving one starts a new record and keeps the old one aside.",
    )

    companion object {
        /** The preference, in `legion-config` beside the setup text. */
        const val KEY = "approved-launch"
    }
}
