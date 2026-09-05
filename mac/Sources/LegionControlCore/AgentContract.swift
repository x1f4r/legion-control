import Foundation

// The agent v3 contract, as this app decodes it.
//
// Every field is optional, always. The app has to keep working against a 2.x agent that has never
// heard of any of this, against a 3.x agent that is a patch ahead of it, and against a 4.x agent
// that has added keys nobody here has seen. So nothing below is required, nothing is renamed, and
// an unrecognised value degrades to "not known" rather than failing the decode of the whole reply.

/// The contract version this app is written against. An agent that reports less than this is missing
/// the safety properties the app depends on, and the machine section says so and offers to install.
let requiredContract = 3

/// The agent version bundled with this build, for the trusted deployment action.
let bundledAgentVersion = "3.0.0"

// MARK: - Reason codes

/// Why something did not take the happy path.
///
/// A closed enum in the contract, with the explicit rule that a client renders by code and falls
/// back to `message`. An unknown code is "failed, show the message" and never a silent success.
enum AgentReason: Sendable, Equatable {
    case busy
    case busyUnknown
    case policyOff
    case policyPaused
    case outsideWindow
    case operationInProgress
    case lockHeld
    case noUpdate
    case latestUnknown
    case notInstalled
    case notRunning
    case appClosed
    case applyAttemptsExhausted
    case applyFailed
    case postconditionFailed
    case rolledBack
    case notConfigured
    case unsupportedPlatform
    case unknownService
    case unknownTarget
    case unknownAction
    case badArgument
    case configInvalid
    case configMissing
    case interrupted
    case expired
    case cancelled
    case alreadyRunning
    case alreadyOnTarget
    case staleRevision
    case controllerConflict
    case signatureInvalid
    case restricted
    case timedOut
    case internalError
    /// A code from a newer agent. Rendered from `message`, and never treated as benign.
    case unrecognised(String)

    init?(code: String?) {
        guard let code, !code.isEmpty else { return nil }
        switch code {
        case "busy": self = .busy
        case "busy-unknown": self = .busyUnknown
        case "policy-off": self = .policyOff
        case "policy-paused": self = .policyPaused
        case "outside-window": self = .outsideWindow
        case "operation-in-progress": self = .operationInProgress
        case "lock-held": self = .lockHeld
        case "no-update": self = .noUpdate
        case "latest-unknown": self = .latestUnknown
        case "not-installed": self = .notInstalled
        case "not-running": self = .notRunning
        case "app-closed": self = .appClosed
        case "apply-attempts-exhausted": self = .applyAttemptsExhausted
        case "apply-failed": self = .applyFailed
        case "postcondition-failed": self = .postconditionFailed
        case "rolled-back": self = .rolledBack
        case "not-configured": self = .notConfigured
        case "unsupported-platform": self = .unsupportedPlatform
        case "unknown-service": self = .unknownService
        case "unknown-target": self = .unknownTarget
        case "unknown-action": self = .unknownAction
        case "bad-argument": self = .badArgument
        case "config-invalid": self = .configInvalid
        case "config-missing": self = .configMissing
        case "interrupted": self = .interrupted
        case "expired": self = .expired
        case "cancelled": self = .cancelled
        case "already-running": self = .alreadyRunning
        case "already-on-target": self = .alreadyOnTarget
        case "stale-revision": self = .staleRevision
        case "controller-conflict": self = .controllerConflict
        case "signature-invalid": self = .signatureInvalid
        case "restricted": self = .restricted
        case "timed-out": self = .timedOut
        case "internal": self = .internalError
        default: self = .unrecognised(code)
        }
    }

