package com.x1f4r.legioncontrol.agent

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

// Every field below is nullable with a null default, on purpose and without exception, exactly as
// compatibility rule 3 requires. The agent on the far side updates on its own schedule, so a field
// this app has never heard of must cost one blank row and a field that disappears must cost the
// same. Nothing here may fail a whole decode.
//
// The names are the contract's. Where the contract has a closed enum this file still decodes a
// plain String and the enum lives in Contract.kt, because a value outside the enum has to survive
// the decode and be shown, not throw.

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
    val staged: String? = null,
    val appPath: String? = null,
)

@Serializable
data class BusyThread(
    val threadId: String? = null,
    val turnId: String? = null,
    val title: String? = null,
    val state: String? = null,
    val at: String? = null,
    /** v3: whether this thread is one of the reasons the gate is closed. */
    val blocking: Boolean? = null,
    /** v3: running | pending | pending-approval | stale. */
    val disposition: String? = null,
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

    /**
     * v3: whether anything is watching this service at all.
     *
     * A service with no busy configuration is not idle, it is
     * unwatched, and a disruptive change against it is a guess. False here is a warning, not a
     * verdict.
     */
    val monitored: Boolean? = null,

    /** v3: t3-sqlite | command | http | none | unmonitored | probe-error | timed-out. */
    val evidence: String? = null,
    val checkedAt: String? = null,
    val elapsedMs: Long? = null,
) {
    val isBusy: Boolean get() = busy == true

    /** True when the answer is "cannot tell", which blocks exactly as busy does. */
    val isUnknown: Boolean get() = unknown == true

    /** True only when something actually looked and found nothing running. */
    val isIdleForSure: Boolean get() = busy == false && unknown != true && monitored != false

    val summary: String
        get() = reason?.takeIf { it.isNotBlank() }
            ?: when {
                isUnknown -> "cannot tell"
                isBusy -> "working"
                monitored == false -> "not monitored"
                else -> "idle"
            }
}

/** The machine-wide busy answer. Unknown counts as blocking, here as everywhere. */
@Serializable
data class BusyAggregate(
    val busy: Boolean? = null,
    val unknown: Boolean? = null,
    val reason: String? = null,
    val monitoredServices: Int? = null,
    val unmonitoredServices: Int? = null,
)

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

/** v3: the service's process, separated from whether it answers on its port. */
@Serializable
data class ServiceProcess(
    val running: Boolean? = null,
    val state: String? = null,
    val startedAt: String? = null,
    val error: String? = null,
)

/** v3: whether the service answers, separated from whether its process exists. */
@Serializable
data class ServiceHealth(
    val ok: Boolean? = null,
    val status: Int? = null,
    val checkedAt: String? = null,
    val elapsedMs: Long? = null,
    val error: String? = null,
)

/** v3: reachability from outside the machine. Only `doctor --deep` fills [reachable] in. */
@Serializable
data class ServiceEndpoint(
    val configured: Boolean? = null,
    val reachable: Boolean? = null,
)

/** One window in local time on the machine. `from` later than `to` is an overnight window. */
@Serializable
data class MaintenanceWindow(
    /** mon tue wed thu fri sat sun. */
    val days: List<String> = emptyList(),
    val from: String = "",
    val to: String = "",
) {
    /** One line for the screen: "Mon–Fri, 02:00 to 06:00". */
    val summary: String
        get() {
            val when0 = if (days.isEmpty()) "any day" else days.joinToString(" ") { it.replaceFirstChar(Char::uppercase) }
            val overnight = if (from > to) " (overnight)" else ""
            return "$when0, $from to $to$overnight"
        }
}

/**
 * The machine-wide update policy, as `status` and `policy` report it.
 *
 * `automatic` is the scheduler's permission and nothing else: a manual request ignores all three of
 * these fields, and force overrides only the busy gate.
 */
@Serializable
data class SystemUpdates(
    val automatic: Boolean? = null,
    val pauseUntil: String? = null,
    val maintenanceWindows: List<MaintenanceWindow>? = null,
    val inWindowNow: Boolean? = null,
    val nextWindow: String? = null,
    val lastCycle: LastCycle? = null,
)

