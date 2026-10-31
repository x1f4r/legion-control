package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.ControllerSource
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.agent.describeRefusal
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchProjection
import com.x1f4r.legioncontrol.data.LaunchReviewEntry
import com.x1f4r.legioncontrol.data.LaunchScope
import com.x1f4r.legioncontrol.data.LaunchTuple
import com.x1f4r.legioncontrol.data.LaunchValues
import com.x1f4r.legioncontrol.data.readControllerConfig
import com.x1f4r.legioncontrol.data.text
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import java.util.Locale

/**
 * The commands from the shared setup that wait for this device, and what can be done about each.
 *
 * Contract section 9.9. Every adopted document is in force, and what it changed in a command, a key
 * file or the update source runs on this device only once somebody holding it said so. Each row
 * shows what this device approved there beside what the setup now offers, argv one item per line,
 * so the person deciding reads exactly what would run. Not now is the quiet answer and changes
 * nothing; the approved command keeps running.
 */
@Composable
fun LaunchReviewCard(app: AppModel, review: List<LaunchReviewEntry>) {
    Text("Approve commands from the shared setup", style = MaterialTheme.typography.titleLarge)
    Spacer(Modifier.height(6.dp))
    app.setupProvenance?.let { QuietText(revisionLine(it)) }
    ExplanationText(
        "Approving lets this device run this command over ssh with its own key.",
        Modifier.padding(top = 6.dp),
    )

    review.groupBy { it.machineId }.forEach { (machineId, entries) ->
        val machine = app.machines.firstOrNull { it.machine.id == machineId }?.machine
        SectionHeading(
            when {
                machineId == null -> "The whole setup"
                else -> machine?.name ?: machineId
            },
        )
        entries.forEach { entry ->
            val systemName = machine?.system(entry.systemId)?.name ?: entry.systemId
            ReviewRow(app, entry, systemName)
        }
    }

    Spacer(Modifier.height(8.dp))
    Actions {
        val approvable = review.filter { it.approvable }
        if (review.size > 1 && approvable.isNotEmpty()) {
            PlainAction("Approve all", enabled = true) { app.approveLaunch(approvable) }
        }
        PlainAction("Not now", enabled = true, emphasis = true) {
            app.deferLaunchReview(app.setupProvenance?.hash)
        }
    }
    app.launchNote?.let { QuietText(it, Modifier.padding(top = 2.dp)) }
}

@Composable
private fun ReviewRow(app: AppModel, entry: LaunchReviewEntry, systemName: String?) {
    Column(Modifier.padding(vertical = 4.dp)) {
        Text(
            when (entry.scope) {
                LaunchScope.SYSTEM -> systemName ?: "A system"
                LaunchScope.IDENTITY_FILE -> "Key file"
                LaunchScope.REPO -> "Where updates come from"
                LaunchScope.LOCAL -> "The Mac's own agent"
            },
            style = MaterialTheme.typography.bodyMedium,
        )
        DetailRow("Approved", alignment = Alignment.Top) {
            if (entry.approved == null) ValueText(null, "nothing yet") else LaunchValueLines(entry.scope, entry.approved)
        }
        DetailRow("Offered", alignment = Alignment.Top) { LaunchValueLines(entry.scope, entry.offered) }
        when (entry.scope) {
            // Shown, because the record is this device's and Approve all should settle it, but said
            // plainly: nothing on this phone reads either.
            LaunchScope.IDENTITY_FILE ->
                QuietText("This phone signs in with its own key and does not use this file.")

            LaunchScope.LOCAL -> QuietText("Only a Mac runs this, on itself.")
            LaunchScope.SYSTEM, LaunchScope.REPO -> Unit
        }
        if (!entry.approvable) QuietText(unapprovableReason(entry), Modifier.padding(top = 2.dp))
        val revertible = app.canRevertLaunch(entry)
        Actions {
            if (entry.approvable) PlainAction("Approve", enabled = true) { app.approveLaunch(listOf(entry)) }
            if (revertible) PlainAction("Revert", enabled = app.editor.revertWaits == null) { app.revertLaunch(entry) }
        }
        // The editor is a page beside this one, and an edit in progress there is what Revert waits for.
        if (revertible) app.editor.revertWaits?.let { QuietText(it) }
    }
}