    /// The sentence to show. `subject` is the service, action or machine the code is about, and
    /// `message` is the agent's own words, which win wherever the app has nothing better to say.
    func sentence(subject: String, message: String?) -> String {
        switch self {
        case .busy:
            return "\(subject) is busy, so nothing was interrupted."
        case .busyUnknown:
            return "Whether \(subject) is busy could not be established, so nothing was interrupted. This is deliberate: an unknown busy state blocks disruptive work rather than allowing it."
        case .policyOff:
            return "Nothing was installed on the schedule: automatic updates are switched off there. Asking for one explicitly still works."
        case .policyPaused:
            return "Nothing was installed on the schedule: updates are paused there. Asking for one explicitly still works."
        case .outsideWindow:
            return "Nothing was installed on the schedule: it is outside the maintenance window there. Asking for one explicitly still works."
        case .operationInProgress:
            return "Another operation is already running on that machine, so this one was refused rather than run beside it."
        case .lockHeld:
            return "The maintenance lock is held by something else on that machine."
        case .noUpdate:
            return "\(subject) is already on the latest build."
        case .latestUnknown:
            return "The latest version of \(subject) could not be read, so there was nothing to compare against and nothing was installed."
        case .notInstalled:
            return "\(subject) is not installed there."
        case .notRunning:
            return "\(subject) is not running."
        case .appClosed:
            return "\(subject) is not open, so there was nothing to quit and restart."
        case .applyAttemptsExhausted:
            return "The update of \(subject) was attempted as many times as it is allowed and did not go in."
        case .applyFailed:
            return "The update of \(subject) failed while it was being applied."
        case .postconditionFailed:
            return "The update command finished but \(subject) is not on the version it should be, so it is reported as failed rather than as installed."
        case .rolledBack:
            return "The update of \(subject) failed and the previous version was put back."
        case .notConfigured:
            return "\(subject) is not configured on that machine."
        case .unsupportedPlatform:
            return "That machine's platform is not one the agent supports."
        case .unknownService:
            return "That machine has no service called \(subject)."
        case .unknownTarget:
            return "That machine has no boot target called \(subject)."
        case .unknownAction:
            return "That machine has no action called \(subject)."
        case .badArgument:
            return "The agent refused an argument this app sent. \(message ?? "")"
        case .configInvalid:
            return "The agent's own configuration on that machine could not be read, so it refuses to change anything until it is fixed."
        case .configMissing:
            return "There is no agent configuration on that machine."
        case .interrupted:
            return "\(subject) was interrupted before it finished, so what it did is not known."
        case .expired:
            return "The queued request for \(subject) expired before the machine was idle."
        case .cancelled:
            return "The request for \(subject) was cancelled."
        case .alreadyRunning:
            return "That request is already running on the machine."
        case .alreadyOnTarget:
            return "\(subject) is already what is running."
        case .staleRevision:
            return "That machine holds a newer revision of the setup, so nothing was written."
        case .controllerConflict:
            return "That machine holds a setup this Mac does not recognise, so nothing was written."
        case .signatureInvalid:
            return "The signature on the artifact did not verify, so nothing was installed."
        case .restricted:
            return "The key this app connects with is not allowed to run that command."
        case .timedOut:
            return "The agent gave up waiting. \(message ?? "")"
        case .internalError:
            return message ?? "The agent hit an error of its own."
        case .unrecognised:
            return message ?? "The agent refused, for a reason this build does not recognise."
        }
    }

    /// Whether "nothing happened" is good news. Everything else is something a person may want to
    /// act on and must not be drawn as a quiet success.
    var isBenign: Bool {
        switch self {
        case .noUpdate, .notInstalled, .alreadyOnTarget: true
        default: false
        }
    }

    /// Whether this is a policy decision the user can override by asking explicitly.
    var isSchedulePolicy: Bool {
        switch self {
        case .policyOff, .policyPaused, .outsideWindow: true
        default: false
        }
    }
}

// MARK: - The shared reply envelope

// The envelope fields (`ok`, `contract`, `agentVersion`, `system`, `reasonCode`, `message`,
// `notes`) are decoded into each result type rather than through a wrapper, because the payload
// keys sit beside them at the top level. `SystemIdentity` in AgentTypes.swift is the `system` block.

// MARK: - Operations