@Serializable
data class LastCycle(
    val opId: String? = null,
    val at: String? = null,
    val action: String? = null,
)

/**
 * One service's effective policy. [inherited] is true when the service sets none of its own.
 *
 * [deferredReason] is a reasonCode and is rendered through [ReasonCode], so "policy-off" reads as
 * "the schedule is off for this service" rather than as "already up to date".
 */
@Serializable
data class ServiceUpdates(
    val automatic: Boolean? = null,
    val inherited: Boolean? = null,
    val pauseUntil: String? = null,
    val maintenanceWindows: List<MaintenanceWindow>? = null,
    val eligibleNow: Boolean? = null,
    val deferredReason: String? = null,
    val inheritedKeys: InheritedPolicyKeys? = null,
)

@Serializable
data class InheritedPolicyKeys(
    val automatic: Boolean? = null,
    val pauseUntil: Boolean? = null,
    val maintenanceWindows: Boolean? = null,
)

/**
 * The patch sent to `policy set` on stdin.
 *
 * Absent means unchanged; an explicit null means inherit. [clear] records which null fields have to
 * be emitted, because a Kotlin nullable value alone cannot distinguish null from an omitted default.
 */
@Serializable
data class PolicyPatch(
    val automatic: Boolean? = null,
    val pauseUntil: String? = null,
    val maintenanceWindows: List<MaintenanceWindow>? = null,
    @kotlinx.serialization.Transient val clear: Set<String> = emptySet(),
) {
    companion object {
        fun inheritAll() = PolicyPatch(clear = setOf("automatic", "pauseUntil", "maintenanceWindows"))
        fun inheritWindows() = PolicyPatch(clear = setOf("maintenanceWindows"))
    }
}

/** Exact JSON patch bytes for `policy set`, including deliberate nulls and omitting unchanged keys. */
internal fun encodePolicyPatch(patch: PolicyPatch): ByteArray =
    kotlinx.serialization.json.buildJsonObject {
        if (patch.automatic != null) put("automatic", kotlinx.serialization.json.JsonPrimitive(patch.automatic))
        else if ("automatic" in patch.clear) put("automatic", kotlinx.serialization.json.JsonNull)
        if (patch.pauseUntil != null) put("pauseUntil", kotlinx.serialization.json.JsonPrimitive(patch.pauseUntil))
        else if ("pauseUntil" in patch.clear) put("pauseUntil", kotlinx.serialization.json.JsonNull)
        if (patch.maintenanceWindows != null) {
            put(
                "maintenanceWindows",
                AgentRequestJson.encodeToJsonElement(
                    kotlinx.serialization.builtins.ListSerializer(MaintenanceWindow.serializer()),
                    patch.maintenanceWindows,
                ),
            )
        } else if ("maintenanceWindows" in patch.clear) {
            put("maintenanceWindows", kotlinx.serialization.json.JsonNull)
        }
    }.toString().toByteArray(Charsets.UTF_8)

/** v3: what `status` says about the last operation that touched one service. */
@Serializable
data class ServiceLastOperation(
    val opId: String? = null,
    val kind: String? = null,
    val action: String? = null,
    val at: String? = null,
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

    // v3 additions.
    val process: ServiceProcess? = null,
    val health: ServiceHealth? = null,
    val endpoint: ServiceEndpoint? = null,
    val updates: ServiceUpdates? = null,
    val pendingVersion: String? = null,
    val drain: Boolean? = null,
    val lastOperation: ServiceLastOperation? = null,
    val notes: List<String>? = null,
) {
    /** What the screen calls it. The id is the contract; the name is only ever for reading. */
    val displayName: String get() = name?.takeIf { it.isNotBlank() } ?: id.orEmpty().ifBlank { "Service" }
}

/**
 * What the machine says about the setup copy it holds.
 *
 * A 2.x agent sends only [hash]; the other four are null on a copy that was pushed without an
 * identity. A hash says which bytes. Only [id] and [revision] say which is newer, and without them
 * this app treats a different document as a question rather than as an upgrade.
 */
