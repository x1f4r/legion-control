package com.x1f4r.legioncontrol.ui

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.agent.humanDuration
import com.x1f4r.legioncontrol.ui.theme.LocalStatusColors

/**
 * The machine itself: which system is up, what it is called, what is running on it, and the things
 * that can be done to it. This is the landing page because it answers the question the app is opened
 * to answer, and nothing on it competes with that answer.
 */
@Composable
fun MachineSection(model: MachineModel, app: AppModel) {
    var sheet by remember(model.machine.id) { mutableStateOf<String?>(null) }
    var servicePage by remember(model.machine.id) { mutableStateOf<RememberedService?>(null) }
    var agentDetails by remember { mutableStateOf(false) }

    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(model.machine.name, style = MaterialTheme.typography.titleLarge)
            Text(compactMachineState(model), style = MaterialTheme.typography.bodyMedium,
                color = if (model.link is LinkState.Online) MaterialTheme.colorScheme.onSurfaceVariant else MaterialTheme.colorScheme.error)
        }
        MoreMenu("Device actions") { close ->
            MenuAction("Device details", close = close) { sheet = "Device details" }
            MenuAction("Control agent", close = close) { agentDetails = true }
            MenuAction("Activity", close = close) { sheet = "Activity" }
            MenuAction("Diagnostics", close = close) { sheet = "Diagnostics" }
            MenuAction("Service setup", model.speaksV3 && !model.isWorking, close) {
                if (model.serviceSetupUnavailable == null) model.serviceSetup.open() else agentDetails = true
            }
        }
    }
    Actions {
        if (model.canWake && !model.isAwake) PlainAction("Wake", !model.isWorking, working = model.isBusyWith(MachineModel.Task.WAKE)) { model.wake() }
        PlainAction("Sleep", !model.isWorking && model.currentSystem != null, working = model.isBusyWith(MachineModel.Task.SLEEP)) { model.requestSleep() }
        if (model.bootChoices.isNotEmpty() || (model.canWake && !model.isAwake)) PlainAction("Power options", !model.isWorking) { sheet = "Power options" }
    }
    if (model.needsKeyAuthorisation) PlainAction("Authorize device", true, emphasis = true) { sheet = "Authorize this device" }
    if (model.hostKeyChanged != null) Actions {
        if (model.hostKeyChanged?.canApprove == true) PlainAction("Check host key", !model.isWorking, emphasis = true) { model.requestTrustHostKey() }
        PlainAction("Manage host identities", !model.isWorking) { model.manageHostIdentities(model.hostKeyChanged!!.address) }
    }
    if (model.status?.configBroken == true) {
        Text("Agent configuration needs attention", color = MaterialTheme.colorScheme.error)
        PlainAction("Details", true) { sheet = "Device details" }
    }
    model.wakeableHelper?.let { PlainAction("Wake helper first", !model.isWorking) { model.requestWakeHelper() } }

    OperationsBlock(model, showHistory = false)
    SectionHeading("Services")
    if (model.knownServices.isEmpty()) {
        QuietText(if (model.isAwake) "No services configured" else "Services unavailable")
        PlainAction("Service setup", model.speaksV3 && !model.isWorking) {
            if (model.serviceSetupUnavailable == null) model.serviceSetup.open() else agentDetails = true
        }
    }
    model.knownServices.forEach { service -> CompactServiceRow(model, service) { servicePage = service } }

    if (model.actions.isNotEmpty()) {
        Spacer(Modifier.height(12.dp))
        SectionHeading("Actions", showsRule = false)
        Actions {
            model.actions.forEach { action ->
                PlainAction(action.displayName, !model.isWorking && model.currentSystem != null,
                    destructive = action.confirm?.isNotBlank() == true, working = model.isBusyWith(MachineModel.Task.RUN, action.id)) { model.requestRun(action) }
            }
        }
    }
    if (!model.status?.metrics.isNullOrEmpty()) {
        PlainAction("Telemetry", true) { sheet = "Telemetry" }
    }
    servicePage?.let { page -> DetailSheet(page.name, { servicePage = null }) { ServiceSection(model, page) } }
    sheet?.let { title -> DetailSheet(title, { sheet = null }) {
        when (title) {
            "Authorize this device" -> AuthorisationScreen(app.publicKey, model.machine.systems, model.statusDetail) { model.refreshNow() }
            "Activity" -> OperationsBlock(model)
            "Diagnostics" -> DiagnosticsBlock(model)
            "Telemetry" -> ConfiguredMetricsBlock(model.status?.metrics.orEmpty())
            "Power options" -> {
                model.bootChoices.forEach { target ->
                    SettingsEntry("Boot into ${target.name}") { sheet = null; model.requestBoot(target) }
                }
                if (!model.isAwake && model.canWake) model.machine.systems.forEach { target ->
                    SettingsEntry("Wake for ${target.name}") { sheet = null; model.wake(target) }
                }
                WakeExplanation(model, app)
            }
            else -> {
                DetailRow("Hostname") { ValueText(model.status?.hostname, "not known") }
                DetailRow("System") { ValueText(model.currentSystem?.name, "not known") }
                DetailRow("Agent") { VersionText(model.abilities.agentVersion, "not known") }
                DetailRow("Address") { ValueText(model.route?.label, "not known") }
                model.lastChecked?.let { checked -> DetailRow("Checked") { QuietText(localTime(checked)) } }
                DetailRow("Setup") { SetupRow(model, app) }
                model.setupNote?.let { QuietText(it) }
                model.status?.config?.problems.orEmpty().forEach { QuietText(listOfNotNull(it.path, it.message).joinToString(": ")) }
                model.status?.notes.orEmpty().forEach { QuietText(it) }
                WakeExplanation(model, app)
                if (model.actions.isNotEmpty()) {
                    SectionHeading("Action details")
                    model.actions.forEach { action ->
                        Text(action.displayName, style = MaterialTheme.typography.bodyMedium)
                        QuietText(if (action.isIdempotent) "Sends a wake packet. Repeating the packet is safe." else "Runs a configured command.")
                        if (action.busyGated == true) QuietText("Waits while work is running.")
                        action.confirm?.let { QuietText(it) }
                    }
                }
            }
        }
    } }
    if (agentDetails) AgentDetailsDialog(model, app) { agentDetails = false }
    ServiceSetupDialog(model.serviceSetup)
    model.trustSettingsSnapshot?.let { snapshot ->
        HostIdentitySettingsDialog(model.trustSettingsAddress.orEmpty(), snapshot,
            model.machine.systems.map { com.x1f4r.legioncontrol.data.HostIdentitySystem(it.id, it.name) },
            model::closeHostIdentities, model::saveHostIdentities)
    }
    model.operationDetails?.let { details -> DetailSheet("Change details", model::dismissOperationDetails) {
        androidx.compose.foundation.text.selection.SelectionContainer { Text(details, style = MaterialTheme.typography.bodySmall) }
    } }
}