/// One operation as the agent records it. The same shape is returned by `op`, embedded under `op` in
/// every mutating reply, and summarised in `history` and `status.operations`.
struct AgentOperation: Decodable, Sendable, Equatable, Identifiable {
    var rawId: String?
    /// update | restart | boot | sleep | run | cycle | self-update
    var kind: String?
    var service: String?
    var target: String?
    var actionId: String?
    /// scheduled | manual | force | queued
    var mode: String?
    var initiator: Initiator?
    /// queued | running | finished
    var state: String?
    var phase: String?
    var progress: Progress?
    var requestedAt: String?
    var startedAt: String?
    var updatedAt: String?
    var finishedAt: String?
    var expiresAt: String?
    var pid: Int?
    var detached: Bool?
    var from: String?
    var to: String?
    var result: Result?
    var log: [LogLine]?
    var agentVersion: String?
    var systemId: String?
    /// Present on a replayed reply: this record already existed and was returned unchanged.
    var replayed: Bool?
    /// Only on a cycle record.
    var children: [Child]?
    /// The action name, when a summary carries it flat rather than inside `result`.
    var action: String?
    var reasonCode: String?

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case kind, service, target, actionId, mode, initiator, state, phase, progress
        case requestedAt, startedAt, updatedAt, finishedAt, expiresAt, pid, detached
        case from, to, result, log, agentVersion, systemId, replayed, children, action, reasonCode
    }

    struct Initiator: Decodable, Sendable, Equatable {
        var client: String?
        var device: String?
        var user: String?
    }

    struct Progress: Decodable, Sendable, Equatable {
        var step: Int?
        var of: Int?
        var note: String?

        /// A fraction for a determinate bar, when both halves are there and make sense.
        var fraction: Double? {
            guard let step, let of, of > 0 else { return nil }
            return min(1, max(0, Double(step) / Double(of)))
        }

        var description: String? {
            if let step, let of, of > 0 {
                return note.map { "step \(step) of \(of): \($0)" } ?? "step \(step) of \(of)"
            }
            return note
        }
    }

    struct Result: Decodable, Sendable, Equatable {
        var ok: Bool?
        var action: String?
        var reasonCode: String?
        var message: String?
        var from: String?
        var to: String?
        var exitCode: Int?
        var output: String?
    }

    struct LogLine: Decodable, Sendable, Equatable, Identifiable {
        var at: String?
        var line: String?
        var id: String { "\(at ?? "")-\(line ?? "")" }
    }

    struct Child: Decodable, Sendable, Equatable, Identifiable {
        var opId: String?
        var service: String?
        var action: String?
        var reasonCode: String?
        var id: String { opId ?? service ?? UUID().uuidString }
    }

    var id: String { rawId ?? "" }

    var isFinished: Bool { state == "finished" }
    var isQueued: Bool { state == "queued" }
    var isRunning: Bool { state == "running" }

    /// The action, wherever the agent put it.
    var resolvedAction: String? { result?.action ?? action }
    var resolvedReason: AgentReason? { AgentReason(code: result?.reasonCode ?? reasonCode) }
    var resolvedMessage: String? { result?.message ?? nil }

    /// What to call this in a list.
    var displayName: String {
        switch kind {
        case "update": "Update \(service ?? "the service")"
        case "restart": "Restart \(service ?? "the service")"
        case "boot": "Boot into \(target ?? "the other system")"
        case "sleep": "Sleep"
        case "run": "Run \(actionId ?? "the action")"
        case "cycle": "Maintenance cycle"
        case "self-update": "Agent self-update"
        default: kind ?? "Operation"
        }
    }
}

/// `status.operations`.
struct AgentOperationsBlock: Decodable, Sendable, Equatable {
    var running: [AgentOperation]?
    var queued: [AgentOperation]?
    var recent: [AgentOperation]?

    var all: [AgentOperation] { (running ?? []) + (queued ?? []) + (recent ?? []) }
}

// MARK: - Policy

/// One maintenance window, in the far side's own local time.
struct MaintenanceWindow: Codable, Sendable, Equatable, Identifiable {
    /// mon…sun, lower case. An empty or absent list means every day.
    var days: [String]?
    var from: String?
    var to: String?

    var id: String { "\((days ?? []).joined(separator: ","))-\(from ?? "")-\(to ?? "")" }

    static let allDays = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]

    var effectiveDays: [String] { (days?.isEmpty == false ? days! : Self.allDays) }

    /// "02:00–06:00, every day" or "22:00–02:00, Sat and Sun". Overnight windows are normal and are
    /// named as such, because a window whose end is before its start is the usual way to say "the
    /// small hours" and reads as a mistake otherwise.
    var summary: String {
        let range = "\(from ?? "??:??")–\(to ?? "??:??")"
        let overnight = crossesMidnight ? " (overnight)" : ""
        let days = effectiveDays
        if days.count == 7 { return "\(range)\(overnight), every day" }
        return "\(range)\(overnight), \(days.map { $0.capitalized }.joined(separator: ", "))"
    }

    var crossesMidnight: Bool {
        guard let from, let to else { return false }
        return from > to
    }

    /// Whether the two ends look like `HH:MM` at all. The agent validates properly; this only keeps
    /// the app from sending something obviously wrong.
    var looksValid: Bool {
        func valid(_ value: String?) -> Bool {
            guard let value, value.count == 5, value[value.index(value.startIndex, offsetBy: 2)] == ":" else { return false }
            let parts = value.split(separator: ":")
            guard parts.count == 2, let hour = Int(parts[0]), let minute = Int(parts[1]) else { return false }
            return (0...23).contains(hour) && (0...59).contains(minute)
        }
        guard valid(from), valid(to), from != to else { return false }
        return effectiveDays.allSatisfy { Self.allDays.contains($0) }
    }
}