@Serializable
data class ControllerMark(
    val hash: String? = null,
    val id: String? = null,
    val revision: Long? = null,
    val updatedAt: String? = null,
    /** mac | desktop | phone | cli | legacy. */
    val source: String? = null,
)

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
    /**
     * command | wol.
     *
     * A `wol` action is the agent sending a magic packet itself, which is what lets any machine be a
     * wake helper without a third-party tool on it. It matters to the app for one reason: sending
     * the same packet twice is the same as sending it once, so a `wol` action whose reply was lost
     * can simply be sent again, and a `command` action cannot.
     */
    val kind: String? = null,
) {
    val displayName: String get() = name?.takeIf { it.isNotBlank() } ?: id.orEmpty().ifBlank { "Run" }

    /** True for an action that is safe to repeat after an ambiguous failure. */
    val isIdempotent: Boolean get() = kind == "wol"
}

/** v3: how long the snapshot took and whether it is all of one. */
@Serializable
data class StatusTiming(
    val budgetMs: Long? = null,
    val elapsedMs: Long? = null,
    /**
     * True when the budget ran out before every probe finished.
     *
     * The whole point of the bounded status: a snapshot that arrives with two fields missing beats
     * one that arrives after the client gave up, but only if it says which fields those were.
     */
    val partial: Boolean? = null,
)

/** v3: what the agent says about itself. */
@Serializable
data class AgentSelf(
    val version: String? = null,
    val contract: Int? = null,
    val base: String? = null,
    val node: String? = null,
    /** True when this session came through the restricted dispatcher. */
    val restrictedSession: Boolean? = null,
)

@Serializable
data class ConfigProblem(
    /** error | warning | migration. */
    val level: String? = null,
    val path: String? = null,
    val message: String? = null,
    val fix: String? = null,
)

/**
 * v3: how the agent's own configuration loaded.
 *
 * `ok: false` blocks every mutating command. A malformed file used to collapse into an empty object
 * that turned automatic updates back on; now the agent refuses to act at all instead.
 */
@Serializable
data class ConfigReport(
    val ok: Boolean? = null,
    /** file | defaults | last-known-good. */
    val source: String? = null,
    val problems: List<ConfigProblem>? = null,
)

/** v3: who asked for an operation. */
@Serializable
data class OpInitiator(
    val client: String? = null,
    val device: String? = null,
    val user: String? = null,
) {
    val summary: String
        get() = listOfNotNull(
            client?.takeIf { it.isNotBlank() },
            device?.takeIf { it.isNotBlank() },
        ).joinToString(" on ").ifBlank { "somebody" }
}

@Serializable
data class OpProgress(
    val step: Int? = null,
    @SerialName("of") val outOf: Int? = null,
    val note: String? = null,
)

/** What an operation ended in. Null while it is still running or queued. */
@Serializable
data class OpResult(
    val ok: Boolean? = null,
    val action: String? = null,
    val reasonCode: String? = null,
    val message: String? = null,
    val from: String? = null,
    val to: String? = null,
    val exitCode: Int? = null,
    /** What a configured action printed. The agent has always returned it; it is now shown. */
    val output: String? = null,
    /**
     * Whether the agent checked that the thing it set out to do actually happened.
     *
     * False on a forced command service update with no way to verify. Never rendered as success
     * without saying so, because force bypasses only the busy gate and never a missing postcondition.
     */
    val verified: Boolean? = null,
)

@Serializable
data class OpLogEntry(
    val at: String? = null,
    val line: String? = null,
)

/** One service's outcome inside a `cycle`. */
@Serializable
data class OpChild(
    val opId: String? = null,
    val service: String? = null,
    val action: String? = null,
    val reasonCode: String? = null,
)

/**
 * One operation, exactly as the agent stores and returns it.
 *
 * This is the record the whole recovery story rests on: an id the client chose, a state, a phase,
 * and a result that is only filled in once it is genuinely over.
 */
