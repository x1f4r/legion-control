import Foundation

// MARK: - Decoded agent payloads

// Every field is optional on purpose: the app must keep working when the agent on the far side is a
// little older or newer than the app, so a missing key degrades one row instead of failing the decode.

/// The first version of the agent reported exactly one thing, and reported it here. A status with no
/// `services` array is read as one service built out of this block, which is what lets the apps be
/// updated before the machines are.
struct T3Status: Decodable, Sendable {
    var installed: String?
    var nightly: String?
    var upToDate: Bool?
    var serverRunning: Bool?
    var healthy: Bool?
    var port: Int?
    /// The build the updater has already downloaded and is holding for the next quit.
    var staged: StagedUpdate?
    /// Which application bundle the agent is actually looking at.
    var appPath: String?
}

/// The staged build, read leniently.
///
/// The agent names a staged version as a plain string, but this app has to survive talking to an
/// agent that is a little older or newer than itself, and "an update is waiting" is exactly the kind
/// of field that grows from a string into an object. Accept a string, a flag, or an object, and let
/// every other shape mean "nothing known" instead of failing the whole status decode.
struct StagedUpdate: Decodable, Sendable {
    var version: String?
    var explicitlyWaiting: Bool?

    private enum CodingKeys: String, CodingKey {
        case version, staged, fileName, waiting, pending
    }

    init(from decoder: any Decoder) throws {
        if let single = try? decoder.singleValueContainer() {
            if let text = try? single.decode(String.self) {
                version = text.isEmpty ? nil : text
                return
            }
            if let flag = try? single.decode(Bool.self) {
                explicitlyWaiting = flag
                return
            }
        }
        let keyed = try decoder.container(keyedBy: CodingKeys.self)
        version = try keyed.decodeIfPresent(String.self, forKey: .version)
            ?? keyed.decodeIfPresent(String.self, forKey: .staged)
            ?? keyed.decodeIfPresent(String.self, forKey: .fileName)
        explicitlyWaiting = try keyed.decodeIfPresent(Bool.self, forKey: .waiting)
            ?? keyed.decodeIfPresent(Bool.self, forKey: .pending)
    }

    /// The pending directory keeps the zip for the build that is already running, so the file being
    /// there proves nothing. A staged version that differs from the installed one is the only honest
    /// "waiting" signal, and the agent saying so outright still wins.
    func isWaiting(installed: String?) -> Bool {
        if let explicitlyWaiting { return explicitlyWaiting }
        guard let version, !version.isEmpty else { return false }
        // Both halves of the comparison are needed. A missing installed version means the agent could
        // not read the bundle, not that the bundle is on nothing, and "differs from nothing" would
        // announce an update that may well be the build already running.
        guard let installed, !installed.isEmpty else { return false }
        return version != installed
    }
}

struct BusyThread: Decodable, Sendable, Equatable {
    var threadId: String?
    var title: String?
    var state: String?
}

struct BusyStatus: Decodable, Sendable, Equatable {
    var busy: Bool?
    var runningTurns: Int?
    var pendingTurns: Int?
    var pendingApprovals: Int?
    var staleTurns: Int?
    var staleApprovals: Int?
    var reason: String?
    var unknown: Bool?
    var threads: [BusyThread]?
    var threadsTruncated: Int?
    /// False when the service declares no busy probe at all. The distinction matters more than it
    /// looks: an unmonitored service is not an idle one, and treating the two the same is how a
    /// disruptive action gets through while work is running.
    var monitored: Bool?
    /// t3-sqlite | command | http | none | unmonitored | probe-error | timed-out
    var evidence: String?
    var checkedAt: String?
    var elapsedMs: Int?
    var error: String?

    var isBusy: Bool { busy ?? false }

    /// Whether the answer is "we do not know", which blocks disruptive work exactly as busy does.
    var isUnknown: Bool { unknown == true }

    /// Whether the service says anything at all about being busy.
    var isMonitored: Bool { monitored ?? (evidence != "unmonitored") }

