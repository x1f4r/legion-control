package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.data.MachineConfig
import com.x1f4r.legioncontrol.data.SetupMerge
import com.x1f4r.legioncontrol.data.WakeHelperConfig
import com.x1f4r.legioncontrol.data.withText

/**
 * Editing the shared setup, on a phone.
 *
 * A structured form rather than only a text field, because typing JSON on a phone keyboard is how
 * mistakes get published to every machine. What it edits is still the document itself: every change
 * is a transform of the parsed tree, so a key written by a newer client survives an edit made here
 * untouched. The raw field is still one page along for anything this form cannot express.
 */
@Composable
fun SetupEditorSection(app: AppModel) {
    val editor = app.editor
    Spacer(Modifier.height(6.dp))

    if (editor.working == null) {
        ExplanationText(
            "There is no setup to edit yet. Fetch one from a machine, or paste one, on this device's " +
                "page.",
        )
        return
    }

    ExplanationText(
        "Changes here become this device's setup when you apply them, and are sent to every machine " +
            "on its next check. Editing with nothing in reach is fine: the changes travel when the " +
            "machines do.",
    )

    val changes = editor.changes()

    if (changes.isNotEmpty()) {
        SectionHeading("Not applied yet", note = "${changes.size}")
        changes.forEach { QuietText("${it.label}: ${it.summary}", Modifier.padding(top = 2.dp)) }
    }

    editor.problem?.let {
        Spacer(Modifier.height(8.dp))
        StatusLine(Mark.Bad, it)
    }
    editor.advisories.forEach { QuietText(it, Modifier.padding(top = 4.dp)) }

    Actions {
        PlainAction(
            label = "Apply",
            enabled = editor.hasChanges && editor.isValid,
            emphasis = editor.hasChanges && editor.isValid,
        ) { editor.apply() }
        PlainAction("Discard", enabled = editor.hasChanges) { editor.discard() }
        PlainAction("Close the editor", enabled = !editor.hasChanges) { app.closeEditor() }
    }
    editor.error?.let { QuietText(it, Modifier.padding(top = 2.dp)) }
    editor.note?.let { QuietText(it, Modifier.padding(top = 2.dp)) }

    SitesBlock(app)
    MachinesBlock(app)
}

@Composable
private fun SitesBlock(app: AppModel) {
    val editor = app.editor
    val sites = editor.typed?.sites.orEmpty()

    SectionHeading("Sites", note = if (sites.isEmpty()) "none" else "${sites.size}")
    ExplanationText(
        "A site is one network. Machines at a site can be woken by a device standing on it, and by " +
            "helpers that live there. Two sites may use the same private addresses, and then this " +
            "app says it cannot tell them apart rather than guessing.",
    )

    sites.forEach { site ->
        Spacer(Modifier.height(10.dp))
        HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        Spacer(Modifier.height(8.dp))
        Text(site.displayName, style = MaterialTheme.typography.titleSmall)
        DetailRow("Address starts") {
            PlainField(
                value = site.lanPrefixes.joinToString(" "),
                onValueChange = { editor.setSitePrefixes(site.id, it.split(Regex("[\\s,]+"))) },
                placeholder = "10.0.0.",
            )
        }
        DetailRow("Broadcast") {
            PlainField(
                value = site.broadcast.joinToString(" "),
                onValueChange = { editor.setSiteBroadcasts(site.id, it.split(Regex("[\\s,]+"))) },
                placeholder = "10.0.0.255",
            )
        }
        Actions {
            PlainAction("Remove ${site.displayName}", enabled = true, destructive = true) {
                editor.removeSite(site.id)
            }
        }
    }

    AddRow(
        label = "Add a site",
        idPlaceholder = "attic-house",
        namePlaceholder = "Attic house",
    ) { id, name -> editor.addSite(id, name) }
}

@Composable
private fun MachinesBlock(app: AppModel) {
    val editor = app.editor
    val machines = editor.typed?.machines.orEmpty()

    SectionHeading("Machines", note = "${machines.size}")
    machines.forEach { machine -> MachineEditor(app, machine) }

    AddRow(
        label = "Add a machine",
        idPlaceholder = "tower",
        namePlaceholder = "Tower",
    ) { id, name -> editor.addMachine(id, name) }
}

