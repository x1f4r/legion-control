package com.x1f4r.legioncontrol.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.BusyStatus
import com.x1f4r.legioncontrol.agent.LastUpdate
import com.x1f4r.legioncontrol.agent.MaintenanceWindow
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.ReasonCode
import com.x1f4r.legioncontrol.agent.PolicyPatch
import com.x1f4r.legioncontrol.agent.RelayStatus
import com.x1f4r.legioncontrol.agent.ServiceStatus

/**
 * What one service is: which version, whether it is answering, what it is doing right now, when the
 * schedule may touch it, and the things that can be done to it.
 *
 * "Automatic" used to be one boolean that also decided whether the Update button worked. It is now
 * the scheduler's permission and nothing else, sitting beside a pause and a window, while a manual
 * update ignores all three.
 */
@Composable
fun ServiceSection(model: MachineModel, page: RememberedService) {
    val service = model.service(page.id)

    Spacer(Modifier.height(6.dp))
    if (service == null) {
        ExplanationText("Not available while ${model.machine.name} is unreachable.")
    } else {
        DetailRow("Installed") { VersionText(service.installed, "not installed") }
        DetailRow(latestLabel(service)) { VersionText(service.latest, "could not be checked") }
        DetailRow("Version") { VersionVerdict(service) }
        DetailRow("Service") { RunningVerdict(service) }
        DetailRow("Doing now") { ActivityVerdict(service.busy) }
        DetailRow("Relay") { RelayVerdict(service.relay) }

        val threads = service.busy?.threads.orEmpty()
        if (threads.isNotEmpty()) {
            ValueIndent {
                Column(Modifier.padding(top = 2.dp, bottom = 4.dp)) {
                    threads.forEach { thread ->
                        val state = listOfNotNull(
                            thread.disposition ?: thread.state,
                            if (thread.stale == true) "stale" else null,
                            if (thread.blocking == false) "not blocking" else null,
                        ).joinToString(", ")
                        Row(Modifier.padding(vertical = 3.dp)) {
                            Text(
                                text = thread.title ?: "untitled",
                                style = MaterialTheme.typography.bodySmall,
                                modifier = Modifier.weight(1f, fill = false),
                            )
                            if (state.isNotEmpty()) {
                                Spacer(Modifier.width(8.dp))
                                QuietText(state)
                            }
                        }
                    }
                    val more = service.busy?.threadsTruncated ?: 0
                    if (more > 0) QuietText("and $more more", Modifier.padding(top = 2.dp))
                }
            }
        }

        service.lastUpdate?.let { last ->
            DetailRow("Last update", alignment = Alignment.Top) { LastUpdateText(last) }
        }

        service.notes.orEmpty().forEach { QuietText(it, Modifier.padding(top = 2.dp)) }
    }

    SectionHeading("Actions", note = model.currentSystem?.let { "on ${it.name}" })
    Actions {
        // Live and emphasised when the agent has compared what is installed against what is
        // published and found it behind. A lookup that failed leaves this dead, because an update
        // pressed on the strength of an unknown stops the service to install nothing.
        val canUpdate = service != null && updateAvailable(model, service)
        PlainAction(
            label = "Update now",
            enabled = !model.isWorking && canUpdate,
            emphasis = canUpdate,
            working = model.isBusyWith(MachineModel.Task.UPDATE, page.id),
        ) { service?.let(model::requestUpdate) }

        if (model.speaksV3) {
            PlainAction(
                label = "Update when idle",
                enabled = !model.isWorking && canUpdate,
            ) { service?.let { model.update(it, whenIdle = true) } }
        }

        PlainAction(
            label = "Restart ${page.name}",
            enabled = !model.isWorking && service != null && model.currentSystem != null &&
                service.canRestart != false,
            destructive = true,
            working = model.isBusyWith(MachineModel.Task.RESTART, page.id),
        ) { service?.let(model::requestRestart) }

        if (model.speaksV3) {
            PlainAction(
                label = "Restart when idle",
                enabled = !model.isWorking && service != null && service.canRestart != false,
                destructive = true,
            ) { service?.let { model.restart(it, whenIdle = true) } }
        }
    }
    updateUnavailableReason(model, service)?.let { QuietText(it, Modifier.padding(top = 2.dp)) }

    MaintenanceBlock(model, page, service)
}