@Serializable
data class OpRecord(
    val id: String? = null,
    val kind: String? = null,
    val service: String? = null,
    val target: String? = null,
    val actionId: String? = null,
    val mode: String? = null,
    val initiator: OpInitiator? = null,
    val state: String? = null,
    val phase: String? = null,
    val progress: OpProgress? = null,
    val requestedAt: String? = null,
    val startedAt: String? = null,
    val updatedAt: String? = null,
    val finishedAt: String? = null,
    val expiresAt: String? = null,
    val pid: Int? = null,
    val detached: Boolean? = null,
    val from: String? = null,
    val to: String? = null,
    val result: OpResult? = null,
    val log: List<OpLogEntry>? = null,
    val children: List<OpChild>? = null,
    val replaced: String? = null,
    val agentVersion: String? = null,
    val systemId: String? = null,
    /** Set when this reply is the record of an earlier request with the same id. */
    val replayed: Boolean? = null,
) {
    val opState: OpState? get() = OpState.fromWire(state)
    val opKind: OpKind? get() = OpKind.fromWire(kind)
    val opPhase: OpPhase? get() = OpPhase.fromWire(phase)
    val opAction: OpAction? get() = OpAction.fromWire(result?.action)

    val isFinished: Boolean get() = opState == OpState.FINISHED

    /** What this operation is about, for a heading: the service, the boot target, or the action. */
    val subject: String?
        get() = service?.takeIf { it.isNotBlank() }
            ?: target?.takeIf { it.isNotBlank() }
            ?: actionId?.takeIf { it.isNotBlank() }
}

/** The short form used by `status.operations` and by `history`. */
@Serializable
data class OpSummary(
    val id: String? = null,
    val kind: String? = null,
    val service: String? = null,
    val target: String? = null,
    val actionId: String? = null,
    val mode: String? = null,
    val state: String? = null,
    val phase: String? = null,
    val action: String? = null,
    val reasonCode: String? = null,
    val requestedAt: String? = null,
    val updatedAt: String? = null,
    val expiresAt: String? = null,
) {
    val opState: OpState? get() = OpState.fromWire(state)
    val opKind: OpKind? get() = OpKind.fromWire(kind)
    val subject: String?
        get() = service?.takeIf { it.isNotBlank() }
            ?: target?.takeIf { it.isNotBlank() }
            ?: actionId?.takeIf { it.isNotBlank() }
}

/** Who holds the operation lock, sent with every `conflict` answer. */
@Serializable
data class OpConflict(
    val opId: String? = null,
    val kind: String? = null,
    val service: String? = null,
    val phase: String? = null,
    val startedAt: String? = null,
)

/** v3: the operations a machine is running, has queued, and finished recently. */
@Serializable
data class OperationsBlock(
    val running: List<OpSummary>? = null,
    val queued: List<OpSummary>? = null,
    val recent: List<OpSummary>? = null,
)

@Serializable
data class ConfiguredMetric(
    val id: String = "",
    val name: String = "",
    val value: Double? = null,
    val unit: String = "",
    val checkedAt: String? = null,
    val error: String? = null,
)

@Serializable
data class AgentStatus(
    val metrics: List<ConfiguredMetric>? = null,
    val ok: Boolean? = null,
    /** 3 on a v3 agent. Absent on 2.x and 1.x, which is what gates every v3 feature. */
    val contract: Int? = null,
    @SerialName("os") val osName: String? = null,
    val system: SystemInfo? = null,
    val hostname: String? = null,
    val agentVersion: String? = null,
    val services: List<ServiceStatus>? = null,
    val bootTargets: List<BootTarget>? = null,
    val actions: List<AgentAction>? = null,
    val busy: BusyStatus? = null,
    val busyAggregate: BusyAggregate? = null,
    val controller: ControllerMark? = null,
    val autoUpdate: Boolean? = null,
    val notes: List<String>? = null,
    /** Set when the agent is reporting its own failure rather than a machine state. */
    val message: String? = null,
    val reasonCode: String? = null,
    val error: String? = null,

    // v3 additions.
    val timing: StatusTiming? = null,
    val agent: AgentSelf? = null,
    val config: ConfigReport? = null,
    val operations: OperationsBlock? = null,
    val updates: SystemUpdates? = null,

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

    /** Whether the far side speaks the contract this build was written against. */
    val speaksV3: Boolean get() = (contract ?: 0) >= Contract.REQUIRED

    /** What the agent reports as its version, wherever it put it. */
    val version: String? get() = agent?.version ?: agentVersion

    val isBusy: Boolean get() = busy?.isBusy == true

    /**
     * Whether every mutation is going to be refused because the agent cannot read its own
     * configuration. Worth saying once at the top rather than once per failed button.
     */
    val configBroken: Boolean get() = config?.ok == false

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
                    staged = t3.staged,
                    appPath = t3.appPath,
                    busy = busy,
                    relay = connect,
                    pendingRestart = pendingRestart,
                    lastUpdate = lastUpdate,
                ),
            )
        }
}