@Composable
private fun MachineEditor(app: AppModel, machine: MachineConfig) {
    val editor = app.editor
    val open = editor.expanded[machine.id] == true

    Spacer(Modifier.height(10.dp))
    HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
    Spacer(Modifier.height(8.dp))
    Text(
        machine.name?.takeIf { it.isNotBlank() } ?: machine.id,
        style = MaterialTheme.typography.titleSmall,
    )
    QuietText(
        listOfNotNull(
            "${machine.endpoints.size} address${if (machine.endpoints.size == 1) "" else "es"}",
            "${machine.systems.size} system${if (machine.systems.size == 1) "" else "s"}",
            machine.site?.let { "at $it" },
            if (machine.wake != null) "wakeable" else null,
        ).joinToString(" · "),
        Modifier.padding(top = 2.dp),
    )

    Actions {
        PlainAction(if (open) "Close" else "Open", enabled = true) {
            editor.expanded[machine.id] = !open
        }
        PlainAction("Remove", enabled = true, destructive = true) { editor.removeMachine(machine.id) }
    }

    if (!open) return

    DetailRow("Called") {
        PlainField(
            value = machine.name.orEmpty(),
            onValueChange = { editor.setMachineName(machine.id, it) },
            placeholder = machine.id,
        )
    }

    val sites = editor.typed?.sites.orEmpty()
    if (sites.isNotEmpty()) {
        DetailRow("At", alignment = Alignment.Top) {
            Actions {
                PlainAction("Nowhere", enabled = machine.site != null) {
                    editor.setMachineSite(machine.id, null)
                }
                sites.forEach { site ->
                    PlainAction(
                        label = site.displayName,
                        enabled = machine.site != site.id,
                        emphasis = machine.site == site.id,
                    ) { editor.setMachineSite(machine.id, site.id) }
                }
            }
        }
    }

    DetailRow("Always on") {
        Actions {
            PlainAction("Yes", enabled = !machine.alwaysOn, emphasis = machine.alwaysOn) {
                editor.setMachineAlwaysOn(machine.id, true)
            }
            PlainAction("No", enabled = machine.alwaysOn, emphasis = !machine.alwaysOn) {
                editor.setMachineAlwaysOn(machine.id, false)
            }
        }
    }
    QuietText(
        "Only changes what a warning says. A wake path whose helpers may all be asleep is called out.",
        Modifier.padding(top = 2.dp),
    )

    EndpointsEditor(app, machine)
    SystemsEditor(app, machine)
    WakeEditor(app, machine)
}

@Composable
private fun EndpointsEditor(app: AppModel, machine: MachineConfig) {
    val editor = app.editor
    SectionHeading("Addresses", note = "${machine.endpoints.size}", showsRule = false)
    machine.endpoints.forEach { endpoint ->
        DetailRow(endpoint.id) {
            Column {
                PlainField(
                    value = endpoint.host,
                    onValueChange = { host ->
                        editor.editEndpoint(machine.id, endpoint.id) { it.withText("host", host) }
                    },
                    placeholder = "host",
                    keyboardType = KeyboardType.Uri,
                )
                PlainField(
                    value = endpoint.user,
                    onValueChange = { user ->
                        editor.editEndpoint(machine.id, endpoint.id) { it.withText("user", user) }
                    },
                    placeholder = "user",
                )
                Actions {
                    PlainAction(
                        label = "On its LAN",
                        enabled = endpoint.kind != "lan",
                        emphasis = endpoint.kind == "lan",
                    ) {
                        editor.editEndpoint(machine.id, endpoint.id) { it.withText("kind", "lan") }
                    }
                    PlainAction(
                        label = "Remote",
                        enabled = endpoint.kind != "remote",
                        emphasis = endpoint.kind == "remote",
                    ) {
                        editor.editEndpoint(machine.id, endpoint.id) { it.withText("kind", "remote") }
                    }
                    PlainAction("Remove", enabled = true, destructive = true) {
                        editor.removeEndpoint(machine.id, endpoint.id)
                    }
                }
            }
        }
    }
    AddEndpointRow(app, machine)
}