/**
 * When the schedule may touch this service, which is three separate things.
 *
 * Whether the scheduler is allowed to act at all, whether it has been told to leave this alone for
 * a while, and the hours it may work in. A manual update ignores every one of them, which is why
 * "Update now" above is live even when everything here is off.
 */
@Composable
private fun MaintenanceBlock(
    model: MachineModel,
    page: RememberedService,
    service: ServiceStatus?,
) {
    if (service?.canUpdate == false) return
    SectionHeading("Maintenance")

    if (!model.speaksV3) {
        ExplanationText(
            "This machine's control agent keeps one switch for the whole system rather than a " +
                "policy per service. Asking for an update by hand works either way.",
        )
        Spacer(Modifier.height(4.dp))
        model.machine.systems.forEach { system -> AutoUpdateRow(model, system, null) }
        return
    }

    val updates = service?.updates
    val system = model.status?.updates

    ExplanationText(
        "These decide when this machine updates on its own. Asking for an update yourself ignores " +
            "all of them and still waits for the machine to be idle.",
    )
    Spacer(Modifier.height(4.dp))

    DetailRow("Scheduled") {
        when {
            updates == null -> ValueText(null, "not known")
            updates.automatic == true && updates.inherited == true ->
                StatusLine(Mark.Good, "On, from the machine's own setting")

            updates.automatic == true -> StatusLine(Mark.Good, "On for this service")
            updates.inherited == true -> StatusLine(Mark.Idle, "Off, from the machine's own setting")
            else -> StatusLine(Mark.Idle, "Off for this service")
        }
    }

    DetailRow("Paused") {
        val until = updates?.pauseUntil ?: system?.pauseUntil
        if (until.isNullOrBlank()) {
            ValueText(null, "not paused")
        } else {
            StatusLine(Mark.Attention, "Until ${localTime(until) ?: until}")
        }
    }

    DetailRow("Windows", alignment = Alignment.Top) {
        val windows = updates?.maintenanceWindows ?: system?.maintenanceWindows
        if (windows.isNullOrEmpty()) {
            ValueText(null, "any time")
        } else {
            Column { windows.forEach { Text(it.summary, style = MaterialTheme.typography.bodyMedium) } }
        }
    }

    DetailRow("Eligible now") {
        when {
            updates?.eligibleNow == true -> StatusLine(Mark.Good, "Yes")
            updates?.deferredReason != null ->
                StatusLine(Mark.Idle, ReasonCode.describe(updates.deferredReason)!!.replaceFirstChar(Char::uppercase))

            updates?.eligibleNow == false -> StatusLine(Mark.Idle, "No")
            else -> ValueText(null, "not known")
        }
    }

    system?.nextWindow?.let {
        QuietText("Next window ${localTime(it) ?: it}.", Modifier.padding(top = 2.dp))
    }

    Actions {
        val paused = !(updates?.pauseUntil ?: system?.pauseUntil).isNullOrBlank()
        if (paused) {
            PlainAction(
                label = "Resume",
                enabled = !model.isWorking,
                working = model.isBusyWith(MachineModel.Task.POLICY, page.id),
            ) { model.resumeUpdates(page.id) }
        } else {
            PlainAction("Pause for 4 hours", enabled = !model.isWorking) {
                model.pauseUpdates("4h", page.id)
            }
            PlainAction("Pause for 2 days", enabled = !model.isWorking) {
                model.pauseUpdates("2d", page.id)
            }
        }
    }

    WindowEditor(model, page, updates?.maintenanceWindows ?: system?.maintenanceWindows.orEmpty())

    Spacer(Modifier.height(4.dp))
    model.machine.systems.forEach { system -> AutoUpdateRow(model, system, page.id) }
}