/**
 * The shape shared by update, restart, auto-update, boot, sleep, run, cycle and cancel.
 *
 * A v3 agent embeds the whole operation record under [op]; a 2.x agent sends the flat fields and
 * nothing else. Both are read, and the record wins where both are present.
 */
@Serializable
data class AgentActionResult(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val agentVersion: String? = null,
    val system: SystemInfo? = null,
    /**
     * One of the [OpAction] values. The 2.x agents send this and nothing else, so it stays the
     * fallback for everything below.
     */
    val action: String? = null,
    val reasonCode: String? = null,
    val target: String? = null,
    val service: String? = null,
    val actionId: String? = null,
    val from: String? = null,
    val to: String? = null,
    val message: String? = null,
    val notes: List<String>? = null,
    val autoUpdate: Boolean? = null,
    val exitCode: Int? = null,
    val output: String? = null,
    val verified: Boolean? = null,
    val error: String? = null,

    /** v3: the whole operation record, which is what the app actually tracks. */
    val op: OpRecord? = null,

    /** v3: who holds the lock, on a `conflict`. */
    val conflict: OpConflict? = null,

    /** v3: set when this reply is a replay of an earlier request with the same id. */
    val replayed: Boolean? = null,

    /** v3: the effective policy after `policy set`, and what `policy` returns. */
    val updates: SystemUpdates? = null,
    val services: List<ServicePolicyEntry>? = null,

    /** v3: what `self-update` verified. */
    val manifest: AgentManifestSummary? = null,
) {
    /** The action, preferring the operation record's own result. */
    val effectiveAction: String? get() = op?.result?.action ?: action

    /** The reason, preferring the operation record's own result. */
    val effectiveReasonCode: String? get() = op?.result?.reasonCode ?: reasonCode

    val effectiveMessage: String?
        get() = (op?.result?.message ?: message ?: error)?.takeIf { it.isNotBlank() }

    val effectiveOutput: String? get() = (op?.result?.output ?: output)?.takeIf { it.isNotBlank() }

    val effectiveFrom: String? get() = op?.result?.from ?: op?.from ?: from
    val effectiveTo: String? get() = op?.result?.to ?: op?.to ?: to

    val effectiveVerified: Boolean? get() = op?.result?.verified ?: verified

    val speaksV3: Boolean get() = (contract ?: 0) >= Contract.REQUIRED
}

/** One service's policy inside a `policy` reply. */
@Serializable
data class ServicePolicyEntry(
    val id: String? = null,
    val name: String? = null,
    val updates: ServiceUpdates? = null,
)

/**
 * What `config set` answered.
 *
 * The three outcomes that mean it worked are `stored`, `noop` (a byte-identical re-push, which is
 * what an idempotent retry produces) and `replaced` (the explicit human decision). The two refusals
 * carry [current], the metadata of the copy the machine is keeping, so the app can say what it is up
 * against instead of only that it said no.
 */
