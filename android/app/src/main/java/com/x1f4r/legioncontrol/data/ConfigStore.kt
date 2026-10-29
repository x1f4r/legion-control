package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
import androidx.core.content.edit
import com.x1f4r.legioncontrol.agent.LaunchGate
import com.x1f4r.legioncontrol.agent.SetupProvenance
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.time.Instant

/** Where a setup was fetched from, so the three fields start where they were left. */
data class SetupSource(
    val host: String,
    val port: Int,
    val user: String,
)

/**
 * Two copies of the same setup that neither descends from the other.
 *
 * Held rather than resolved. This is the state the whole lineage machinery exists to produce
 * instead of a silent overwrite: somebody edited on two devices while one of them was offline, and
 * the only thing that can decide which parts of each to keep is a person.
 *
 * Recorded only from bytes that hash to what the machine reported, and [theirProvenance] is what
 * those bytes say about themselves. [mineProvenance] is the document this screen was built against:
 * Merge, Keep mine and Take theirs each replace it only if it is still the one in force.
 *
 * [machineId] is the machine that served the bytes. A name is only what to call it: names are free
 * text, and two machines may share one.
 */
data class SetupDivergence(
    val machineId: String,
    val machineName: String,
    val reason: String,
    /** Their document, exactly as the machine stores it. */
    val theirText: String,
    val theirProvenance: SetupProvenance,
    val mineProvenance: SetupProvenance?,
    /** The newest ancestor both sides share, when this device kept a copy of it whose bytes still hash to it. */
    val baseHash: String?,
    val at: Long,
)

/**
 * Two different setups entirely, which is a replace-or-adopt question and never a merge. Replace
 * goes to [machineId], the machine that served [theirText], and to no other.
 */
data class SetupIdentityClash(
    val machineId: String,
    val machineName: String,
    val reason: String,
    val theirText: String,
    val theirProvenance: SetupProvenance,
    val mineProvenance: SetupProvenance?,
    val at: Long,
)

/** What became of a document a machine offered, or of a merge. */
sealed interface SetupOutcome {
    data class Applied(val reason: String) : SetupOutcome
    data object Unchanged : SetupOutcome

    /**
     * The document in force is no longer the one the decision was made against, so nothing was
     * written: an edit saved meanwhile stays, and the next status decides from it.
     */
    data object HeadMoved : SetupOutcome
    data class Invalid(val reason: String) : SetupOutcome

    /**
     * This device could not write the document down, so nothing changed. Nothing is wrong with the
     * document: the same decision can be made again later.
     */
    data class NotSaved(val reason: String) : SetupOutcome
}

/**
 * The setup this device holds, and everything about where it came from.
 *
 * Every device is a peer now: this one edits, publishes and adopts exactly like the Mac and the
 * desktop. What makes that safe is not a rule about who may write, it is ancestry. The document
 * carries the hashes of the revisions it was made from, a machine accepts a push only from a copy
 * that descends from what it holds, and this store only ever adopts a copy that its own applied
 * document is an ancestor of. Everything else stops and asks.
 *
 * The text and not the parsed object is what is stored, because the text is what gets hashed and
 * what other devices compare against. A field showing a re-serialised document would also reformat
 * what somebody typed and turn every visit into a diff they did not ask for.
 *
 * Beside the text sits this device's record of the commands it approved (contract section 9.9), in
 * the same preferences and written in the same edit. Adopting a document never approves anything in
 * it: [launch] is what every machine is driven from, and it holds only what this device approved.
 *
 * Every write that replaces the document with something read elsewhere is a compare-and-swap
 * (contract section 9.5): it goes ahead only if the hash in storage, read again under [HEAD_LOCK]
 * at the moment of writing, is still the one the decision was made against. A decision to fetch is
 * made from one status and the bytes arrive a round trip later; without the check, an edit saved in
 * between was replaced by bytes that were chosen against the document it replaced (A15).
 */
