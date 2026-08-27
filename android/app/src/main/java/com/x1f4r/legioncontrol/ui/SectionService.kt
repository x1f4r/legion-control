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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.agent.BusyStatus
import com.x1f4r.legioncontrol.agent.LastUpdate
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.RelayStatus
import com.x1f4r.legioncontrol.agent.ServiceStatus

/**
 * What one service is: which version, whether it is answering, what it is doing right now, and the
 * two things that can be done to it. The automatic update switches live here too, because they are
 * the same subject and there is no reason for them to be a page of their own.
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
                            thread.state,
                            if (thread.stale == true) "stale" else null,
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
                    if (more > 0) {
                        QuietText("and $more more", Modifier.padding(top = 2.dp))
                    }
                }
            }
        }

        service.lastUpdate?.let { last ->
            DetailRow("Last update", alignment = Alignment.Top) { LastUpdateText(last) }
        }
    }

    SectionHeading("Actions", note = model.currentSystem?.let { "on ${it.name}" })
    Actions {
        // Live and emphasised only when the agent has actually compared what is installed against
        // what is published and found it behind. A lookup that failed leaves this dead, because an
        // update pressed on the strength of an unknown stops the service to install nothing.
        val canUpdate = service != null && updateAvailable(model, service)
        PlainAction(
            label = "Update now",
            enabled = !model.isWorking && canUpdate,
            emphasis = canUpdate,
            working = model.isBusyWith(MachineModel.Task.UPDATE, page.id),
        ) { service?.let(model::requestUpdate) }

        PlainAction(
            label = "Restart ${page.name}",
            enabled = !model.isWorking && service != null && model.currentSystem != null &&
                service.canRestart != false,
            destructive = true,
            working = model.isBusyWith(MachineModel.Task.RESTART, page.id),
        ) { service?.let(model::requestRestart) }
    }
    updateUnavailableReason(model, service)?.let { QuietText(it, Modifier.padding(top = 2.dp)) }

    SectionHeading("Update automatically")
    ExplanationText(
        "Each system keeps its own setting, for everything it looks after. Only the system that is " +
            "awake can be changed.",
    )
    Spacer(Modifier.height(4.dp))
    model.machine.systems.forEach { system -> AutoUpdateRow(model, system) }
}

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
        service.canUpdate == false -> "The agent has no way to update this service, only to restart it."
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
        // The agent sets this when it found a newer build and held the install back because the
        // service was busy. Nothing has been installed yet, so this must not claim that it has.
        service.pendingRestart == true ->
            StatusLine(Mark.Attention, "A newer version is queued for the next idle window")

        else -> StatusLine(Mark.Attention, "A newer version is available")
    }
}

@Composable
private fun RunningVerdict(service: ServiceStatus) {
    val port = service.port
    when {
        service.healthy == true ->
            StatusLine(Mark.Good, port?.let { "Healthy on port $it" } ?: "Healthy")

        service.running == true ->
            StatusLine(Mark.Attention, port?.let { "Running but not answering on port $it" }
                ?: "Running but not answering")

        else -> StatusLine(Mark.Bad, "Stopped")
    }
}

@Composable
private fun ActivityVerdict(busy: BusyStatus?) {
    when {
        busy == null -> ValueText(null, "not known")

        // The agent could not read the probe. That is not the same as nothing running, and drawing
        // it as "Idle" would be the reading that gets somebody's work killed.
        busy.unknown == true ->
            StatusLine(Mark.Unknown, "Cannot tell, the agent could not read what it is doing")

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

/**
 * Two lines rather than one. The outcome and a readable local clock time on top, the version
 * transition underneath, because a single joined string has nowhere sensible to break on a phone
 * and wraps in the middle of a version number.
 */
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
private fun AutoUpdateRow(model: MachineModel, system: MachineSystem) {
    val haptics = rememberHaptics()
    val isLive = model.currentSystem?.id == system.id
    val value = model.autoUpdateValue(system)
    val enabled = isLive && !model.isWorking

    fun toggle(to: Boolean) {
        haptics.tick()
        model.setAutoUpdate(to, system)
    }

    val row = Modifier
        .fillMaxWidth()
        .defaultMinSize(minHeight = 60.dp)
        .let { base ->
            if (enabled) base.clickable { toggle(!(value ?: false)) } else base
        }
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
        Switch(
            checked = value == true,
            onCheckedChange = { toggle(it) },
            enabled = enabled,
        )
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
            "Updates install on their own."
        } else {
            "Updates only happen when you ask."
        }
    }
    val checked = model.remembered[system.id]?.checkedAt
    if (value == null || checked == null) {
        return "Not known yet. Boot ${system.name} once to read it."
    }
    return "Last known: ${if (value) "on" else "off"} (read on ${localTime(checked)})"
}