private fun compactMachineState(model: MachineModel): String = when (val state = model.link) {
    is LinkState.Online -> listOfNotNull(state.system.name, compactActivityState(model.status?.busy)).joinToString(" · ")
    is LinkState.AgentMissing -> "Agent not installed"
    is LinkState.HostKeyChanged -> "Host key needs approval"
    is LinkState.NeedsKeyAuthorisation -> "SSH key needs authorization"
    is LinkState.Unsettled -> "Outcome unconfirmed"
    is LinkState.Misconfigured -> "Setup needs attention"
    is LinkState.Offline -> "Not reachable"
    LinkState.Unknown -> "Checking…"
}

/**
 * What the control agent on the far side is, and whether this app can talk to it properly.
 *
 * The contract number rather than only a version, because that is what actually decides whether
 * durable operations, policies and the setup work at all. A machine below it is not broken, it is
 * behind, and the install action is right there.
 */
@Composable
private fun AgentRow(model: MachineModel, onDetails: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        VersionText(model.abilities.agentVersion, "not known")
        Spacer(Modifier.width(8.dp))
        PlainAction("Details", enabled = true, onClick = onDetails)
    }
}

@Composable
private fun AgentDetailsDialog(model: MachineModel, app: AppModel, onDismiss: () -> Unit) {
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Control agent") },
        text = {
            Column(Modifier.verticalScroll(androidx.compose.foundation.rememberScrollState())) {
                DetailRow("Version") { VersionText(model.abilities.agentVersion, "not known") }
                if (model.abilities.contract > 0) QuietText("Protocol contract ${model.abilities.contract}.")
                if (!model.speaksV3 && model.abilities.contract > 0) {
                    ExplanationText("Agent ${Contract.REQUIRED} is required for durable changes, policies and shared setup editing.")
                }
                if (model.status?.agent?.restrictedSession == true) {
                    ExplanationText("This connection uses a restricted SSH key. Editing service setup requires an administrator SSH key.")
                }
                val version = app.agentBundleVersion
                Actions {
                    PlainAction(
                        label = version?.let { "Install agent $it" } ?: "Install agent",
                        enabled = version != null && !model.isWorking,
                        emphasis = version != null,
                    ) { if (version != null) { onDismiss(); model.requestAgentInstall(version) } }
                }
                if (version == null) QuietText(app.agentBundleProblem ?: "This build has no signed agent bundle to install.")
            }
        },
        confirmButton = { androidx.compose.material3.TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

/** Which setup copy this machine holds, and how it relates to the one on this device. */
@Composable
private fun SetupRow(model: MachineModel, app: AppModel) {
    val mark = model.status?.controller
    val mine = app.setupProvenance
    Column {
        when {
            mark?.hash == null -> ValueText(null, "not carrying one")
            mark.hash == mine?.hash -> StatusLine(Mark.Good, "In step with this device")
            else -> StatusLine(Mark.Attention, "A different copy")
        }
        mark?.let {
            val parts = listOfNotNull(
                it.revision?.let { revision -> "revision $revision" },
                com.x1f4r.legioncontrol.agent.ControllerSource.describe(it.source)
                    ?.let { source -> "written by $source" },
            )
            if (parts.isNotEmpty()) QuietText(parts.joinToString(", "), Modifier.padding(top = 2.dp))
        }
    }
}

/**
 * What this app has asked this machine to do, and what became of it.
 *
 * The section the whole durable-operation story is for. A change that is still running says which
 * phase it is in; one whose outcome was lost says so in those words rather than picking a verdict;
 * one that is queued shows when it gives up and can be cancelled.
 */
@Composable
internal fun OperationsBlock(model: MachineModel, showHistory: Boolean = true) {
    val pending = model.pending
    val history = model.history.filter { it.isOver }.take(5)
    val server = (model.agentOperations + model.recentAgentOperations).distinctBy { it.id }
        .filterNot { summary -> model.history.any { it.id == summary.id } }
    val activeServer = server.filter { it.state != "finished" }
    val finishedServer = server.filter { it.state == "finished" }
    var recentExpanded by remember(model.machine.id) { mutableStateOf(false) }

    if (pending.isNotEmpty() || activeServer.isNotEmpty()) {
        SectionHeading("Changes")
        pending.forEach { record -> OperationRow(model, record) }
        // Server-owned work keeps its canonical identity and remains visible until it finishes.
        activeServer.forEach { summary -> OperationRow(model, summary) }
    }

    if (showHistory && (history.isNotEmpty() || finishedServer.isNotEmpty())) {
        PlainAction(if (recentExpanded) "Recent changes ▾" else "Recent changes ▸", enabled = true) {
            recentExpanded = !recentExpanded
        }
        if (recentExpanded) {
            history.forEach { record ->
                val mark = when (record.succeeded) {
                    true -> Mark.Good
                    false -> Mark.Bad
                    null -> Mark.Unknown
                }
                StatusLine(mark, record.summary, style = MaterialTheme.typography.bodySmall)
                QuietText(localTime(record.updatedAt), Modifier.padding(start = 17.dp, bottom = 4.dp))
                PlainAction("Details", enabled = !model.isWorking) { model.showOperation(record.id) }
            }
            finishedServer.forEach { summary -> OperationRow(model, summary) }
        }
    }
}

@Composable
private fun OperationRow(model: MachineModel, summary: com.x1f4r.legioncontrol.agent.OpSummary) {
    val what = listOfNotNull(summary.opKind?.label ?: summary.kind, summary.subject).joinToString(" ")
    val outcome = summary.action ?: summary.state ?: "outcome unknown"
    val mark = when {
        summary.state == "queued" -> Mark.Idle
        summary.state == "running" -> Mark.Busy
        com.x1f4r.legioncontrol.agent.OpAction.fromWire(summary.action)?.isSuccess == true -> Mark.Good
        summary.action == "failed" -> Mark.Bad
        else -> Mark.Unknown
    }
    StatusLine(mark, "$what: $outcome")
    if (summary.state == "finished") localTime(summary.updatedAt ?: summary.requestedAt)?.let { time ->
        QuietText(time, Modifier.padding(start = 17.dp, bottom = 4.dp))
    }
    summary.phase?.takeIf { summary.state == "running" }?.let { QuietText("Phase: $it") }
    summary.expiresAt?.takeIf { summary.state == "queued" }?.let { QuietText("Expires: $it") }
    Actions {
        if (summary.state == "queued") PlainAction("Cancel", enabled = !model.isWorking) { model.cancelRemote(summary) }
        summary.id?.let { id -> PlainAction("Details", enabled = !model.isWorking) { model.showOperation(id) } }
    }
}

@Composable
private fun OperationRow(model: MachineModel, record: OperationRecord) {
    val mark = when (record.state) {
        TrackedState.OUTCOME_UNKNOWN -> Mark.Unknown
        TrackedState.QUEUED -> Mark.Idle
        TrackedState.AWAITING_DECISION -> Mark.Attention
        else -> Mark.Busy
    }
    Column(Modifier.padding(vertical = 2.dp)) {
        StatusLine(mark, record.summary)
        record.progressFraction?.let { fraction ->
            Spacer(Modifier.height(6.dp))
            LinearProgressIndicator(
                progress = { fraction },
                modifier = Modifier.fillMaxWidth().padding(start = 17.dp),
            )
        }
        Actions {
            PlainAction("Details", enabled = !model.isWorking) { model.showOperation(record.id) }
            if (record.isCancellable) {
                PlainAction("Cancel", enabled = true) { model.cancel(record) }
            }
            if (record.retryable && record.isOver) {
                PlainAction("Try again", enabled = !model.isWorking) { model.retry(record) }
            }
        }
    }
}

/**
 * Why waking is or is not on offer, from where this device is standing.
 *
 * The sentence is worked out from the site rules rather than written here, because whether a
 * broadcast can reach a machine depends on which network this phone is on and on whether that
 * question can even be answered: two houses on the same router defaults look identical from here.
 */
@Composable
private fun WakeExplanation(model: MachineModel, app: AppModel) {
    if (!model.canWake || model.isAwake) return
    val plan = com.x1f4r.legioncontrol.agent.planWake(
        target = model.machine,
        sites = app.sites,
        siteMatch = app.siteMatch,
        machines = app.machines.map { it.machine },
        localAddresses = emptyList(),
    )
    val sentence = when (plan) {
        is com.x1f4r.legioncontrol.agent.WakePlan.Direct -> plan.why
        is com.x1f4r.legioncontrol.agent.WakePlan.ViaHelpers -> plan.why
        is com.x1f4r.legioncontrol.agent.WakePlan.Impossible -> plan.reasons.joinToString(" ")
        com.x1f4r.legioncontrol.agent.WakePlan.NotConfigured -> null
    }
    sentence?.let { QuietText(it, Modifier.padding(top = 2.dp)) }

    model.wakeableHelper?.let { helper ->
        val name = app.machines.firstOrNull { it.machine.id == helper.machineId }?.machine?.name
            ?: helper.machineId
        Actions {
            PlainAction("Wake $name first", enabled = !model.isWorking) { model.requestWakeHelper() }
        }
    }
}

/**
 * Finding out what is actually wrong, on demand.
 *
 * Nothing here runs on a poll. Every check is one authenticated request per address, which is
 * exactly the traffic that gets a source penalised when it happens four times a minute in the
 * background and is entirely harmless when somebody presses a button.
 */
@Composable
private fun DiagnosticsBlock(model: MachineModel) {
    val context = androidx.compose.ui.platform.LocalContext.current
    model.bundleText?.let { bundle ->
        androidx.compose.material3.AlertDialog(
            onDismissRequest = model::clearBundle,
            title = { Text("Diagnostic bundle") },
            text = { androidx.compose.foundation.text.selection.SelectionContainer {
                Text(bundle, Modifier.verticalScroll(androidx.compose.foundation.rememberScrollState()), style = MaterialTheme.typography.bodySmall)
            } },
            confirmButton = { androidx.compose.material3.TextButton(onClick = {
                val file = java.io.File(java.io.File(context.cacheDir, "diagnostics").apply { mkdirs() }, "legion-control-diagnostics.json")
                file.writeText(bundle, Charsets.UTF_8)
                val uri = androidx.core.content.FileProvider.getUriForFile(context, com.x1f4r.legioncontrol.BuildConfig.APPLICATION_ID + ".updates", file)
                val send = android.content.Intent(android.content.Intent.ACTION_SEND).apply {
                    type = "application/json"
                    putExtra(android.content.Intent.EXTRA_STREAM, uri)
                    clipData = android.content.ClipData.newRawUri("Diagnostic bundle", uri)
                    addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION)
                }
                context.startActivity(android.content.Intent.createChooser(send, "Share diagnostics"))
            }) { Text("Share") } },
            dismissButton = { androidx.compose.material3.TextButton(onClick = model::clearBundle) { Text("Close") } },
        )
    }
    SectionHeading("Diagnostics")
    Actions {
        PlainAction(
            label = "Check addresses",
            enabled = !model.isWorking,
            working = model.diagnosing,
        ) { model.runDiagnostics() }

        PlainAction("Check agent", enabled = !model.isWorking && model.speaksV3) { model.runDoctor() }
        PlainAction("Deep diagnostics", enabled = !model.isWorking && model.speaksV3) { model.runDoctor(deep = true) }

        PlainAction("Recent log", enabled = !model.isWorking) { model.readLog() }

        PlainAction("Export diagnostics", enabled = !model.isWorking && model.speaksV3) {
            model.buildBundle()
        }
    }

    if (model.diagnostics.isNotEmpty() || model.doctorReport != null || model.recentLog.isNotEmpty()) {
        PlainAction("Hide results", enabled = !model.isWorking) { model.hideDiagnostics() }
    }

    model.diagnostics.forEach { diagnosis ->
        val mark = when {
            diagnosis.status.isGood -> Mark.Good
            diagnosis.status.needsDecision -> Mark.Attention
            else -> Mark.Bad
        }
        Column(Modifier.padding(top = 4.dp)) {
            StatusLine(mark, "${diagnosis.label} ${diagnosis.status.label}")
            QuietText(
                listOfNotNull(diagnosis.address, "${diagnosis.millis} ms", diagnosis.detail)
                    .joinToString(" · "),
                Modifier.padding(start = 17.dp, top = 2.dp),
            )
        }
    }

    model.doctorReport?.checks.orEmpty()
        .filterNot { it.isGood }
        .forEach { check ->
            Column(Modifier.padding(top = 4.dp)) {
                StatusLine(
                    if (check.isFailure) Mark.Bad else Mark.Attention,
                    check.summary ?: check.id.orEmpty(),
                )
                check.fix?.let { QuietText(it, Modifier.padding(start = 17.dp, top = 2.dp)) }
            }
        }

    if (model.recentLog.isNotEmpty()) {
        Spacer(Modifier.height(8.dp))
        Text(
            text = model.recentLog.takeLast(20).joinToString("\n"),
            style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/**
 * The one thing you open this app to find out, so it carries weight the other rows do not, and it
 * changes with a fade rather than a jump.
 */
@Composable
private fun RunningNow(model: MachineModel) {
    val palette = LocalStatusColors.current
    val reboot = model.rebootInProgress
    val link = model.link

    AnimatedContent(
        targetState = if (reboot != null && !model.isAwake) null else link,
        transitionSpec = { fadeIn(tween(200)) togetherWith fadeOut(tween(140)) },
        label = "running",
    ) { state ->
        Column {
            when {
                state == null ->
                    StatusLine(Mark.Busy, "Restarting into ${reboot?.name ?: "another system"}")

                state is LinkState.Online -> Row(verticalAlignment = Alignment.CenterVertically) {
                    PlatformGlyph(state.system.platform, 22.dp, palette.good)
                    Spacer(Modifier.width(11.dp))
                    Text(state.system.name, style = MaterialTheme.typography.titleLarge)
                }

                state is LinkState.AgentMissing ->
                    StatusLine(
                        Mark.Attention,
                        "${state.system.name}, but the control agent is not installed",
                    )

                state is LinkState.HostKeyChanged -> StatusLine(
                    Mark.Bad,
                    if (state.isFirstContact) {
                        "The host key for ${state.address} has not been checked yet"
                    } else {
                        "The host key for ${state.address} is not a trusted one"
                    },
                )

                // The distinction finding 14 was about: a command that overran or a reply that was
                // lost is not a machine that is asleep, and drawing it as one is how a reboot that
                // never happened gets reported as one that did.
                state is LinkState.Unsettled ->
                    StatusLine(Mark.Attention, "Answered, then stopped part way. Outcome not known")

                state is LinkState.Misconfigured ->
                    StatusLine(Mark.Bad, "This machine's setup cannot be used to reach it")

                state is LinkState.Offline -> StatusLine(Mark.Attention, "Asleep or unreachable")

                else -> StatusLine(Mark.Unknown, "Checking")
            }
        }
    }
}

/** Only metrics explicitly returned by configured probes exist here; no local samples are stored. */
@Composable
private fun ConfiguredMetricsBlock(metrics: List<com.x1f4r.legioncontrol.agent.ConfiguredMetric>) {
    if (metrics.isEmpty()) return
    var detail by remember { mutableStateOf<com.x1f4r.legioncontrol.agent.ConfiguredMetric?>(null) }
    SectionHeading("Telemetry")
    metrics.forEach { metric ->
        DetailRow(metric.name.ifBlank { metric.id }) {
            val number = metric.value?.takeIf { it.isFinite() }?.toString()?.removeSuffix(".0")
            Row(verticalAlignment = Alignment.CenterVertically) {
                ValueText(number?.let { "$it ${metric.unit}".trim() }, "Unavailable")
                if (!metric.error.isNullOrBlank()) {
                    Spacer(Modifier.width(8.dp))
                    PlainAction("Details", enabled = true) { detail = metric }
                }
            }
        }
    }
    detail?.let { metric ->
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { detail = null },
            title = { Text(metric.name.ifBlank { metric.id }) },
            text = { Text(listOfNotNull(metric.checkedAt?.let { "Checked: $it" }, metric.error).joinToString("\n\n")) },
            confirmButton = { androidx.compose.material3.TextButton(onClick = { detail = null }) { Text("Close") } },
        )
    }
}