@Composable
private fun AddEndpointRow(app: AppModel, machine: MachineConfig) {
    var id by remember { mutableStateOf("") }
    var host by remember { mutableStateOf("") }
    var user by remember { mutableStateOf("") }
    var problem by remember { mutableStateOf<String?>(null) }

    DetailRow("New address") {
        Column {
            PlainField(id, { id = it }, "an id, e.g. lan")
            PlainField(host, { host = it }, "host", keyboardType = KeyboardType.Uri)
            PlainField(user, { user = it }, "user")
            Actions {
                PlainAction("Add", enabled = id.isNotBlank() && host.isNotBlank() && user.isNotBlank()) {
                    problem = app.editor.addEndpoint(machine.id, id, host, user)
                    if (problem == null) {
                        id = ""
                        host = ""
                        user = ""
                    }
                }
            }
            problem?.let { QuietText(it) }
        }
    }
}

@Composable
private fun SystemsEditor(app: AppModel, machine: MachineConfig) {
    val editor = app.editor
    SectionHeading("Systems", note = "${machine.systems.size}", showsRule = false)
    ExplanationText(
        "The command that runs the control agent, written as you would type it. A path with a space " +
            "in it is fine as long as the system says which shell it logs in to.",
    )
    machine.systems.forEach { system ->
        DetailRow(system.name ?: system.id, alignment = Alignment.Top) {
            Column {
                PlainField(
                    value = system.agent.joinToString(" "),
                    onValueChange = { editor.setSystemAgent(machine.id, system.id, it) },
                    placeholder = "/usr/bin/node ~/.legion-control/agent/src/index.mjs",
                )
                Actions {
                    listOf("posix", "powershell", "cmd").forEach { shell ->
                        PlainAction(
                            label = shell,
                            enabled = system.shell != shell,
                            emphasis = system.shell == shell,
                        ) { editor.setSystemShell(machine.id, system.id, shell) }
                    }
                    PlainAction(
                        label = "unset",
                        enabled = system.shell != null,
                    ) { editor.setSystemShell(machine.id, system.id, null) }
                }
                QuietText(
                    when (RemoteShell.fromWire(system.shell)) {
                        RemoteShell.AUTO ->
                            "Not set. Fine while nothing in the command needs quoting; a path with a " +
                                "space in it needs an answer here."

                        RemoteShell.POSIX -> "sh, bash, zsh or fish."
                        RemoteShell.CMD -> "cmd.exe, which is what Windows OpenSSH starts with by default."
                        RemoteShell.POWERSHELL -> "PowerShell, set as this account's DefaultShell."
                    },
                )
                Actions {
                    PlainAction(
                        label = if (system.restricted) "Restricted key" else "Ordinary shell",
                        enabled = true,
                        emphasis = system.restricted,
                    ) { editor.setSystemRestricted(machine.id, system.id, !system.restricted) }
                    PlainAction("Remove", enabled = true, destructive = true) {
                        editor.removeSystem(machine.id, system.id)
                    }
                }
                if (system.restricted) {
                    QuietText(
                        "This key is tied to the agent's own dispatcher, which reads the arguments " +
                            "itself and never lets a shell see them.",
                    )
                }
            }
        }
    }
    AddSystemRow(app, machine)
}

@Composable
private fun AddSystemRow(app: AppModel, machine: MachineConfig) {
    var id by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("") }
    var platform by remember { mutableStateOf("linux") }
    var problem by remember { mutableStateOf<String?>(null) }

    DetailRow("New system") {
        Column {
            PlainField(id, { id = it }, "an id, e.g. linux")
            PlainField(name, { name = it }, "what to call it")
            Actions {
                listOf("linux", "windows", "mac").forEach {
                    PlainAction(it, enabled = platform != it, emphasis = platform == it) {
                        platform = it
                    }
                }
            }
            Actions {
                PlainAction("Add", enabled = id.isNotBlank()) {
                    problem = app.editor.addSystem(machine.id, id, name, platform)
                    if (problem == null) {
                        id = ""
                        name = ""
                    }
                }
            }
            problem?.let { QuietText(it) }
        }
    }
}

/**
 * How this machine gets woken, and by whom.
 *
 * The helper list is the part that matters away from home. A magic packet is a LAN broadcast, so a
 * phone in another city cannot send one; a machine already on that network can, and the order here
 * is the order they are asked in.
 */