/// The effective update policy, system-wide or for one service.
///
/// This is the model behind finding 06: scheduled eligibility, an explicit manual request and
/// permission to interrupt busy work are three different things, and switching the schedule off
/// used to switch off all three.
struct AgentUpdatePolicy: Decodable, Sendable, Equatable {
    /// null on a service means "inherit the system's answer".
    var automatic: Bool?
    var pauseUntil: String?
    var maintenanceWindows: [MaintenanceWindow]?
    /// Only on `status.updates`.
    var inWindowNow: Bool?
    var nextWindow: String?
    var lastCycle: LastCycle?
    /// Only on a service: whether these values came from the system rather than from the service.
    var inherited: Bool?
    /// Which individual keys were inherited, when the agent breaks it down that far.
    var inheritedKeys: [String: Bool]?
    var eligibleNow: Bool?
    var deferredReason: String?

    struct LastCycle: Decodable, Sendable, Equatable {
        var opId: String?
        var at: String?
        var action: String?
        var reasonCode: String?
    }

    var pausedUntilDate: Date? {
        guard let pauseUntil else { return nil }
        return ISO8601DateFormatter.lenient.date(from: pauseUntil)
    }

    var isPaused: Bool {
        guard let pausedUntilDate else { return false }
        return pausedUntilDate > Date()
    }

    var deferred: AgentReason? { AgentReason(code: deferredReason) }

    /// The one line the maintenance rows show for the schedule itself.
    var scheduleSummary: String {
        if automatic == false { return "off the schedule" }
        if let pausedUntilDate, pausedUntilDate > Date() {
            return "paused until \(pausedUntilDate.formatted(date: .abbreviated, time: .shortened))"
        }
        guard let windows = maintenanceWindows, !windows.isEmpty else { return "on the schedule, at any time" }
        return "on the schedule, " + windows.map(\.summary).joined(separator: "; ")
    }

    /// The patch to send to `policy set`, built from this policy with one field changed. Only the
    /// three writable keys are ever sent, because the agent treats the body as a patch and every
    /// other key here is derived.
    static func patch(automatic: Bool?? = nil, pauseUntil: Date?? = nil,
                      windows: [MaintenanceWindow]?? = nil) -> Data? {
        var body: [String: Any] = [:]
        if let automatic {
            body["automatic"] = automatic.map { $0 as Any } ?? NSNull()
        }
        if let pauseUntil {
            body["pauseUntil"] = pauseUntil.map { ISO8601DateFormatter().string(from: $0) as Any } ?? NSNull()
        }
        if let windows {
            if let windows {
                body["maintenanceWindows"] = windows.map { window -> [String: Any] in
                    var entry: [String: Any] = [:]
                    entry["days"] = window.effectiveDays
                    if let from = window.from { entry["from"] = from }
                    if let to = window.to { entry["to"] = to }
                    return entry
                }
            } else {
                body["maintenanceWindows"] = NSNull()
            }
        }
        guard !body.isEmpty else { return nil }
        return try? JSONSerialization.data(withJSONObject: body, options: [.sortedKeys])
    }
}

// MARK: - Per-service detail

struct AgentProcessInfo: Decodable, Sendable, Equatable {
    var running: Bool?
    var state: String?
    var startedAt: String?
    var error: String?
}

struct AgentHealthInfo: Decodable, Sendable, Equatable {
    var ok: Bool?
    var status: Int?
    var checkedAt: String?
    var elapsedMs: Int?
    var error: String?
}

/// External reachability, which is not the same thing as the relay process being up. Only ever
/// filled in by `doctor --deep`.
struct AgentEndpointInfo: Decodable, Sendable, Equatable {
    var configured: Bool?
    var reachable: Bool?
    var url: String?
    var error: String?
}

/// The last operation that touched one service.
struct AgentLastOperation: Decodable, Sendable, Equatable {
    var opId: String?
    var kind: String?
    var action: String?
    var reasonCode: String?
    var at: String?
}