@Composable
private fun WindowEditor(
    model: MachineModel,
    page: RememberedService,
    current: List<MaintenanceWindow>,
) {
    var days by remember(page.id) { mutableStateOf("mon tue wed thu fri") }
    var from by remember(page.id) { mutableStateOf("02:00") }
    var to by remember(page.id) { mutableStateOf("06:00") }
    var problem by remember(page.id) { mutableStateOf<String?>(null) }

    SectionHeading("Edit maintenance windows")
    ExplanationText(
        "Times are local to the machine. A window whose end is earlier than its start continues " +
            "overnight. Use three-letter days separated by spaces.",
    )
    current.forEachIndexed { index, window ->
        DetailRow("Window ${index + 1}") {
            Text(window.summary, style = MaterialTheme.typography.bodyMedium)
            PlainAction("Remove", enabled = !model.isWorking, destructive = true) {
                model.writePolicy(
                    PolicyPatch(maintenanceWindows = current.filterIndexed { at, _ -> at != index }),
                    page.id,
                )
            }
        }
    }
    DetailRow("Days") {
        PlainField(days, { days = it }, "mon tue wed thu fri")
    }
    DetailRow("From") {
        PlainField(from, { from = it }, "02:00")
    }
    DetailRow("To") {
        PlainField(to, { to = it }, "06:00")
    }
    Actions {
        PlainAction("Add window", enabled = !model.isWorking && current.size < 14) {
            val parsedDays = days.lowercase().split(Regex("[\\s,]+"))
                .filter { it.isNotBlank() }.distinct()
            problem = when {
                parsedDays.isEmpty() || parsedDays.any { it !in WEEKDAYS } ->
                    "Days have to use mon, tue, wed, thu, fri, sat or sun."

                !CLOCK.matches(from) || !CLOCK.matches(to) ->
                    "Times have to use 24-hour HH:MM, from 00:00 through 23:59."

                else -> null
            }
            if (problem == null) {
                model.writePolicy(
                    PolicyPatch(maintenanceWindows = current + MaintenanceWindow(parsedDays, from, to)),
                    page.id,
                )
            }
        }
        PlainAction("Allow any time", enabled = !model.isWorking && current.isNotEmpty()) {
            model.writePolicy(PolicyPatch(maintenanceWindows = emptyList()), page.id)
        }
        PlainAction("Inherit machine policy", enabled = !model.isWorking) {
            model.writePolicy(PolicyPatch.inheritAll(), page.id)
        }
    }
    problem?.let { StatusLine(Mark.Bad, it) }
}

private val WEEKDAYS = setOf("mon", "tue", "wed", "thu", "fri", "sat", "sun")
private val CLOCK = Regex("^(?:[01]\\d|2[0-3]):[0-5]\\d$")

/** "Latest nightly" when the agent named a channel, and plain "Latest" when it did not. */
private fun latestLabel(service: ServiceStatus): String =
    service.channel?.takeIf { it.isNotBlank() }?.let { "Latest $it" } ?: "Latest"

/**
 * Whether there is actually something newer to install.
 *
 * Every part of this has to be true at once, and `upToDate == null` is the case that matters: the
 * agent could not reach whatever publishes the version, so it does not know. An update offered on
 * the strength of a lookup that failed is an update that stops the service to install nothing.
 */
private fun updateAvailable(model: MachineModel, service: ServiceStatus): Boolean =
    model.currentSystem != null &&
        service.canUpdate != false &&
        service.installed != null &&
        service.latest != null &&
        service.upToDate == false

/** Why updating is not on offer, in one line. Null when it is. */
private fun updateUnavailableReason(model: MachineModel, service: ServiceStatus?): String? {
    if (service != null && updateAvailable(model, service)) return null
    return when {
        model.currentSystem == null -> "${model.machine.name} has to be awake before this can be updated."
        service == null -> "The agent said nothing about this service."
        service.canUpdate == false -> "Updates are not managed by this agent."
        service.installed == null -> "It is not installed on this system."
        service.latest == null || service.upToDate == null ->
            "The newest version could not be looked up, so there is nothing to compare against."

        else -> "It is already on the newest version."
    }
}

@Composable
private fun VersionVerdict(service: ServiceStatus) {
    when {
        service.installed == null -> StatusLine(Mark.Unknown, "Not installed here")
        service.latest == null -> StatusLine(Mark.Attention, "The newest version could not be read")
        service.upToDate == true -> StatusLine(Mark.Good, "Up to date")
        service.pendingVersion != null ->
            StatusLine(Mark.Attention, "${service.pendingVersion} is waiting for an idle moment")

        service.pendingRestart == true ->
            StatusLine(Mark.Attention, "A newer version is queued for the next idle window")

        else -> StatusLine(Mark.Attention, "A newer version is available")
    }
}

/**
 * Whether the service is running, and whether it answers, which are two questions.
 *
 * The v3 status separates them: a process that exists and a port that replies are different facts
 * and used to be one line. Where the agent only sends the old pair, the old wording is used.
 */