/**
 * A launch value as it would be used: argv one item per line and then the grammar, or the path or
 * repository on its own. Characters that would hide what the text says are written out as escapes.
 */
@Composable
internal fun LaunchValueLines(scope: LaunchScope, value: JsonElement) {
    Column {
        when (scope) {
            LaunchScope.SYSTEM -> LaunchTuple.from(value)?.let { TupleLines(it) }
                ?: QuietText("something this build cannot read as a command")

            LaunchScope.IDENTITY_FILE, LaunchScope.REPO -> (value as? JsonPrimitive)?.takeIf { it.isString }
                ?.let { CommandText(shown(it.content)) }
                ?: QuietText("something this build cannot read")

            LaunchScope.LOCAL -> when (val agent = (value as? JsonObject)?.get("agent")) {
                is JsonNull -> QuietText("node with the default path")
                is JsonPrimitive -> if (agent.isString) CommandText(shown(agent.content)) else QuietText("something this build cannot read")
                else -> QuietText("something this build cannot read")
            }
        }
    }
}

@Composable
private fun TupleLines(tuple: LaunchTuple) {
    val agent = tuple.agent
    if (agent == null) {
        QuietText("no command, so nothing is run")
        return
    }
    agent.forEach { CommandText(shown(it)) }
    QuietText(grammarLabel(tuple.grammar), Modifier.padding(top = 2.dp))
}

/**
 * A fetched setup, shown in full before anything of it is applied: enrolment (contract section 9.9).
 *
 * Every machine and system with its argv and grammar, and any key file, update source or local agent.
 * Confirm is the only way past it, and Cancel leaves this device exactly as it was.
 */
@Composable
fun EnrolmentSummary(app: AppModel) {
    val preview = app.enrolment.preview ?: return
    Spacer(Modifier.height(10.dp))
    SectionHeading("What the setup from ${preview.host} runs", showsRule = false)
    ExplanationText(
        if (preview.approves) {
            "Confirming puts it in force here and approves every command below, so this device runs " +
                "them over ssh with its own key."
        } else {
            "This device already has a setup. Confirming puts this one in force, and every command " +
                "below that this device has not approved waits for approval."
        },
    )
    preview.machines.forEach { machine ->
        Spacer(Modifier.height(8.dp))
        Text(machine.name, style = MaterialTheme.typography.titleSmall)
        machine.systems.forEach { system ->
            DetailRow(system.name, alignment = Alignment.Top) { Column { TupleLines(system.tuple) } }
        }
        machine.identityFile?.let { path -> DetailRow("Key file") { CommandText(shown(path)) } }
    }
    preview.repo?.let { repo -> DetailRow("Updates from") { CommandText(shown(repo)) } }
    preview.local?.let { local ->
        DetailRow("Mac's own agent") {
            local.agent?.let { CommandText(shown(it)) } ?: QuietText("node with the default path")
        }
    }
    Actions {
        PlainAction("Confirm", enabled = true, emphasis = true) { app.confirmEnrolment() }
        PlainAction("Cancel", enabled = true) { app.cancelEnrolment() }
    }
}