// MARK: - Status envelope pieces

struct AgentTiming: Decodable, Sendable, Equatable {
    var budgetMs: Int?
    var elapsedMs: Int?
    var partial: Bool?
}

struct AgentInfo: Decodable, Sendable, Equatable {
    var version: String?
    var contract: Int?
    var base: String?
    var node: String?
    /// True when this app is talking through the restricted dispatcher rather than a full shell.
    var restrictedSession: Bool?
}

/// Whether the agent could read its own configuration. `ok: false` blocks every mutation on the far
/// side, so the app has to be able to say so plainly rather than showing a machine that refuses
/// every button for no visible reason.
struct AgentConfigState: Decodable, Sendable, Equatable {
    var ok: Bool?
    /// file | defaults | last-known-good
    var source: String?
    var problems: [Problem]?

    /// One thing wrong with the agent's own configuration, in its own words, with what to do.
    struct Problem: Decodable, Sendable, Equatable, Identifiable {
        /// error | warning
        var level: String?
        /// Where in its config file, as a path a person can find.
        var path: String?
        var message: String?
        var fix: String?

        var id: String { "\(path ?? "")-\(message ?? "")" }
        var isError: Bool { level == "error" }

        /// The whole thing as one line, for the notes list.
        var sentence: String {
            var parts: [String] = []
            if let path, !path.isEmpty { parts.append(path) }
            if let message { parts.append(message) }
            if let fix, !fix.isEmpty { parts.append("Fix: \(fix)") }
            return parts.joined(separator: " — ")
        }
    }

    var errors: [Problem] { (problems ?? []).filter(\.isError) }

    var summary: String {
        switch source {
        case "file" where ok != false: "read from its own config file"
        case "defaults": "running on inert defaults: it has no config file"
        case "last-known-good": "running on the last config that parsed; the current file is broken"
        default: ok == false ? "could not be read" : "not reported"
        }
    }
}

// MARK: - Doctor

struct AgentCheck: Decodable, Sendable, Equatable, Identifiable {
    var rawId: String?
    /// ok | warn | fail
    var level: String?
    var summary: String?
    var detail: String?
    var fix: String?

    private enum CodingKeys: String, CodingKey {
        case rawId = "id"
        case level, summary, detail, fix
    }

    var id: String { rawId ?? summary ?? UUID().uuidString }
    var displayName: String { rawId ?? "check" }

    enum Verdict: Sendable, Equatable { case ok, warning, failed, unknown }

    var verdict: Verdict {
        switch level?.lowercased() {
        case "ok": .ok
        case "warn": .warning
        case "fail": .failed
        default: .unknown
        }
    }
}

struct AgentDoctorResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var agentVersion: String?
    var message: String?
    var reasonCode: String?
    var notes: [String]?
    var checks: [AgentCheck]?
}

// MARK: - History, logs, policy, bundle

struct AgentHistoryResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var message: String?
    var reasonCode: String?
    /// The contract does not pin the key; all three plausible names are accepted so the app works
    /// whichever one the agent settled on. See integration-notes-mac.txt.
    var operations: [AgentOperation]?
    var history: [AgentOperation]?
    var items: [AgentOperation]?

    var records: [AgentOperation] { operations ?? history ?? items ?? [] }
}

/// A log tail. Each entry is either a bare line or `{ at, line }`, and both are accepted.
struct AgentLogsResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var message: String?
    var reasonCode: String?
    var path: String?
    var lines: [Entry]?

    struct Entry: Decodable, Sendable, Equatable {
        var at: String?
        var line: String?

        init(from decoder: any Decoder) throws {
            if let single = try? decoder.singleValueContainer(), let text = try? single.decode(String.self) {
                line = text
                return
            }
            let keyed = try decoder.container(keyedBy: CodingKeys.self)
            at = try keyed.decodeIfPresent(String.self, forKey: .at)
            line = try keyed.decodeIfPresent(String.self, forKey: .line)
        }

        private enum CodingKeys: String, CodingKey { case at, line }

        var text: String {
            guard let at, !at.isEmpty else { return line ?? "" }
            return "\(at) \(line ?? "")"
        }
    }

    var text: [String] { (lines ?? []).map(\.text) }
}