@Composable
private fun WakeEditor(app: AppModel, machine: MachineConfig) {
    val editor = app.editor
    val wake = machine.wake
    SectionHeading("Waking", showsRule = false)

    DetailRow("Hardware address") {
        PlainField(
            value = wake?.mac.orEmpty(),
            onValueChange = { editor.setWakeMac(machine.id, it) },
            placeholder = "AA:BB:CC:DD:EE:FF",
        )
    }
    DetailRow("Broadcast") {
        PlainField(
            value = wake?.broadcast.orEmpty().joinToString(" "),
            onValueChange = { editor.setWakeBroadcasts(machine.id, it.split(Regex("[\\s,]+"))) },
            placeholder = "10.0.0.255",
        )
    }
    if (machine.site == null) {
        DetailRow("Its network") {
            PlainField(
                value = wake?.lanPrefix.orEmpty(),
                onValueChange = { editor.setWakeLanPrefix(machine.id, it) },
                placeholder = "10.0.0.",
            )
        }
    }

    val helpers = wake?.effectiveHelpers.orEmpty()
    SectionHeading("Helpers", note = if (helpers.isEmpty()) "none" else "${helpers.size}", showsRule = false)
    ExplanationText(
        "Machines already on this one's network that can send the packet, asked in this order. " +
            "None of them is ever woken automatically.",
    )
    helpers.forEachIndexed { index, helper ->
        val name = editor.typed?.machines?.firstOrNull { it.id == helper.machine }?.name ?: helper.machine
        DetailRow("${index + 1}. $name") {
            Column {
                QuietText("runs \"${helper.action}\"")
                Actions {
                    if (index > 0) {
                        PlainAction("Ask earlier", enabled = true) {
                            editor.promoteHelper(machine.id, index)
                        }
                    }
                    PlainAction("Remove", enabled = true, destructive = true) {
                        editor.removeHelper(machine.id, index)
                    }
                }
            }
        }
    }
    AddHelperRow(app, machine, helpers)
}

@Composable
private fun AddHelperRow(
    app: AppModel,
    machine: MachineConfig,
    helpers: List<WakeHelperConfig>,
) {
    var helperId by remember { mutableStateOf("") }
    var action by remember { mutableStateOf("") }
    var problem by remember { mutableStateOf<String?>(null) }
    val candidates = app.editor.typed?.machines.orEmpty().filter { it.id != machine.id }

    DetailRow("Add a helper", alignment = Alignment.Top) {
        Column {
            Actions {
                candidates.forEach { candidate ->
                    PlainAction(
                        label = candidate.name ?: candidate.id,
                        enabled = helperId != candidate.id,
                        emphasis = helperId == candidate.id,
                    ) { helperId = candidate.id }
                }
            }
            PlainField(action, { action = it }, "the action id on that machine, e.g. wake-tower")
            Actions {
                PlainAction("Add", enabled = helperId.isNotBlank() && action.isNotBlank()) {
                    problem = app.editor.addHelper(machine.id, helperId, action)
                    if (problem == null) {
                        helperId = ""
                        action = ""
                    }
                }
            }
            problem?.let { QuietText(it) }
            if (candidates.isEmpty()) {
                QuietText("There is no other machine in this setup to ask.")
            }
        }
    }
}

@Composable
private fun AddRow(
    label: String,
    idPlaceholder: String,
    namePlaceholder: String,
    onAdd: (String, String) -> String?,
) {
    var id by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("") }
    var problem by remember { mutableStateOf<String?>(null) }

    DetailRow(label, alignment = Alignment.Top) {
        Column {
            PlainField(id, { id = it }, idPlaceholder)
            PlainField(name, { name = it }, namePlaceholder)
            Actions {
                PlainAction("Add", enabled = id.isNotBlank()) {
                    problem = onAdd(id, name)
                    if (problem == null) {
                        id = ""
                        name = ""
                    }
                }
            }
            problem?.let { QuietText(it) }
        }
    }
}

/**
 * Two copies of the same setup that neither descends from the other, and what to do about it.
 *
 * This is the screen the whole lineage arrangement exists to produce instead of a silent overwrite.
 * Somebody edited on two devices while one of them was away, and both edits are real: the default is
 * to keep both, taking each side's own changes, and asking only about the entries both sides moved.
 */
