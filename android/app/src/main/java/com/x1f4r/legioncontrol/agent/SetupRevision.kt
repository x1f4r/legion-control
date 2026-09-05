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
 * What to do about the copy a machine is holding.
 *
 * Only two of these happen on their own, and both of them are along strict descent: push when the
 * machine's copy is an ancestor of mine, fetch when mine is an ancestor of the machine's. Descent is
 * acyclic, so two devices can never take turns overwriting each other. Everything else stops here
 * and waits for a person.
 */
sealed interface SetupDecision {
    /** The machine holds exactly what this device has. */
    data object InSync : SetupDecision

    /** The machine holds nothing, or something this device's copy descends from. Push, no prompt. */
    data class Push(val reason: String) : SetupDecision

    /** This device's copy is an ancestor of the machine's. Fetch and apply, no prompt. */
    data class Adopt(val reason: String) : SetupDecision

    /**
     * The hashes differ and descent cannot be decided from the status reply alone.
     *
     * One `config meta` round trip settles it, and it is deliberately not done on every poll: the
     * status carries only the hash, and asking for the ancestry is a second command.
     */
    data object AskForMeta : SetupDecision

    /** A different setup entirely. Always a question, never a merge. */
    data class DifferentSetup(val reason: String) : SetupDecision

    /** The same setup, and neither side descends from the other. A person merges it. */
    data class Diverged(val reason: String) : SetupDecision

    /** The machine cannot carry a setup at all. Excluded from reconciliation, never a conflict. */
    data class CannotCarry(val reason: String) : SetupDecision
}

/**
 * What a status reply alone can settle.
 *
 * [reportedHash] is `status.controller.hash`. Everything here is decidable without a second round
 * trip, and anything that is not returns [SetupDecision.AskForMeta].
 */
fun decideFromStatus(
    reportedHash: String?,
    applied: SetupProvenance?,
    speaksV3: Boolean,
): SetupDecision {
    if (!speaksV3) {
        return SetupDecision.CannotCarry(
            "This machine's control agent is older than contract 3, so it cannot carry the setup.",
        )
    }
    if (applied == null) return SetupDecision.AskForMeta
    val mine = applied.hash ?: return SetupDecision.AskForMeta

    if (reportedHash.isNullOrBlank()) {
        return SetupDecision.Push("It is not carrying a setup yet.")
    }
    if (reportedHash == mine) return SetupDecision.InSync
    if (reportedHash in applied.lineage) {
        return SetupDecision.Push("It is carrying a copy this one was made from.")
    }
    return SetupDecision.AskForMeta
}

/**
 * What the machine's own ancestry settles, once it has been asked for.
 *
 * [held] is what `config meta` reported. The order of the three tests is the whole rule: a different
 * setup id is never a merge, my hash being in their ancestry means I am behind, and anything left is
 * genuine divergence.
 */
fun decideFromMeta(held: SetupProvenance, applied: SetupProvenance?): SetupDecision {
    if (applied == null) {
        return SetupDecision.Adopt("This is the first setup this device has been given.")
    }
    val mine = applied.hash
    val theirs = held.hash

    if (!theirs.isNullOrBlank() && theirs == mine) return SetupDecision.InSync

    val myId = applied.authority?.takeIf { it.isNotBlank() }
    val theirId = held.authority?.takeIf { it.isNotBlank() }

    // A machine holding a copy with no identity at all is a legacy push, and the first identified
    // document is accepted by the agent. Pushing is right and it needs no question.
    if (theirId == null) {
        return SetupDecision.Push("It is carrying a copy that does not say which setup it belongs to.")
    }
    if (myId != null && myId != theirId) {
        return SetupDecision.DifferentSetup(
            "This device has the setup \"$myId\" and that machine is carrying \"$theirId\". " +
                "Two setups cannot be merged; one of them has to replace the other.",
        )
    }

    if (!theirs.isNullOrBlank() && theirs in applied.lineage) {
        return SetupDecision.Push("It is carrying a copy this one was made from.")
    }
    if (!mine.isNullOrBlank() && mine in held.lineage) {
        return SetupDecision.Adopt(
            "That machine has a newer copy of the same setup, made from the one this device has.",
        )
    }

    return SetupDecision.Diverged(
        "This device has revision ${applied.revision ?: 0} and that machine has revision " +
            "${held.revision ?: 0} of the same setup, and neither was made from the other. " +
            "Somebody edited both.",
    )
}

/**
 * Whether a machine that answered with [reportedHash] is worth a `config meta` round trip.
 *
 * [asked] is the set of hashes already asked about and answered, and it is what keeps a machine that
 * is genuinely diverged from being interrogated every fifteen seconds: it is asked once per distinct
 * copy, and asking again about the same bytes gets the same answer.
 */
fun shouldAskForMeta(reportedHash: String?, applied: String?, asked: Set<String>): Boolean =
    !reportedHash.isNullOrBlank() && reportedHash != applied && reportedHash !in asked

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