    var summary: String {
        if let reason, !reason.isEmpty { return reason }
        if isUnknown { return "busy state unknown" }
        if !isMonitored { return "not monitored" }
        return isBusy ? "working" : "idle"
    }

    /// The one line under the "Doing now" row, which has to be honest about the three states rather
    /// than the two the app used to draw.
    var verdict: Verdict {
        if isBusy { return .busy }
        if isUnknown { return .unknown }
        if !isMonitored { return .unmonitored }
        return .idle
    }

    enum Verdict: Sendable, Equatable { case busy, idle, unknown, unmonitored }
}

struct LastUpdate: Decodable, Sendable {
    var at: String?
    var from: String?
    var to: String?
    var result: String?
    var message: String?
}

/// A tunnel that has to be up for a service to be reachable from outside.
struct RelayStatus: Decodable, Sendable {
    var configured: Bool?
    var running: Bool?
}

/// Which system answered. Absent from the first version of the agent, where the platform name is the
/// system id.
struct SystemIdentity: Decodable, Sendable {
    var id: String?
    var name: String?
}

/// One system this one can boot into, as the agent on the far side knows it. The name is only used
/// when the controller config has no name for that id.
struct BootTargetInfo: Decodable, Sendable, Identifiable {
    var rawId: String?
    var name: String?

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case name
    }

    var id: String { rawId ?? name ?? "" }
}

/// A named command the agent offers a button for. No version, no health, no busy state of its own.
struct AgentActionInfo: Decodable, Sendable, Equatable, Identifiable {
    var rawId: String?
    var name: String?
    var confirm: String?
    var busyGated: Bool?
    /// `command` for an argv the machine runs, `wol` for a magic packet the agent sends itself.
    ///
    /// The difference matters in exactly one place, and it matters a lot there: a `wol` action is a
    /// declared, idempotent UDP send, so a wake attempt whose reply was lost may be repeated or
    /// followed by another helper. A `command` action may do anything, so an ambiguous outcome has
    /// to be settled before anything else is tried.
    var kind: String?

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case name, confirm, busyGated, kind
    }

    var id: String { rawId ?? name ?? "" }
    var displayName: String { name ?? id }

    /// True only when the agent says outright that this action is a magic packet.
    var isDeclaredWakePacket: Bool { kind == "wol" }
}

/// What the machine is holding of the controller config. The agent never reads the document, it only
/// stores the bytes it was given, so the hash is the whole of what it has to say about it.
///
/// The key being absent altogether is the thing that matters: an agent from before the setup was
/// shared has nothing to hold, and such a machine is left alone rather than told about a document it
/// would reject. A hash of nothing means it is ready for one and has none yet.
struct ControllerCopy: Decodable, Sendable, Equatable {
    var hash: String?
    /// Which setup the copy belongs to and how far along it is. Absent from an agent that only ever
    /// stored bytes, and from a document that was written by hand.
    var id: String?
    var revision: Int?
    var updatedAt: String?
    var source: String?
    /// The human name of the device that wrote it.
    var device: String?
    /// What it descends from. Only `config meta` and a refusal carry this; `status` deliberately
    /// leaves it out to keep the snapshot small.
    var lineage: [String]?
    var bytes: Int?

    var identity: ControllerIdentity {
        ControllerIdentity(id: id, revision: revision, updatedAt: updatedAt, source: source,
                           device: device, lineage: lineage)
    }
}