@Composable
private fun RunningVerdict(service: ServiceStatus) {
    val port = service.port
    val process = service.process
    val health = service.health
    when {
        process != null && health != null -> when {
            process.running != true -> StatusLine(Mark.Bad, "Stopped")
            health.ok == true -> StatusLine(Mark.Good, port?.let { "Healthy on port $it" } ?: "Healthy")
            health.error != null -> StatusLine(Mark.Attention, "Running, and ${health.error}")
            else -> StatusLine(
                Mark.Attention,
                port?.let { "Running but not answering on port $it" } ?: "Running but not answering",
            )
        }

        service.healthy == true ->
            StatusLine(Mark.Good, port?.let { "Healthy on port $it" } ?: "Healthy")

        service.running == true -> StatusLine(
            Mark.Attention,
            port?.let { "Running but not answering on port $it" } ?: "Running but not answering",
        )

        else -> StatusLine(Mark.Bad, "Stopped")
    }
}

/**
 * What the service is doing, and whether anything is actually watching.
 *
 * Three answers rather than two. "Not monitored" is the one that would otherwise read as "idle",
 * and that reading is what gets somebody's work killed: nothing looked, so nothing was found.
 */
@Composable
private fun ActivityVerdict(busy: BusyStatus?) {
    when {
        busy == null -> ValueText(null, "not known")

        busy.isUnknown -> StatusLine(
            Mark.Unknown,
            "Cannot tell: " + (busy.error ?: "the agent could not read the probe"),
        )

        busy.monitored == false -> StatusLine(
            Mark.Unknown,
            "Nothing is watching this service, so nobody can say whether it is working",
        )

        busy.isBusy -> StatusLine(Mark.Busy, busy.summary)
        else -> StatusLine(Mark.Idle, "Idle")
    }
}

@Composable
private fun RelayVerdict(relay: RelayStatus?) {
    when {
        relay == null || relay.configured != true -> ValueText(null, "not configured here")
        relay.running == true -> StatusLine(Mark.Good, "Running")
        else -> StatusLine(Mark.Bad, "Configured but not running")
    }
}

@Composable
private fun LastUpdateText(last: LastUpdate) {
    Column {
        Row(verticalAlignment = Alignment.Bottom) {
            Text(
                text = last.result ?: "none recorded",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            localTime(last.at)?.let {
                Spacer(Modifier.width(8.dp))
                QuietText(it)
            }
        }
        val from = last.from
        val to = last.to
        if (from != null && to != null) {
            Text(
                text = "$from to $to",
                style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 2.dp),
            )
        } else {
            val message = last.message
            if (last.result == null && !message.isNullOrBlank()) {
                QuietText(message, Modifier.padding(top = 2.dp))
            }
        }
    }
}

@Composable
private fun AutoUpdateRow(model: MachineModel, system: MachineSystem, serviceId: String?) {
    val haptics = rememberHaptics()
    val isLive = model.currentSystem?.id == system.id
    val value = model.autoUpdateValue(system)
    val enabled = isLive && !model.isWorking

    fun toggle(to: Boolean) {
        haptics.tick()
        model.setAutoUpdate(to, system, serviceId)
    }

    val row = Modifier
        .fillMaxWidth()
        .defaultMinSize(minHeight = 60.dp)
        .let { base -> if (enabled) base.clickable { toggle(!(value ?: false)) } else base }
        .padding(vertical = 8.dp)

    Row(row, verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(
                text = system.name,
                style = MaterialTheme.typography.bodyLarge,
                color = if (isLive) {
                    MaterialTheme.colorScheme.onSurface
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
            QuietText(autoUpdateNote(model, system, isLive, value), Modifier.padding(top = 2.dp))
        }
        Spacer(Modifier.width(12.dp))
        Switch(checked = value == true, onCheckedChange = { toggle(it) }, enabled = enabled)
    }
}

private fun autoUpdateNote(
    model: MachineModel,
    system: MachineSystem,
    isLive: Boolean,
    value: Boolean?,
): String {
    if (isLive) {
        return if (value == true) {
            "The scheduler may install updates on its own."
        } else {
            "The scheduler leaves this alone. Asking for an update by hand still works."
        }
    }
    val checked = model.remembered[system.id]?.checkedAt
    if (value == null || checked == null) {
        return "Not known yet. Boot ${system.name} once to read it."
    }
    return "Last known: ${if (value) "on" else "off"} (read on ${localTime(checked)})"
}
