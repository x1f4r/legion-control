package com.x1f4r.legioncontrol.agent

/**
 * Who wrote a setup document, which version of it this is, and what it descends from.
 *
 * A hash answers "is this the same bytes". A revision number answers "how many edits have there
 * been". Neither answers the question that matters when several devices can edit: is this copy
 * newer, or is it a different branch of the same history?
 *
 * Revision numbers cannot tell those apart. Two people editing revision 5 while offline both produce
 * a revision 6, and a number comparison would let the second one erase the first without anybody
 * being told. Ancestry can tell, because descent is a fact rather than a count: a copy is taken only
 * when one side is provably an ancestor of the other, and anything else stops at a person.
 */
data class SetupProvenance(
    /** The setup id, shared by every peer. Null on a copy pushed by a client that carried none. */
    val authority: String?,
    /** Monotonic within one setup id. Null on a copy that does not carry one. */
    val revision: Long?,
    /** The canonical hash of this copy. */
    val hash: String?,
    /** Hashes of the ancestors, newest first. Empty on a copy written before ancestry existed. */
    val lineage: List<String> = emptyList(),
    /** When it was written, as the writer saw it. Display only, never compared. */
    val updatedAt: String? = null,
    /** mac | desktop | phone | cli | legacy: which kind of client wrote it. */
    val sourceKind: String? = null,
    /** Free text: which device wrote it. */
    val device: String? = null,
    /** Which machine this copy was read from, so a conflict can name both sides. */
    val readFrom: String? = null,
) {
    /** One line for the screen: which setup, which revision, who wrote it. */
    fun describe(): String {
        val who = authority?.takeIf { it.isNotBlank() } ?: "an unnamed setup"
        val which = revision?.let { "revision $it" } ?: "no revision"
        return "$who, $which"
    }

    /** Who to blame for this revision, in words. */
    fun describeAuthor(): String {
        val name = device?.takeIf { it.isNotBlank() }
        val kind = ControllerSource.describe(sourceKind)
        return listOfNotNull(name, kind?.let { "on $it" }).joinToString(" ").ifBlank { "somebody" }
    }
}

/**
 * What changes if a document is taken, in terms somebody can decide from.
 *
 * A full JSON diff on a phone screen is unreadable. Which machines come and go, and whether the
 * addresses moved, is what actually decides whether a document is the one you meant.
 */
data class SetupChangePreview(
    val added: List<String>,
    val removed: List<String>,
    val changed: List<String>,
    val endpointsBefore: Int,
    val endpointsAfter: Int,
) {
    val isEmpty: Boolean
        get() = added.isEmpty() && removed.isEmpty() && changed.isEmpty() &&
            endpointsBefore == endpointsAfter

    fun sentences(): List<String> = buildList {
        if (added.isNotEmpty()) add("Adds ${added.joinToString(", ")}.")
        if (removed.isNotEmpty()) add("Removes ${removed.joinToString(", ")}.")
        if (changed.isNotEmpty()) add("Changes ${changed.joinToString(", ")}.")
        if (endpointsBefore != endpointsAfter) {
            add("Addresses go from $endpointsBefore to $endpointsAfter.")
        }
        if (isEmpty) add("The same machines and the same addresses; something else changed.")
    }
}