/// Something on a system that has a version, can be running or not, can be busy, and can be updated
/// and restarted.
struct ServiceStatus: Decodable, Sendable, Identifiable {
    var rawId: String?
    var name: String?
    var kind: String?
    var installed: String?
    var latest: String?
    var channel: String?
    var upToDate: Bool?
    var running: Bool?
    var healthy: Bool?
    var port: Int?
    /// Kind "app" only: the build the updater has downloaded and is holding for the next quit.
    var staged: StagedUpdate?
    /// Kind "app" only: which application bundle the agent is looking at.
    var appPath: String?
    var busy: BusyStatus?
    var relay: RelayStatus?
    var pendingRestart: Bool?
    var lastUpdate: LastUpdate?
    var canUpdate: Bool?
    var canRestart: Bool?
    /// The named actions this particular service offers, as opposed to the machine-wide ones.
    var actions: [AgentActionInfo]?
    /// v3: the service process itself, told apart from whether it answers.
    var process: AgentProcessInfo?
    /// v3: whether it answers, told apart from whether the process exists.
    var health: AgentHealthInfo?
    /// v3: whether it is reachable from outside, which relay-process-is-up never proved. Only ever
    /// filled in by `doctor --deep`.
    var endpoint: AgentEndpointInfo?
    /// v3: the effective update policy for this service.
    var updates: AgentUpdatePolicy?
    /// v3: a newer build the agent has found and is holding for an idle window.
    var pendingVersion: String?
    /// v3: whether the service can be asked to stop taking new work before it is stopped.
    var drain: Bool?
    var lastOperation: AgentLastOperation?
    /// Anything the agent could not read while describing this service, in its own words. Drawn as
    /// written: "not checked" is a fact about the reading, and hiding it makes the row lie.
    var notes: [String]?

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case name, kind, installed, latest, channel, upToDate, running, healthy, port
        case staged, appPath, busy, relay, pendingRestart, lastUpdate, canUpdate, canRestart
        case actions, process, health, endpoint, updates, pendingVersion, drain, lastOperation, notes
    }

    var id: String { rawId ?? name ?? "service" }
    var displayName: String { name ?? id }

    var isBusy: Bool { busy?.isBusy ?? false }

    var busyReason: String? {
        guard let busy, busy.isBusy else { return nil }
        return busy.summary
    }

    /// The build downloaded and waiting for a quit, when there really is one.
    var stagedVersion: String? {
        guard let staged, staged.isWaiting(installed: installed) else { return nil }
        return staged.version
    }

    var hasStagedUpdate: Bool { staged?.isWaiting(installed: installed) ?? false }

    /// Why the update button can do nothing, or nil when there really is something to install.
    ///
    /// The "no" answers are deliberately kept apart. Up to date means we compared and there is
    /// nothing to fetch; a missing latest means the source could not be reached, so we compared
    /// against nothing at all and have no idea. Neither is an invitation to press the button, and
    /// treating the second as "an update is available" would offer work that may not exist.
    var updateUnavailableReason: String? {
        if canUpdate == false { return "\(displayName) cannot be updated from here." }
        if hasStagedUpdate { return nil }
        if installed == nil { return "\(displayName) is not installed on this system." }
        switch upToDate {
        case false: return nil
        case true: return "Already on the latest version, so there is nothing to install."
        case nil: return "The latest version could not be checked, so there is nothing to compare against."
        }
    }

    var canBeUpdated: Bool { updateUnavailableReason == nil }
}

struct AgentStatus: Decodable, Sendable {
    var ok: Bool?
    var reasonCode: String?
    /// Set when the agent is reporting its own failure rather than a machine state.
    var message: String?
    var os: String?
    var system: SystemIdentity?
    var hostname: String?
    var agentVersion: String?
    var services: [ServiceStatus]?
    var busy: BusyStatus?
    var bootTargets: [BootTargetInfo]?
    var actions: [AgentActionInfo]?
    var autoUpdate: Bool?
    /// Whatever the agent could not read while answering. Shown verbatim: these are the sentences
    /// that turn "not checked" from a shrug into something a person can act on.
    var notes: [String]?
    var controller: ControllerCopy?

    // v3.
    /// The contract the far side speaks. Absent means 2.x or older, and every v3 flag stays unsent.
    var contract: Int?
    /// How long the agent gave itself and whether it ran out. A partial snapshot is still true; it
    /// is simply not all of it, and a row from one must say so rather than reading as "not there".
    var timing: AgentTiming?
    var agent: AgentInfo?
    /// Whether the agent could read its own config. `ok: false` blocks every mutation over there.
    var config: AgentConfigState?
    /// Running, queued and recent operations.
    var operations: AgentOperationsBlock?
    /// The system-wide update policy.
    var updates: AgentUpdatePolicy?
    var metrics: [AgentMetric]?