struct AgentPolicyResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var message: String?
    var reasonCode: String?
    /// Which service was addressed, or null for the system.
    var service: String?
    /// The effective policy for whatever scope was addressed.
    var updates: AgentUpdatePolicy?
    /// Every service's effective policy, so the whole maintenance picture costs one round trip
    /// rather than one per service.
    var services: [ServicePolicy]?
    /// The 2.x boolean, still mirrored.
    var autoUpdate: Bool?
    /// Which keys a `policy set` actually changed.
    var changed: [String]?

    struct ServicePolicy: Decodable, Sendable, Equatable, Identifiable {
        var id: String
        var name: String?
        var updates: AgentUpdatePolicy?
    }

    func policy(forService id: String) -> AgentUpdatePolicy? {
        (services ?? []).first { $0.id == id }?.updates
    }
}

/// `bundle` returns whole sub-documents. They are not decoded here: the app's own export writes them
/// back out as JSON, and re-encoding through a typed model would drop exactly the unfamiliar keys a
/// diagnostic bundle exists to carry.
struct AgentBundleResult: Sendable {
    var raw: Data
}

/// The reply to `config` (read): the document a machine is holding, plus what it says about it.
///
/// The exact bytes matter here in a way they do not anywhere else. The hash both sides compare is a
/// hash of the canonical bytes, and re-serialising a decoded JSON object does not reproduce them:
/// key order, spacing and number formatting are all free. So the raw text is preferred wherever the
/// agent sends it, and a re-encoded object is only ever used when its hash matches what the machine
/// reports — which is checked at the call site before anything is adopted.
struct RemoteControllerDocument: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var message: String?
    var reasonCode: String?
    var hash: String?
    var meta: ControllerCopy?
    /// The document as text, when the agent sends it that way.
    var raw: String?
    /// The document as an object, when it does not.
    var controller: JSONAny?

    private enum CodingKeys: String, CodingKey {
        case ok, contract, message, reasonCode, hash, meta, raw, controller
    }

    var reason: AgentReason? { AgentReason(code: reasonCode) }

    /// The bytes to hash and adopt, or nil when the reply carried no document.
    ///
    /// A `raw` field, when the agent sends one, is the document exactly as it is stored and needs no
    /// reconstruction. Without it the object is written out again with the shared writer, which
    /// reproduces the stored bytes for any document written by a client that sorts its keys. The
    /// caller checks the result against the hash the machine reports and refuses to adopt anything
    /// that does not match, so a document written some other way is a visible message rather than a
    /// silent divergence.
    var documentBytes: Data? {
        if let raw, !raw.isEmpty { return Data(raw.utf8) }
        guard let object = controller?.value as? [String: Any] else { return nil }
        return try? JSONText.canonicalDocument(object)
    }
}

/// The reply to `busy`.
///
/// A different shape from `status`: the top-level `busy` is the aggregate as a plain boolean and
/// each entry in `services` is a busy record rather than a service. This app does not call the
/// command — the status it already polls carries the same evidence — but the type exists so the
/// shared fixtures can be decoded, which is how a drift between the two shapes would be noticed.
struct AgentBusyResult: Decodable, Sendable {
    var ok: Bool?
    var contract: Int?
    var agentVersion: String?
    var system: SystemIdentity?
    var message: String?
    var reasonCode: String?
    var busy: Bool?
    var unknown: Bool?
    var reason: String?
    var monitoredServices: Int?
    var unmonitoredServices: Int?
    var services: [Service]?

    struct Service: Decodable, Sendable, Equatable, Identifiable {
        var id: String
        var name: String?
        var busy: Bool?
        var unknown: Bool?
        var monitored: Bool?
        var reason: String?
        var evidence: String?
        var checkedAt: String?
        var elapsedMs: Int?
        var error: String?
        var runningTurns: Int?
        var pendingTurns: Int?
        var pendingApprovals: Int?
        var staleTurns: Int?
        var staleApprovals: Int?
        var threads: [BusyThread]?
        var threadsTruncated: Int?

        /// The same three-way verdict the status path uses, so the two cannot disagree about what
        /// "not busy" means.
        var verdict: BusyStatus.Verdict {
            if busy == true { return .busy }
            if unknown == true { return .unknown }
            if monitored == false { return .unmonitored }
            return .idle
        }
    }

    /// Whether anything on the machine blocks disruptive work. An unknown or unmonitored service
    /// counts: an absence of evidence is not evidence of idleness.
    var blocksDisruptiveWork: Bool {
        if busy == true || unknown == true { return true }
        return (services ?? []).contains { $0.verdict != .idle }
    }
}