@Serializable
data class ConfigSetReply(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val agentVersion: String? = null,
    val system: SystemInfo? = null,
    val reasonCode: String? = null,
    val message: String? = null,
    val notes: List<String>? = null,
    /** stored | noop | replaced. */
    val action: String? = null,
    val hash: String? = null,
    val bytes: Long? = null,
    val id: String? = null,
    val revision: Long? = null,
    val source: String? = null,
    /** True on a refusal where neither side descends from the other. */
    val divergent: Boolean? = null,
    /** What the machine is keeping, on a refusal. */
    val current: ControllerMeta? = null,
) {
    val stored: Boolean get() = action == "stored" || action == "replaced"
    val unchanged: Boolean get() = action == "noop"
    val accepted: Boolean get() = ok == true && (stored || unchanged)
    val reason: ReasonCode? get() = ReasonCode.fromWire(reasonCode)
}

/** The metadata a machine keeps beside the document, as `config meta` and a refusal report it. */
@Serializable
data class ControllerMeta(
    val id: String? = null,
    val revision: Long? = null,
    val updatedAt: String? = null,
    val source: String? = null,
    val hash: String? = null,
    val bytes: Long? = null,
    /** The ancestry of the copy this machine holds. */
    val lineage: List<String>? = null,
    val device: String? = null,
)

/** One `config meta` reply. */
@Serializable
data class ConfigMetaReply(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val agentVersion: String? = null,
    val meta: ControllerMeta? = null,
    val hash: String? = null,
    val message: String? = null,
    val error: String? = null,
)

/** What `self-update` says it verified before swapping the tree. */
@Serializable
data class AgentManifestSummary(
    val schema: Int? = null,
    val version: String? = null,
    val contract: Int? = null,
    val files: Int? = null,
    val signatureVerified: Boolean? = null,
    val keyFingerprint: String? = null,
)

@Serializable
data class AgentVersion(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val agentVersion: String? = null,
    val system: SystemInfo? = null,
    val error: String? = null,
)

/** One check from `doctor`. */
@Serializable
data class DoctorCheck(
    val id: String? = null,
    /** ok | warn | fail. Anything else reads as unknown rather than failing a decode. */
    val level: String? = null,
    val summary: String? = null,
    val detail: String? = null,
    val fix: String? = null,
) {
    val isFailure: Boolean get() = level.equals("fail", ignoreCase = true)
    val isWarning: Boolean get() = level.equals("warn", ignoreCase = true)
    val isGood: Boolean get() = level.equals("ok", ignoreCase = true)
}

@Serializable
data class DoctorReport(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val agentVersion: String? = null,
    val checks: List<DoctorCheck>? = null,
    val message: String? = null,
    val error: String? = null,
) {
    val failures: List<DoctorCheck> get() = checks.orEmpty().filter { it.isFailure }
    val warnings: List<DoctorCheck> get() = checks.orEmpty().filter { it.isWarning }
}

/**
 * One `history` reply.
 *
 * The contract does not fix the key, so all three plausible spellings are read and whichever is
 * present wins.
 */
@Serializable
data class HistoryReply(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val operations: List<OpSummary>? = null,
    val history: List<OpSummary>? = null,
    val items: List<OpSummary>? = null,
    val message: String? = null,
    val error: String? = null,
) {
    val entries: List<OpSummary> get() = operations ?: history ?: items ?: emptyList()
}

/** One line of the agent's log. */
@Serializable
data class LogLine(
    val at: String? = null,
    val command: String? = null,
    val text: String? = null,
)

/** One `logs` reply. Same open question as [HistoryReply]; see question A3. */
@Serializable
data class LogsReply(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val lines: List<LogLine>? = null,
    val logs: List<LogLine>? = null,
    val message: String? = null,
    val error: String? = null,
) {
    val entries: List<LogLine> get() = lines ?: logs ?: emptyList()
}

/** Lenient on the way in, because being strict here only ever turns a readable status into nothing. */
internal val AgentJson: Json = Json {
    ignoreUnknownKeys = true
    isLenient = true
    explicitNulls = false
    coerceInputValues = true
}

/** Strict on the way out, for the one thing this app sends as JSON: a policy patch on stdin. */
internal val AgentRequestJson: Json = Json {
    encodeDefaults = false
    explicitNulls = true
    prettyPrint = false
}