    // The first version of the agent's shape. Read only to build a service out of when `services`
    // is absent; nothing else in the app touches these.
    var t3: T3Status?
    var pendingRestart: Bool?
    var lastUpdate: LastUpdate?
    var connect: RelayStatus?

    var isBusy: Bool { busy?.isBusy ?? false }

    /// The contract the far side speaks, with 2 meaning "an agent from before contracts". Every
    /// v3-only flag and command is gated on this rather than on a version number the app would have
    /// to keep a table of.
    var contractVersion: Int { contract ?? agent?.contract ?? 2 }

    /// Whether this agent is new enough for the safety properties the app depends on.
    var speaksRequiredContract: Bool { contractVersion >= requiredContract }

    /// The agent version, wherever it put it.
    var version: String? { agent?.version ?? agentVersion }

    /// True when the app is talking through the restricted dispatcher rather than a full shell.
    var isRestrictedSession: Bool { agent?.restrictedSession == true }

    /// True when the agent ran out of its budget and answered with what it had.
    var isPartial: Bool { timing?.partial == true }

    /// Whether the far side will refuse every mutation because its own config is broken.
    var refusesMutations: Bool { config?.ok == false }

    /// Everything the agent could not read, across the machine and every service on it. Deduplicated
    /// because a probe that fails once per service would otherwise print the same sentence four
    /// times.
    var allNotes: [String] {
        var seen = Set<String>()
        var out: [String] = []
        for note in (notes ?? []) + (services ?? []).flatMap({ $0.notes ?? [] })
        where seen.insert(note).inserted {
            out.append(note)
        }
        for problem in config?.problems ?? [] where seen.insert(problem.sentence).inserted {
            out.append("agent config: \(problem.sentence)")
        }
        return out
    }

    /// Operations still running over there.
    var runningOperations: [AgentOperation] { operations?.running ?? [] }
    /// Requests the agent is holding until the machine is idle.
    var queuedOperations: [AgentOperation] { operations?.queued ?? [] }
    var recentOperations: [AgentOperation] { operations?.recent ?? [] }

    /// One operation by id, wherever in the block it is.
    func operation(id: String) -> AgentOperation? {
        (operations?.all ?? []).first { $0.id == id }
    }

    /// Which platform answered, when the agent named one we know.
    var platform: Platform? { os.flatMap(Platform.init(rawValue:)) }

    /// The system id the far side reports. With no `system` block that is the platform name, which
    /// is exactly what an agent written before systems existed would have been configured as.
    var systemId: String? {
        if let id = system?.id, !id.isEmpty { return id }
        return os
    }

    var systemName: String? {
        guard let name = system?.name, !name.isEmpty else { return nil }
        return name
    }

    /// The services to draw.
    ///
    /// An agent that predates `services` reports one thing, in `t3`, with its busy state, its relay
    /// and its last update at the top level of the status. Reading that as a single service is what
    /// lets this app be updated before the machines are.
    var resolvedServices: [ServiceStatus] {
        if let services { return services }
        guard let t3 else { return [] }
        return [ServiceStatus(
            rawId: "t3",
            name: "T3 Code",
            kind: nil,
            installed: t3.installed,
            latest: t3.nightly,
            channel: nil,
            upToDate: t3.upToDate,
            running: t3.serverRunning,
            healthy: t3.healthy,
            port: t3.port,
            staged: t3.staged,
            appPath: t3.appPath,
            busy: busy,
            relay: connect,
            pendingRestart: pendingRestart,
            lastUpdate: lastUpdate,
            canUpdate: nil,
            canRestart: nil
        )]
    }

    /// Whether the far side really has a services array. The command line grew `--service` at the
    /// same time, so this is also the test for whether it is safe to pass one.
    var reportsServices: Bool { services != nil }

    /// Whether the far side keeps a copy of the controller config at all. The command line grew
    /// `config set` at the same time, so this is also the test for whether there is anything to
    /// push the document to.
    var reportsControllerCopy: Bool { controller != nil }

