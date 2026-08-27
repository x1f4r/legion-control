package com.x1f4r.legioncontrol.agent

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

// Every field below is nullable with a null default, on purpose and without exception. The agent on
// the far side updates on its own schedule, so a field this app has never heard of must cost one
// blank row and a field that disappears must cost the same. Nothing here may fail a whole decode.

/** Which system answered. Absent on an agent older than the idea of systems; see [AgentStatus]. */
@Serializable
data class SystemInfo(
    val id: String? = null,
    val name: String? = null,
)

/**
 * The first version of the agent reported exactly one service and called it t3. It is read here
 * only to build a service out of, and nothing else in this app looks at it.
 */
@Serializable
data class T3Status(
    val installed: String? = null,
    val nightly: String? = null,
    /** null means the registry lookup did not come back, which is not the same as "yes". */
    val upToDate: Boolean? = null,
    val serverRunning: Boolean? = null,
    val healthy: Boolean? = null,
    val port: Int? = null,
)

@Serializable
data class BusyThread(
    val threadId: String? = null,
    val turnId: String? = null,
    val title: String? = null,
    val state: String? = null,
    val at: String? = null,
    val stale: Boolean? = null,
)

@Serializable
data class BusyStatus(
    val busy: Boolean? = null,
    val runningTurns: Int? = null,
    val pendingTurns: Int? = null,
    val pendingApprovals: Int? = null,
    val staleTurns: Int? = null,
    val staleApprovals: Int? = null,
    val reason: String? = null,
    val threads: List<BusyThread>? = null,
    val threadsTruncated: Int? = null,
    /** The agent could not read the probe, so "not busy" here means "cannot tell". */
    val unknown: Boolean? = null,
    val error: String? = null,
) {
    val isBusy: Boolean get() = busy == true
    val summary: String get() = reason?.takeIf { it.isNotBlank() } ?: if (isBusy) "working" else "idle"
}

@Serializable
data class LastUpdate(
    val at: String? = null,
    val from: String? = null,
    val to: String? = null,
    val result: String? = null,
    val message: String? = null,
)

/** A tunnel that has to be up for a service to be reachable from outside. */
@Serializable
data class RelayStatus(
    val configured: Boolean? = null,
    val running: Boolean? = null,
)

/**
 * One thing on a system that has a version, can be running, can be busy, and can be updated and
 * restarted. What every service page on the phone is drawn from.
 */
@Serializable
data class ServiceStatus(
    val id: String? = null,
    val name: String? = null,
    val kind: String? = null,
    val installed: String? = null,
    /** null when it could not be checked, which is not the same as "there is nothing newer". */
    val latest: String? = null,
    val channel: String? = null,
    val upToDate: Boolean? = null,
    val running: Boolean? = null,
    val healthy: Boolean? = null,
    val port: Int? = null,
    val staged: String? = null,
    val appPath: String? = null,
    val busy: BusyStatus? = null,
    val relay: RelayStatus? = null,
    val pendingRestart: Boolean? = null,
    val lastUpdate: LastUpdate? = null,
    val canUpdate: Boolean? = null,
    val canRestart: Boolean? = null,
) {
    /** What the screen calls it. The id is the contract; the name is only ever for reading. */
    val displayName: String get() = name?.takeIf { it.isNotBlank() } ?: id.orEmpty().ifBlank { "Service" }
}

/** A system this one can arm the next boot for. */
@Serializable
data class BootTarget(
    val id: String? = null,
    val name: String? = null,
)

/** A named command with a button. No version, no health, no busy state of its own. */
@Serializable
data class AgentAction(
    val id: String? = null,
    val name: String? = null,
    /** Shown before running. Absent for actions that need no confirmation. */
    val confirm: String? = null,
    val busyGated: Boolean? = null,
) {
    val displayName: String get() = name?.takeIf { it.isNotBlank() } ?: id.orEmpty().ifBlank { "Run" }
}

@Serializable
data class AgentStatus(
    val ok: Boolean? = null,
    @SerialName("os") val osName: String? = null,
    val system: SystemInfo? = null,
    val hostname: String? = null,
    val agentVersion: String? = null,
    val services: List<ServiceStatus>? = null,
    val bootTargets: List<BootTarget>? = null,
    val actions: List<AgentAction>? = null,
    val busy: BusyStatus? = null,
    val autoUpdate: Boolean? = null,
    val notes: List<String>? = null,
    /** Set when the agent is reporting its own failure rather than a machine state. */
    val message: String? = null,
    val error: String? = null,

    // The first version of the agent, kept readable for exactly as long as a machine somewhere is
    // still running it. Nothing outside this file reads these four.
    val t3: T3Status? = null,
    val pendingRestart: Boolean? = null,
    val lastUpdate: LastUpdate? = null,
    val connect: RelayStatus? = null,
) {
    /**
     * The id of the system that answered.
     *
     * An agent that predates the idea of a system says nothing here, and the platform name it does
     * report is what that system's id would have defaulted to, so it is the right fallback rather
     * than a guess. Anything that has to match this against the configuration does so by id first.
     */
    val systemId: String? get() = system?.id?.takeIf { it.isNotBlank() } ?: osName?.takeIf { it.isNotBlank() }

    /** What the agent called it, when it called it anything. */
    val systemName: String? get() = system?.name?.takeIf { it.isNotBlank() }

    /** Whether this reply came from an agent that knows what a service is. Decides `--service`. */
    val reportsServices: Boolean get() = services != null

    val isBusy: Boolean get() = busy?.isBusy == true

    /**
     * The services on this system, whatever the agent's vintage.
     *
     * An agent that predates `services` described exactly one service and spread it across the top
     * level of the reply: the versions under `t3`, the busy state, the relay, the pending restart
     * and the last update each in their own key. Folding that back into one service here is what
     * lets everything above this file be written once, against the shape the contract now has.
     */
    val effectiveServices: List<ServiceStatus>
        get() {
            services?.takeIf { it.isNotEmpty() }?.let { return it }
            if (services != null) return emptyList()
            val t3 = t3 ?: return emptyList()
            return listOf(
                ServiceStatus(
                    id = "t3",
                    name = "T3 Code",
                    kind = "npm",
                    installed = t3.installed,
                    latest = t3.nightly,
                    channel = "nightly",
                    upToDate = t3.upToDate,
                    running = t3.serverRunning,
                    healthy = t3.healthy,
                    port = t3.port,
                    busy = busy,
                    relay = connect,
                    pendingRestart = pendingRestart,
                    lastUpdate = lastUpdate,
                ),
            )
        }
}

/** The shape shared by update, restart, auto-update, boot, sleep and run. */
@Serializable
data class AgentActionResult(
    val ok: Boolean? = null,
    /**
     * One of: updated, current, noop, deferred, failed, rolled-back, restarted, armed, rebooting,
     * sleeping, ran.
     */
    val action: String? = null,
    val target: String? = null,
    val service: String? = null,
    val from: String? = null,
    val to: String? = null,
    val message: String? = null,
    val autoUpdate: Boolean? = null,
    val exitCode: Int? = null,
    val error: String? = null,
)

@Serializable
data class AgentVersion(
    val ok: Boolean? = null,
    val agentVersion: String? = null,
    val error: String? = null,
)

/** Lenient on the way in, because being strict here only ever turns a readable status into nothing. */
internal val AgentJson: Json = Json {
    ignoreUnknownKeys = true
    isLenient = true
    explicitNulls = false
}
