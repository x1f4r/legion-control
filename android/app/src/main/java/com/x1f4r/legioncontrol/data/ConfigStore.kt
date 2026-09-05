package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
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
 */
data class SetupDivergence(
    val machineName: String,
    val reason: String,
    /** Their document, exactly as the machine served it. */
    val theirText: String,
    val theirProvenance: SetupProvenance,
    val mineProvenance: SetupProvenance?,
    /** The newest ancestor both sides share, when this device kept a copy of it. */
    val baseHash: String?,
    val at: Long,
)

/** Two different setups entirely, which is a replace-or-adopt question and never a merge. */
data class SetupIdentityClash(
    val machineName: String,
    val reason: String,
    val theirText: String,
    val theirProvenance: SetupProvenance,
    val mineProvenance: SetupProvenance?,
    val at: Long,
)

/** What became of a document a machine offered. */
sealed interface SetupOutcome {
    data class Applied(val reason: String) : SetupOutcome
    data object Unchanged : SetupOutcome
    data class Diverged(val reason: String) : SetupOutcome
    data class DifferentSetup(val reason: String) : SetupOutcome
    data class Invalid(val reason: String) : SetupOutcome
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
 */
class ConfigStore(
    context: Context,
    private val revisions: RevisionCache,
) {
    private val prefs: SharedPreferences =
        context.applicationContext.getSharedPreferences("legion-config", Context.MODE_PRIVATE)

    private val _text = MutableStateFlow(prefs.getString(KEY_TEXT, null).orEmpty())

    /** Exactly what is stored, so the field on screen starts as what was last applied. */
    val text: StateFlow<String> = _text.asStateFlow()

    private val _document = MutableStateFlow(
        _text.value.takeIf { it.isNotBlank() }?.let { ControllerDocument.parse(it).getOrNull() },
    )

    /** The applied document as a tree, which is what an edit works on. */
    val document: StateFlow<ControllerDocument?> = _document.asStateFlow()

    private val _config = MutableStateFlow(readControllerConfig(_text.value).getOrNull())

    /** The configuration in force. Null means there is none, which is the first-run state. */
    val config: StateFlow<ControllerConfig?> = _config.asStateFlow()

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
     * A document that already names a setup keeps it and counts as an edit of whatever this device
     * was holding, so pasting a colleague's export and then editing it stays in that setup's
     * history. One that names no setup gets a fresh id, revision 1 and no ancestry, which is R3:
     * the phone is a peer and the setup it creates belongs to the fleet rather than to the phone.
     *
     * Nothing is stored on a failure, on purpose: an app that is working against a good
     * configuration must not lose it because a half-finished edit was applied by accident.
     */
    fun applyPasted(text: String, deviceName: String?): String? {
        val parsed = ControllerDocument.parse(text).getOrElse { return it.message }
        val applied = _document.value
        val document = when {
            parsed.identity == null -> parsed.withFreshIdentity(deviceName, now())
            // Same setup, and this device already had a copy: an ordinary edit, so it descends from
            // what was here and every machine will fast-forward onto it.
            applied != null && applied.setupId == parsed.setupId ->
                parsed.asEditOf(applied, deviceName, now())

            else -> parsed
        }
        return store(document)
    }

    /**
     * Puts an edit of the applied document into force.
     *
     * R2. The edit becomes current at once, because an edit made with no machine in reach is a real
     * edit and not a draft; publishing is what happens on the next poll of each machine.
     */
    fun applyEdit(edited: ControllerDocument, deviceName: String?): String? {
        val applied = _document.value ?: return store(edited.withFreshIdentity(deviceName, now()))
        if (edited.canonicalText == applied.canonicalText) return null
        return store(edited.asEditOf(applied, deviceName, now()))
    }

    /**
     * Takes a copy off a machine, which is only ever called when it descends from this one.
     *
     * The hash is checked against what the machine said it was serving before anything is applied: a
     * document whose bytes do not hash to the hash that was reported is not the document that was
     * promised, and it is refused rather than repaired.
     */
    fun adoptFromMachine(
        text: String,
        expectedHash: String?,
        machineName: String,
    ): SetupOutcome {
        val parsed = ControllerDocument.parse(text).getOrElse {
            return SetupOutcome.Invalid(it.message ?: "That setup could not be read.")
        }
        if (!expectedHash.isNullOrBlank() && parsed.hash != expectedHash) {
            return SetupOutcome.Invalid(
                "$machineName served a setup whose contents do not match the hash it reported. " +
                    "Nothing was applied.",
            )
        }
        if (parsed.canonicalText == _text.value) return SetupOutcome.Unchanged
        val failure = store(parsed, readFrom = machineName)
        return if (failure == null) {
            SetupOutcome.Applied("Updated from $machineName.")
        } else {
            SetupOutcome.Invalid(failure)
        }
    }

    /** Remembers a divergence for the user to settle. Nothing is applied while one is outstanding. */
    fun recordDivergence(
        machineName: String,
        reason: String,
        theirText: String,
        theirProvenance: SetupProvenance,
    ) {
        val mine = _provenance.value
        val base = SetupMerge.commonBase(
            myLineage = listOfNotNull(mine?.hash) + mine?.lineage.orEmpty(),
            theirLineage = listOfNotNull(theirProvenance.hash) + theirProvenance.lineage,
        )
        _divergence.value = SetupDivergence(
            machineName = machineName,
            reason = reason,
            theirText = theirText,
            theirProvenance = theirProvenance,
            mineProvenance = mine,
            baseHash = base?.takeIf { revisions.has(it) },
            at = System.currentTimeMillis(),
        )
    }

    fun clearDivergence() {
        _divergence.value = null
    }

    fun recordIdentityClash(
        machineName: String,
        reason: String,
        theirText: String,
        theirProvenance: SetupProvenance,
    ) {
        _clash.value = SetupIdentityClash(
            machineName = machineName,
            reason = reason,
            theirText = theirText,
            theirProvenance = theirProvenance,
            mineProvenance = _provenance.value,
            at = System.currentTimeMillis(),
        )
    }

    fun clearIdentityClash() {
        _clash.value = null
    }

    /**
     * Puts a merged document into force, descending from both sides.
     *
     * The lineage carries both parents, which is what makes the result acceptable to every machine
     * whichever branch it was on: each of them finds its own hash in the ancestry and fast-forwards.
     */
    fun applyMerge(
        merged: ControllerDocument,
        theirProvenance: SetupProvenance,
        deviceName: String?,
    ): String? {
        val mine = _document.value
        val myHash = mine?.hash
        val theirHash = theirProvenance.hash
        val parents = (listOfNotNull(myHash, theirHash) + mine?.lineage.orEmpty() + theirProvenance.lineage)
            .distinct()
            .take(MAX_LINEAGE)
        val identity = DocumentIdentity(
            id = mine?.setupId ?: theirProvenance.authority ?: ControllerDocument.newSetupId(),
            name = mine?.identity?.name,
            revision = maxOf(mine?.revision ?: 0L, theirProvenance.revision ?: 0L) + 1L,
            updatedAt = now(),
            source = ControllerDocument.CLIENT_KIND,
            device = deviceName,
            lineage = parents,
        )
        val failure = store(merged.withIdentity(identity))
        if (failure == null) clearDivergence()
        return failure
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

    private fun store(document: ControllerDocument, readFrom: String? = null): String? {
        val text = document.canonicalText
        val typed = readControllerConfig(text)
        typed.exceptionOrNull()?.let { return it.message ?: "That configuration could not be read." }

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

        prefs.edit()
            .putString(KEY_TEXT, text)
            .putString(KEY_HASH, provenance.hash)
            .putString(KEY_AUTHORITY, provenance.authority)
            .putString(KEY_REVISION, provenance.revision?.toString())
            .putString(KEY_UPDATED_AT, provenance.updatedAt)
            .putString(KEY_LINEAGE, provenance.lineage.joinToString("\n"))
            .putString(KEY_SOURCE_KIND, provenance.sourceKind)
            .putString(KEY_DEVICE, provenance.device)
            .putString(KEY_PROVENANCE_SOURCE, provenance.readFrom)
            .apply()

        // Kept before it is announced, so a merge that needs this revision as a base can find it
        // even if the app is killed in the next moment.
        revisions.remember(document.hash, text)

        _provenance.value = provenance
        _text.value = text
        _document.value = document
        _config.value = typed.getOrNull()
        _divergence.value = null
        return null
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