    /// The sha256 the machine reports for the copy it holds, or nil when it holds none.
    var controllerHash: String? {
        guard let hash = controller?.hash, !hash.isEmpty else { return nil }
        return hash
    }

    func service(id: String) -> ServiceStatus? { resolvedServices.first { $0.id == id } }
}

/// The reply to every mutating command: update, restart, boot, sleep, run, cycle, cancel,
/// auto-update, policy set.
///
/// Both the 2.x flat shape and the v3 envelope are read from the same struct, because an app that
/// has to keep working against both cannot afford two decoders that can disagree.
struct AgentActionResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var agentVersion: String?
    var system: SystemIdentity?
    /// updated | noop | deferred | queued | failed | rolled-back | accepted | conflict |
    /// interrupted | cancelled | expired | restarted | rebooting | rebooted | armed | sleeping |
    /// slept | ran | cycled | skipped
    var action: String?
    var target: String?
    var service: String?
    var from: String?
    var to: String?
    var message: String?
    var reasonCode: String?
    var notes: [String]?
    var autoUpdate: Bool?
    var exitCode: Int?
    /// What a configured action actually printed. The agent has always returned this and neither app
    /// ever showed it, which is why "the action ran" was as much as anyone ever learned.
    var output: String?
    /// The durable record this belongs to. Present on every v3 mutating reply, and the whole reason
    /// an outcome that was lost can be asked about later instead of guessed at now.
    var op: AgentOperation?
    /// True when the agent already had a record under this id and returned it unchanged. Proof that
    /// a retry after a dropped link did not start the work a second time.
    var replayed: Bool?
    /// Set on `action: "conflict"`: what is already running over there.
    var conflict: Conflict?
    /// Set when something was replaced: `true` on a `config set --replace`, and the id of the
    /// superseded request on a queued one. Both shapes are accepted, because they are the same word
    /// meaning two things and neither is worth failing a whole decode over.
    var replaced: ReplacedMarker?
    /// When a queued request stops waiting, echoed at the top level as well as inside the record.
    var expiresAt: String?
    /// Set when the agent could not detach and ran the work synchronously instead.
    var detached: Bool?
    /// One entry per service, for a cycle.
    var children: [AgentOperation.Child]?
    /// The effective policy, on an `auto-update` or `policy set` reply.
    var updates: AgentUpdatePolicy?

    var didInstallAgent: Bool { ok == true && action == "installed" }

    var reason: AgentReason? { AgentReason(code: reasonCode) }

    /// The operation id the far side is filing this under, wherever it put it.
    var operationId: String? { op?.id.isEmpty == false ? op?.id : nil }

    /// True when the reply is "I have started it, ask me later". The client then polls `op`.
    var isAccepted: Bool { action == "accepted" }

    /// True when the far side refused because something else is already running.
    var isConflict: Bool { action == "conflict" }

    /// True when the request was taken and is being held for an idle moment.
    var isQueued: Bool { action == "queued" }

    /// `true`, or the id of what was replaced.
    struct ReplacedMarker: Decodable, Sendable, Equatable {
        var flag: Bool?
        var id: String?

        init(from decoder: any Decoder) throws {
            let container = try decoder.singleValueContainer()
            if let flag = try? container.decode(Bool.self) { self.flag = flag; return }
            if let id = try? container.decode(String.self) { self.id = id; self.flag = true; return }
            self.flag = nil
        }

        var didReplace: Bool { flag == true || id != nil }
    }

    struct Conflict: Decodable, Sendable, Equatable {
        var opId: String?
        var kind: String?
        var service: String?
        var phase: String?
        var startedAt: String?

        var summary: String {
            let what = [kind, service].compactMap { $0 }.joined(separator: " ")
            let where_ = phase.map { " (\($0))" } ?? ""
            return what.isEmpty ? "another operation" : what + where_
        }
    }
}