/** A fetched setup and every launch value in it, waiting for Confirm or Cancel. */
data class EnrolmentPreview(
    /** Exactly what the machine served, which is what Confirm applies. */
    val text: String,
    val host: String,
    /** The hash the machine reported, which [text] was checked against and is stored only under. */
    val servedHash: String,
    /** The hash in storage when this was shown, null for none: Confirm replaces only that one. */
    val head: String?,
    /** Whether confirming approves what is shown: only on a device that applies no setup yet. */
    val approves: Boolean,
    val machines: List<Machine>,
    /** A repository other than the built-in one, or null. */
    val repo: String?,
    val local: LaunchProjection.Local?,
) {
    data class Machine(val name: String, val systems: List<System>, val identityFile: String?)

    data class System(val name: String, val tuple: LaunchTuple)

    companion object {
        /**
         * The preview of [text], or why it would be refused. A document that could not be applied is
         * said so now rather than after somebody read through it and pressed Confirm.
         */
        fun of(text: String, host: String, servedHash: String, head: String?, approves: Boolean): Result<EnrolmentPreview> {
            val document = ControllerDocument.parse(text).getOrElse { return Result.failure(it) }
            readControllerConfig(document.canonicalText).exceptionOrNull()?.let { return Result.failure(it) }
            val projection = LaunchValues.projection(document.root)
            val rawMachines = (document.root["machines"] as? JsonArray).orEmpty().filterIsInstance<JsonObject>()
            return Result.success(
                EnrolmentPreview(
                    text = text,
                    host = host,
                    servedHash = servedHash,
                    head = head,
                    approves = approves,
                    machines = projection.machines.map { machine ->
                        val raw = rawMachines.firstOrNull { it.text("id") == machine.id }
                        val rawSystems = (raw?.get("systems") as? JsonArray).orEmpty().filterIsInstance<JsonObject>()
                        Machine(
                            name = raw?.text("name") ?: machine.id,
                            systems = machine.systems.map { system ->
                                System(
                                    name = rawSystems.firstOrNull { it.text("id") == system.id }?.text("name") ?: system.id,
                                    tuple = system.tuple,
                                )
                            },
                            identityFile = machine.identityFile,
                        )
                    },
                    repo = projection.repo,
                    local = projection.local,
                ),
            )
        }
    }
}

/** "Revision N, says it was written by <device> (<source>). Not verified." */
internal fun revisionLine(provenance: SetupProvenance): String {
    val revision = provenance.revision?.let { "Revision $it" } ?: "This revision"
    val device = provenance.device?.takeIf { it.isNotBlank() }?.let(::shown)
    val source = ControllerSource.describe(provenance.sourceKind)?.let(::shown)
    val author = when {
        device != null && source != null -> "$device ($source)"
        else -> device ?: source
    }
    return if (author == null) {
        "$revision does not say who wrote it. Not verified."
    } else {
        "$revision, says it was written by $author. Not verified."
    }
}

/** How the line is written, in words. */
internal fun grammarLabel(grammar: String?): String = when (RemoteShell.fromGrammar(grammar)) {
    RemoteShell.POSIX -> "written for a POSIX shell"
    RemoteShell.DISPATCH -> "written for the agent's restricted dispatcher"
    RemoteShell.CMD -> "written for cmd.exe"
    RemoteShell.POWERSHELL -> "written for PowerShell"
    null -> grammar?.removePrefix("refused:")?.let { "cannot be written: ${describeRefusal(it, null)}" }
        ?: "no grammar"
}

private fun unapprovableReason(entry: LaunchReviewEntry): String = when (entry.scope) {
    LaunchScope.SYSTEM -> if (entry.offeredTuple?.grammar?.startsWith("refused:") == true) {
        "Its shell settings leave no way to write this command. It can only be reverted or left."
    } else {
        "An argument holds a control, invisible or bidirectional formatting character, or is longer " +
            "than 4096 bytes, so what it says cannot be checked by reading it. It can only be reverted or left."
    }

    LaunchScope.REPO -> "This is not a GitHub owner/name. It can only be reverted or left."
    LaunchScope.IDENTITY_FILE, LaunchScope.LOCAL ->
        "It holds a control, invisible or bidirectional formatting character, or is longer than 4096 " +
            "bytes, so what it says cannot be checked by reading it. It can only be reverted or left."
}

/**
 * [text] with every character that could hide what it says written as `\uXXXX`.
 *
 * A right-to-left override makes a path display as a different one, and a zero-width space makes two
 * different paths look the same. Spelled out, the person deciding sees that something is there.
 */
internal fun shown(text: String): String = buildString {
    for (char in text) {
        if (HIDDEN.matches(char.toString())) {
            append("\\u").append(char.code.toString(16).uppercase(Locale.ROOT).padStart(4, '0'))
        } else {
            append(char)
        }
    }
}

private val HIDDEN = Regex("[\\u0000-\\u001F\\u007F-\\u009F\\u200B-\\u200F\\u202A-\\u202E\\u2060-\\u2064\\u2066-\\u2069\\uFEFF]")