@Composable
fun DivergenceSection(app: AppModel) {
    val divergence = app.divergence ?: return
    val differences = app.divergenceDifferences()

    Spacer(Modifier.height(6.dp))
    StatusLine(Mark.Attention, "The setup was edited in two places")
    Spacer(Modifier.height(8.dp))
    ExplanationText(divergence.reason)

    Spacer(Modifier.height(10.dp))
    DetailRow("Here") {
        ValueText(
            listOfNotNull(
                divergence.mineProvenance?.revision?.let { "revision $it" },
                divergence.mineProvenance?.describeAuthor(),
            ).joinToString(", ").takeIf { it.isNotBlank() },
            "this device's copy",
        )
    }
    DetailRow("On ${divergence.machineName}") {
        ValueText(
            listOfNotNull(
                divergence.theirProvenance.revision?.let { "revision $it" },
                divergence.theirProvenance.describeAuthor(),
            ).joinToString(", ").takeIf { it.isNotBlank() },
            "the other copy",
        )
    }
    QuietText(
        if (divergence.baseHash != null) {
            "This device still has the revision both were made from, so only the entries both " +
                "sides changed need an answer."
        } else {
            "This device no longer has the revision both were made from, so every difference needs " +
                "an answer."
        },
        Modifier.padding(top = 4.dp),
    )

    SectionHeading("What differs", note = "${differences.size}")
    differences.forEach { difference ->
        val choice = app.mergeChoices[difference.path]
        Column(Modifier.padding(vertical = 4.dp)) {
            Text(difference.label, style = MaterialTheme.typography.bodyMedium)
            QuietText(difference.summary, Modifier.padding(top = 2.dp))
            Actions {
                PlainAction(
                    label = "Keep this device's",
                    enabled = choice != SetupMerge.Choice.MINE,
                    emphasis = choice == SetupMerge.Choice.MINE,
                ) { app.chooseMerge(difference.path, SetupMerge.Choice.MINE) }
                PlainAction(
                    label = "Take ${divergence.machineName}'s",
                    enabled = choice != SetupMerge.Choice.THEIRS,
                    emphasis = choice == SetupMerge.Choice.THEIRS,
                ) { app.chooseMerge(difference.path, SetupMerge.Choice.THEIRS) }
            }
        }
    }

    SectionHeading("Settle it")
    Actions {
        PlainAction("Merge", enabled = true, emphasis = true) { app.applyMerge() }
        PlainAction("Keep this device's", enabled = true) { app.keepMine() }
        PlainAction("Take ${divergence.machineName}'s", enabled = true) { app.takeTheirs() }
    }
    QuietText(
        "Merging writes one revision made from both, so every machine takes it without anything " +
            "being overwritten.",
        Modifier.padding(top = 2.dp),
    )
    app.configError?.let { QuietText(it, Modifier.padding(top = 4.dp)) }
}

/**
 * A machine carrying a different setup entirely, which cannot be merged.
 *
 * Two setups have nothing to say to each other: their revisions are unrelated and their machine
 * lists may describe different houses. So this is the one question with no middle answer, and the
 * only place in the app where `--replace` is ever sent.
 */
@Composable
fun IdentityClashSection(app: AppModel) {
    val clash = app.identityClash ?: return
    Spacer(Modifier.height(6.dp))
    StatusLine(Mark.Attention, "${clash.machineName} belongs to a different setup")
    Spacer(Modifier.height(8.dp))
    ExplanationText(clash.reason)

    val preview = app.clashPreview()
    if (preview != null) {
        SectionHeading("Taking theirs would")
        preview.sentences().forEach { QuietText(it, Modifier.padding(top = 2.dp)) }
    }

    SectionHeading("Settle it")
    Actions {
        PlainAction("Replace ${clash.machineName}'s copy", enabled = true, destructive = true) {
            app.replaceTheirSetup()
        }
        PlainAction("Follow their setup instead", enabled = true) { app.adoptTheirSetup() }
        PlainAction("Leave it", enabled = true) { app.dismissIdentityClash() }
    }
    app.configError?.let { QuietText(it, Modifier.padding(top = 4.dp)) }
}