class ConfigStore internal constructor(
    private val prefs: SharedPreferences,
    private val revisions: RevisionCache,
) {
    constructor(context: Context, revisions: RevisionCache) : this(
        context.applicationContext.getSharedPreferences("legion-config", Context.MODE_PRIVATE),
        revisions,
    )

    private val _text = MutableStateFlow(prefs.getString(KEY_TEXT, null).orEmpty())

    /** Exactly what is stored, so the field on screen starts as what was last applied. */
    val text: StateFlow<String> = _text.asStateFlow()

    /**
     * The document in force as it was left, under applied intake (contract section 9.7): never
     * refused as long as section 12 accepts its bytes. A document an earlier build stored under rules
     * that were wider in one place and narrower in another stays in force with its errors shown,
     * rather than disappearing at the first start of this one.
     */
    private val loaded: AcceptedConfig? =
        _text.value.takeIf { it.isNotBlank() }?.let { acceptControllerConfig(it, Intake.APPLIED).getOrNull() }

    private val _document = MutableStateFlow(
        loaded?.let { ControllerDocument.parse(_text.value).getOrNull() },
    )

    /** The applied document as a tree, which is what an edit works on. */
    val document: StateFlow<ControllerDocument?> = _document.asStateFlow()

    private val _config = MutableStateFlow(loaded?.config)

    /** The configuration in force. Null means there is none, which is the first-run state. */
    val config: StateFlow<ControllerConfig?> = _config.asStateFlow()

    private val _acceptance = MutableStateFlow(
        loaded?.verdict ?: _text.value.takeIf { it.isNotBlank() }?.let { DocumentAcceptance.check(it, Intake.APPLIED) },
    )

    /**
     * What section 9.7 says of the document in force: its warnings, and under applied intake its
     * errors. While it has an error this device publishes nothing (section 9.5) and each machine one
     * is inside is not driven; it still reads, fetches and merges.
     */
    val acceptance: StateFlow<DocumentAcceptance.Verdict?> = _acceptance.asStateFlow()

    // Seeded before anything evaluates the gate, so the first start of this build drives exactly
    // what the one before it did.
    private val approvals = LaunchApprovals(prefs).also { it.seedIfMissing(_text.value, _document.value?.root) }

    private val _launch = MutableStateFlow(gate(settledRecord()))

    /**
     * What every machine is driven from: [config] with each command, grammar and the update source
     * as this device approved them, and what waits for approval.
     */
    val launch: StateFlow<LaunchGate.Effective> = _launch.asStateFlow()

    private val _launchProblem = MutableStateFlow(approvals.read().problem)

    /** Why the record of approved commands could not be read, when it could not. Everything waits meanwhile. */
    val launchProblem: StateFlow<String?> = _launchProblem.asStateFlow()

    private val _provenance = MutableStateFlow(storedProvenance())

    /** Which setup this is, which revision, and what it descends from. */
    val provenance: StateFlow<SetupProvenance?> = _provenance.asStateFlow()

    private val _divergence = MutableStateFlow<SetupDivergence?>(null)

    /** A copy that neither descends from nor is descended from this one. */
    val divergence: StateFlow<SetupDivergence?> = _divergence.asStateFlow()

    private val _clash = MutableStateFlow<SetupIdentityClash?>(null)

    /** A machine carrying an entirely different setup. */
    val identityClash: StateFlow<SetupIdentityClash?> = _clash.asStateFlow()

    private val _source = MutableStateFlow(storedSource())

    /** The machine the setup was last fetched from, so the fields start where they were left. */
    val source: StateFlow<SetupSource?> = _source.asStateFlow()

    /**
     * Puts a document typed or pasted in here into force.
     *
     * Local intake (contract section 9.7): a document with no `controller` block, or a block with no
     * id, gets a fresh setup id, revision 1 and no ancestry, keeping the block's other keys, and is
     * then checked like any other. The phone is a peer and the setup it creates belongs to the fleet
     * rather than to the phone. A document that already names a setup keeps it and counts as an edit
     * of whatever this device was holding, so pasting a colleague's export and then editing it stays
     * in that setup's history.
     *
     * Nothing is stored on a failure, on purpose: an app that is working against a good
     * configuration must not lose it because a half-finished edit was applied by accident.
     *
     * A paste is an edit made on this device, so every command it changes is approved, at its new
     * value, in the same write as the document (contract section 9.9).
     */
    fun applyPasted(text: String, deviceName: String?): String? = paste(text, deviceName, Approving.EDIT)

    /**
     * Puts a setup fetched from a machine into force, once every launch value in it was shown and
     * the person confirmed it.
     *
     * Enrolment (contract section 9.9): on a device that applies no setup yet, confirming approves
     * every one of them. On a device that already applies one it is taking a different document,
     * which approves nothing, and anything this device has not approved waits for review.
     */
    fun applyEnrolment(text: String, deviceName: String?): String? =
        paste(text, deviceName, if (_document.value == null) Approving.EVERYTHING else Approving.NOTHING)

    private fun paste(text: String, deviceName: String?, approving: Approving): String? {
        val verdict = DocumentAcceptance.check(text, Intake.LOCAL)
        if (!verdict.valid) return verdict.errors.firstOrNull()?.message ?: "That configuration could not be read."
        val parsed = ControllerDocument.parse(text).getOrElse { return it.message }
        val applied = _document.value
        val document = when {
            verdict.assignsIdentity -> parsed.withFreshIdentity(deviceName, now())
            // Same setup, and this device already had a copy: an ordinary edit, so it descends from
            // what was here and every machine will fast-forward onto it.
            applied != null && applied.setupId == parsed.setupId ->
                parsed.asEditOf(applied, deviceName, now())

            else -> parsed
        }
        return store(document, approving = approving).failure
    }

    /**
     * Puts an edit of the applied document into force.
     *
     * R2. The edit becomes current at once, because an edit made with no machine in reach is a real
     * edit and not a draft; publishing is what happens on the next poll of each machine. The
     * commands it changes are approved in the same write.
     */
    fun applyEdit(edited: ControllerDocument, deviceName: String?): String? {
        val applied = _document.value
            ?: return store(edited.withFreshIdentity(deviceName, now()), approving = Approving.EDIT).failure
        if (edited.canonicalText == applied.canonicalText) return null
        return store(edited.asEditOf(applied, deviceName, now()), approving = Approving.EDIT).failure
    }

    /**
     * Records exactly the values [entries] showed, at their places: Approve on the review (contract
     * section 9.9). A newer value offered since is a new entry, and stays one.
     *
     * An entry that cannot be approved is left as it is. A value that cannot be shown faithfully can
     * only be reverted or left.
     */
    fun approveLaunch(entries: List<LaunchReviewEntry>): String? {
        val changes = entries.filter { it.approvable }.map { it.change() }
        if (changes.isEmpty()) return "Nothing here can be approved. It can only be reverted or left."
        val record = synchronized(HEAD_LOCK) {
            val before = prefs.all
            val editor = prefs.edit()
            val record = approvals.update(editor, _document.value?.root, changes)
            if (!committed(editor, before)) return "This device could not save the approval just now, so nothing was approved."
            record
        }
        announce(record)
        return null
    }

    /**
     * The hash of the document in force, read from storage, which is what every compare-and-swap
     * here is made against. Null when there is none.
     */
    fun appliedHash(): String? = synchronized(HEAD_LOCK) { storedHash() }

    /**
     * Takes a copy off a machine: automatically along strict descent, or as Take theirs or following
     * another setup, each of which a person asked for.
     *
     * [text] is stored exactly as it was read (canonicalised, never re-serialised), and only if it
     * hashes to [expectedRemoteHash], the hash that justified taking it: bytes that do not are not the
     * document that was promised, and they are refused rather than repaired. They replace the
     * document in force only if that is still [expectedLocalHash], the head the decision was made
     * against, null meaning none; otherwise nothing is written and [SetupOutcome.HeadMoved] says so.
     */
    fun adoptFromMachine(
        text: String,
        expectedRemoteHash: String?,
        expectedLocalHash: String?,
        machineName: String,
    ): SetupOutcome {
        if (expectedRemoteHash.isNullOrBlank()) {
            return SetupOutcome.Invalid("$machineName did not say which setup it holds, so nothing was applied.")
        }
        val parsed = ControllerDocument.parse(text).getOrElse {
            return SetupOutcome.Invalid(it.message ?: "That setup could not be read.")
        }
        if (parsed.hash != expectedRemoteHash) {
            return SetupOutcome.Invalid(
                "$machineName served a setup whose contents do not match the hash it reported. " +
                    "Nothing was applied.",
            )
        }
        return when (val stored = store(parsed, readFrom = machineName, head = Head.Is(expectedLocalHash))) {
            Stored.Written -> SetupOutcome.Applied("Updated from $machineName.")
            Stored.Unchanged -> SetupOutcome.Unchanged
            Stored.HeadMoved -> SetupOutcome.HeadMoved
            is Stored.Refused -> SetupOutcome.Invalid(stored.reason)
            is Stored.NotSaved -> SetupOutcome.NotSaved(stored.reason)
        }
    }

    /**
     * Remembers a divergence for the user to settle. Nothing is applied while one is outstanding.
     *
     * [theirText] is the machine's document, already checked to hash to what it reported, and what
     * the screen says about the other side comes from those bytes. The base is the newest ancestor of
     * this device's document that theirs descends from too and whose kept copy still hashes to it
     * (contract section 9.8); a copy that does not is no base at all.
     */
    fun recordDivergence(machineId: String, machineName: String, reason: String, theirText: String) {
        val theirs = ControllerDocument.parse(theirText).getOrNull() ?: return
        val mine = _provenance.value
        _divergence.value = SetupDivergence(
            machineId = machineId,
            machineName = machineName,
            reason = reason,
            theirText = theirs.canonicalText,
            theirProvenance = provenanceOf(theirs, machineName),
            mineProvenance = mine,
            baseHash = SetupMerge.commonBase(mine?.hash, theirs.hash, mine?.lineage.orEmpty(), theirs.lineage, revisions::get),
            at = System.currentTimeMillis(),
        )
    }

    fun clearDivergence() {
        _divergence.value = null
    }

    /** Remembers a machine carrying another setup entirely, from its verified bytes. */
    fun recordIdentityClash(machineId: String, machineName: String, reason: String, theirText: String) {
        val theirs = ControllerDocument.parse(theirText).getOrNull() ?: return
        _clash.value = SetupIdentityClash(
            machineId = machineId,
            machineName = machineName,
            reason = reason,
            theirText = theirs.canonicalText,
            theirProvenance = provenanceOf(theirs, machineName),
            mineProvenance = _provenance.value,
            at = System.currentTimeMillis(),
        )
    }

    fun clearIdentityClash() {
        _clash.value = null
    }

    /**
     * Puts a merged document into force: Merge, or Keep mine, which is Merge with every entry
     * answered mine (contract section 9.8). The merge already wrote its identity, a lineage that
     * carries both heads, so every machine on either branch takes it as an ordinary fast-forward.
     *
     * It replaces the document in force only if that is still [expectedHead], the one the
     * divergence screen was built from.
     */
    fun applyMerge(mergedText: String, expectedHead: String?): SetupOutcome {
        val merged = ControllerDocument.parse(mergedText).getOrElse {
            return SetupOutcome.Invalid(it.message ?: "The merged setup could not be read.")
        }
        return when (val stored = store(merged, head = Head.Is(expectedHead))) {
            Stored.Written -> SetupOutcome.Applied("Merged.")
            Stored.Unchanged -> SetupOutcome.Unchanged
            Stored.HeadMoved -> SetupOutcome.HeadMoved
            is Stored.Refused -> SetupOutcome.Invalid(stored.reason)
            is Stored.NotSaved -> SetupOutcome.NotSaved(stored.reason)
        }
    }

    /** The document a machine is holding, kept so a merge has a base to work from. */
    fun rememberRevision(hash: String?, text: String) {
        hash?.takeIf { it.isNotBlank() }?.let { revisions.remember(it, text) }
    }

    fun revision(hash: String?): String? = revisions.get(hash)

    fun rememberSource(source: SetupSource) {
        prefs.edit()
            .putString(KEY_SOURCE_HOST, source.host)
            .putInt(KEY_SOURCE_PORT, source.port)
            .putString(KEY_SOURCE_USER, source.user)
            .apply()
        _source.value = source
    }

    /** What the document in force has to be at the moment of writing for a write to go ahead. */
    private sealed interface Head {
        /** Whatever it is: an edit made here, a paste, enrolment. */
        data object Any : Head

        /** Exactly the document with this hash, or none at all when it is null. */
        data class Is(val hash: String?) : Head
    }

    /** What became of a write. */
    private sealed interface Stored {
        data object Written : Stored
        data object Unchanged : Stored
        data object HeadMoved : Stored
        data class Refused(val reason: String) : Stored

        /** The disk would not take the write, and everything is as it was before it. */
        data class NotSaved(val reason: String) : Stored

        val failure: String?
            get() = when (this) {
                is Refused -> reason
                is NotSaved -> reason
                else -> null
            }
    }

    /**
     * Stores [document] as the one in force.
     *
     * [approving] says what it approves, and only an edit made here and enrolment approve anything.
     * Taking a copy off a machine, a merge, keeping this device's copy and following another setup
     * are all adoption: the document applies in full, and each command in it this device did not
     * approve waits.
     *
     * The check of [head] and the write are one step under [HEAD_LOCK], and the write is a commit,
     * so no other store in this process can come between reading the head and replacing it. A
     * commit the disk refused is undone, and nothing is announced.
     */
    private fun store(
        document: ControllerDocument,
        readFrom: String? = null,
        approving: Approving = Approving.NOTHING,
        head: Head = Head.Any,
    ): Stored {
        val text = document.canonicalText
        val accepted = acceptControllerConfig(text).getOrElse {
            return Stored.Refused(it.message ?: "That configuration could not be read.")
        }

        val identity = document.identity
        val provenance = SetupProvenance(
            authority = identity?.id,
            revision = identity?.revision,
            hash = document.hash,
            lineage = identity?.lineage.orEmpty(),
            updatedAt = identity?.updatedAt,
            sourceKind = identity?.source,
            device = identity?.device,
            readFrom = readFrom ?: "this device",
        )

        val record = synchronized(HEAD_LOCK) {
            if (head is Head.Is) {
                val current = storedHash()
                if (current == document.hash) return Stored.Unchanged
                if (current != head.hash) return Stored.HeadMoved
            }

            val after = LaunchValues.projection(document.root)
            val changes = when (approving) {
                Approving.NOTHING -> emptyList()
                Approving.EDIT -> LaunchValues.changes(_document.value?.let { LaunchValues.projection(it.root) }, after)
                Approving.EVERYTHING -> LaunchValues.changes(null, after)
            }

            // The approvals go into the same edit as the document they belong to, recorded before
            // anything can read the document back, so an edit made here never shows up for review.
            val before = prefs.all
            val editor = prefs.edit()
                .putString(KEY_TEXT, text)
                .putString(KEY_HASH, provenance.hash)
                .putString(KEY_AUTHORITY, provenance.authority)
                .putString(KEY_REVISION, provenance.revision?.toString())
                .putString(KEY_UPDATED_AT, provenance.updatedAt)
                .putString(KEY_LINEAGE, provenance.lineage.joinToString("\n"))
                .putString(KEY_SOURCE_KIND, provenance.sourceKind)
                .putString(KEY_DEVICE, provenance.device)
                .putString(KEY_PROVENANCE_SOURCE, provenance.readFrom)
            val record = approvals.update(editor, document.root, changes)
            if (!committed(editor, before)) {
                return Stored.NotSaved("This device could not save the setup just now, so nothing changed.")
            }

            // Kept before it is announced, so a merge that needs this revision as a base can find
            // it even if the app is killed in the next moment.
            revisions.remember(document.hash, text)

            _provenance.value = provenance
            _text.value = text
            _document.value = document
            _config.value = accepted.config
            _acceptance.value = accepted.verdict
            _divergence.value = null
            record
        }
        announce(record)
        return Stored.Written
    }

    /** The hash in storage, which only [store] writes. Called under [HEAD_LOCK]. */
    private fun storedHash(): String? = prefs.getString(KEY_HASH, null)?.takeIf { it.isNotBlank() }

    /**
     * Writes [editor] to disk, or puts the preferences back to [before] and says it did not.
     *
     * A commit and not an apply, because what is announced is what gets published: a document sent
     * to machines must still be the one in force here if the app is killed a moment later, and the
     * next compare-and-swap must read the head the last write left. A commit that could not reach
     * the disk has changed the preferences in memory all the same, which is where that head is read
     * from, so it is undone there: a store left ahead of what it announced would refuse every
     * adoption as a moved head and read the machine again on every status. Only text is written
     * beside the document, so text is all there is to put back.
     */
    private fun committed(editor: SharedPreferences.Editor, before: Map<String, *>): Boolean {
        if (editor.commit()) return true
        val after = prefs.all
        // An apply: memory is what has to be right at once. The disk still holds what it held
        // before the refused write, and this brings it back to the same on its own time.
        prefs.edit {
            for (key in after.keys + before.keys) {
                val was = before[key]
                if (was == after[key]) continue
                when (was) {
                    null -> remove(key)
                    is String -> putString(key, was)
                }
            }
        }
        return false
    }

    /** What a document says about itself, for the divergence and different-setup screens. */
    private fun provenanceOf(document: ControllerDocument, machineName: String): SetupProvenance {
        val identity = document.identity
        return SetupProvenance(
            authority = identity?.id,
            revision = identity?.revision,
            hash = document.hash,
            lineage = identity?.lineage.orEmpty(),
            updatedAt = identity?.updatedAt,
            sourceKind = identity?.source,
            device = identity?.device,
            readFrom = machineName,
        )
    }

    /** What an applied document approves. */
    private enum class Approving {
        /** Adoption of any kind. */
        NOTHING,

        /** An edit made on this device: each launch value it changed. */
        EDIT,

        /** Enrolment: every launch value of the document. */
        EVERYTHING,
    }

    /** The gate over what is in force, for [record]. */
    private fun gate(record: LaunchApprovals.Record): LaunchGate.Effective =
        LaunchGate.effective(_config.value, _document.value?.root, record.values)

    private fun announce(record: LaunchApprovals.Record) {
        _launchProblem.value = record.problem
        _launch.value = gate(record)
    }

    /**
     * The record at load, with what the applied document reuses recorded at its places, written back
     * when that changed it: the gate's evaluation at load is an evaluation like any other.
     */
    private fun settledRecord(): LaunchApprovals.Record {
        val before = approvals.read()
        val editor = prefs.edit()
        val settled = approvals.update(editor, _document.value?.root, emptyList())
        if (settled != before) editor.apply()
        return settled
    }

    private fun storedProvenance(): SetupProvenance? {
        val hash = prefs.getString(KEY_HASH, null)?.takeIf { it.isNotBlank() }
        val authority = prefs.getString(KEY_AUTHORITY, null)?.takeIf { it.isNotBlank() }
        if (hash == null && authority == null) return null
        return SetupProvenance(
            authority = authority,
            revision = prefs.getString(KEY_REVISION, null)?.toLongOrNull(),
            hash = hash,
            lineage = prefs.getString(KEY_LINEAGE, null)
                .orEmpty()
                .split('\n')
                .filter { it.isNotBlank() },
            updatedAt = prefs.getString(KEY_UPDATED_AT, null)?.takeIf { it.isNotBlank() },
            sourceKind = prefs.getString(KEY_SOURCE_KIND, null)?.takeIf { it.isNotBlank() },
            device = prefs.getString(KEY_DEVICE, null)?.takeIf { it.isNotBlank() },
            readFrom = prefs.getString(KEY_PROVENANCE_SOURCE, null)?.takeIf { it.isNotBlank() },
        )
    }

    private fun storedSource(): SetupSource? {
        val host = prefs.getString(KEY_SOURCE_HOST, null)?.takeIf { it.isNotBlank() } ?: return null
        return SetupSource(
            host = host,
            port = prefs.getInt(KEY_SOURCE_PORT, 22),
            user = prefs.getString(KEY_SOURCE_USER, null).orEmpty(),
        )
    }

    private fun now(): String = Instant.now().toString()

    private companion object {
        /**
         * One lock for every store in the process. Two models of the same preferences, such as the
         * one a rebuilt activity makes beside the old one, still take turns at the head (A6).
         */
        val HEAD_LOCK = Any()

        const val KEY_TEXT = "controller-config"
        const val KEY_HASH = "controller-hash"
        const val KEY_AUTHORITY = "controller-authority"
        const val KEY_REVISION = "controller-revision"
        const val KEY_UPDATED_AT = "controller-updated-at"
        const val KEY_LINEAGE = "controller-lineage"
        const val KEY_SOURCE_KIND = "controller-source-kind"
        const val KEY_DEVICE = "controller-device"
        const val KEY_PROVENANCE_SOURCE = "controller-source"
        const val KEY_SOURCE_HOST = "setup-source-host"
        const val KEY_SOURCE_PORT = "setup-source-port"
        const val KEY_SOURCE_USER = "setup-source-user"
    }
}
