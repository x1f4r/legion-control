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

struct BusyThread: Decodable, Sendable {
    var threadId: String?
    var title: String?
    var state: String?
}

struct BusyStatus: Decodable, Sendable {
    var busy: Bool?
    var runningTurns: Int?
    var pendingTurns: Int?
    var pendingApprovals: Int?
    var reason: String?
    var unknown: Bool?
    var threads: [BusyThread]?

    var isBusy: Bool { busy ?? false }

    var summary: String {
        if let reason, !reason.isEmpty { return reason }
        return isBusy ? "working" : "idle"
    }
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
struct AgentActionInfo: Decodable, Sendable, Identifiable {
    var rawId: String?
    var name: String?
    var confirm: String?
    var busyGated: Bool?

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case name, confirm, busyGated
    }

    var id: String { rawId ?? name ?? "" }
    var displayName: String { name ?? id }
}

/// What the machine is holding of the controller config. The agent never reads the document, it only
/// stores the bytes it was given, so the hash is the whole of what it has to say about it.
///
/// The key being absent altogether is the thing that matters: an agent from before the setup was
/// shared has nothing to hold, and such a machine is left alone rather than told about a document it
/// would reject. A hash of nothing means it is ready for one and has none yet.
struct ControllerCopy: Decodable, Sendable {
    var hash: String?
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

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case name, kind, installed, latest, channel, upToDate, running, healthy, port
        case staged, appPath, busy, relay, pendingRestart, lastUpdate, canUpdate, canRestart
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
        if hasStagedUpdate { return nil }
        if canUpdate == false { return "\(displayName) cannot be updated from here." }
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
    var notes: [String]?
    var controller: ControllerCopy?

    // The first version of the agent's shape. Read only to build a service out of when `services`
    // is absent; nothing else in the app touches these.
    var t3: T3Status?
    var pendingRestart: Bool?
    var lastUpdate: LastUpdate?
    var connect: RelayStatus?

    var isBusy: Bool { busy?.isBusy ?? false }

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

/// Shared shape of the update / restart / boot / sleep / run / auto-update replies.
struct AgentActionResult: Decodable, Sendable {
    var ok: Bool?
    var action: String?
    var target: String?
    var service: String?
    var from: String?
    var to: String?
    var message: String?
    var autoUpdate: Bool?
    var exitCode: Int?
}

/// The reply to `config set`: the hash the machine now holds, or why it kept what it had.
struct AgentConfigResult: Decodable, Sendable {
    var ok: Bool?
    var hash: String?
    var error: String?
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