/// The reply to `config set`, `config` and `config meta`: what the machine now holds.
struct AgentConfigResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var hash: String?
    var error: String?
    var message: String?
    var reasonCode: String?
    var meta: ControllerCopy?
    /// What the machine is holding, on a refusal. This is what makes a conflict something the app can
    /// show rather than only report: it carries the other side's revision, author and lineage.
    var current: ControllerCopy?
    /// True on a refusal where neither side descends from the other, as opposed to one where this
    /// device is simply behind.
    var divergent: Bool?
    /// `stored`, `noop` or `replaced`.
    var action: String?
    /// How big the stored document is.
    var bytes: Int?
    /// Accepted flat as well, for an agent that does not nest them.
    var id: String?
    var revision: Int?
    var updatedAt: String?
    var source: String?

    var reason: AgentReason? { AgentReason(code: reasonCode) }

    var identity: ControllerIdentity {
        if let meta { return meta.identity }
        if let current { return current.identity }
        return ControllerIdentity(id: id, revision: revision, updatedAt: updatedAt, source: source)
    }

    /// True when the machine took the document.
    var wasStored: Bool { ok != false && (action == nil || action == "stored" || action == "noop" || action == "replaced") }

    /// The hash the machine reports, wherever it put it. On a refusal that is the hash of what it
    /// kept, which is exactly what the reconciliation needs to know.
    var storedHash: String? { hash ?? meta?.hash ?? current?.hash }
}

/// The reply to `op ID`.
struct AgentOperationResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var message: String?
    var reasonCode: String?
    var op: AgentOperation?
    /// Some replies carry the record at the top level rather than under `op`.
    var id: String?
    var kind: String?
    var state: String?

    var reason: AgentReason? { AgentReason(code: reasonCode) }

    /// The record, when the agent had one.
    ///
    /// The top-level `id` on this reply is the id that was asked about, not a record: a reply with
    /// `op: null` is the agent saying it has never heard of it, and reconstructing something from
    /// the echoed id would turn that answer into a phantom operation.
    var operation: AgentOperation? { op }

    /// True when the agent answered and has no record of that id.
    ///
    /// That is itself an answer, and the whole reason an id is carried: an agent that files every
    /// operation it starts and has no record of this one never started it. Anything else — a
    /// restricted key, a broken config — leaves the question open rather than settling it wrongly.
    var isUnknownOperation: Bool {
        guard op == nil else { return false }
        switch reason {
        case nil, .notConfigured: return true
        default: return false
        }
    }
}

// MARK: - Systems as the app draws them

/// One system, resolved against the config.
///
/// Usually this is a configured system and nothing more. It also has to cover the case where the
/// agent names a system the config has never heard of, because the alternative is drawing nothing
/// at all for a machine that is up and answering.
struct SystemDescriptor: Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    var symbolName: String
    /// The configured system this resolved to, when it resolved to one. Only a configured system
    /// can be commanded: its `agent` argv is how anything is run over there.
    var configured: SystemConfig?

    init(_ system: SystemConfig) {
        id = system.id
        name = system.name
        symbolName = system.symbolName
        configured = system
    }

    /// A system the config does not describe. Named by its own id, drawn with whatever the platform
    /// suggests, and understood to be uncommandable.
    init(unknownId id: String, platform: Platform?) {
        self.id = id
        name = id
        symbolName = platform?.symbolName ?? "questionmark.circle"
        configured = nil
    }

    /// Which system answered, given what the agent said and what the config knows.
    ///
    /// The id is tried first because it is the thing the two files are meant to agree on. A machine
    /// whose agent was configured before the controller was still names its platform, and one Linux
    /// system in the config is unambiguously the Linux one, so that is the second chance. Only when
    /// both fail is the system drawn as itself and left uncommandable.
    static func resolve(_ status: AgentStatus, on machine: Machine, fallback: SystemConfig) -> SystemDescriptor {
        if let id = status.systemId, let configured = machine.system(id: id) {
            return SystemDescriptor(configured)
        }
        if let platform = status.platform, let configured = machine.system(platform: platform) {
            return SystemDescriptor(configured)
        }
        if let id = status.systemId, !id.isEmpty {
            return SystemDescriptor(unknownId: id, platform: status.platform)
        }
        // Nothing was said at all, so the command shape that worked is the only evidence there is.
        return SystemDescriptor(fallback)
    }
}
