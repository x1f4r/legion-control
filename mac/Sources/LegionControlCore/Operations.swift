import Foundation
import Observation

/// What kind of thing was asked for.
enum OperationKind: String, Codable, Sendable, Equatable {
    case update
    case updateAll
    case restart
    case boot
    case sleep
    case action
    case autoUpdate
    case policy
    case queue
    case wake
    case shareSetup
    case appUpdate

    var title: String {
        switch self {
        case .update: "Update"
        case .updateAll: "Update everything"
        case .restart: "Restart"
        case .boot: "Boot"
        case .sleep: "Sleep"
        case .action: "Action"
        case .autoUpdate: "Automatic updates"
        case .policy: "Update policy"
        case .queue: "Queued request"
        case .wake: "Wake"
        case .shareSetup: "Share setup"
        case .appUpdate: "Legion Control update"
        }
    }

    /// Whether losing track of this one is dangerous. A read, a setting and a wake are all safe to
    /// repeat; an update, a restart, a reboot and a suspend are not.
    var isDisruptive: Bool {
        switch self {
        case .update, .updateAll, .restart, .boot, .sleep, .action, .appUpdate: true
        case .autoUpdate, .policy, .queue, .wake, .shareSetup: false
        }
    }
}

/// Where an operation ended up.
enum OperationState: String, Codable, Sendable, Equatable {
    case running
    case succeeded
    /// It ran and deliberately did nothing. Kept apart from success because "nothing happened" is
    /// only good news for one of the several reasons it happens.
    case noop
    /// The far side refused because the machine was busy.
    case deferred
    /// Taken and held for the next idle moment.
    case queued
    /// Refused because something else already holds the machine.
    case conflict
    /// A queued request that ran out of time before the machine was ever idle.
    case expired
    case failed
    /// The honest one. The command may or may not have run, and this app does not know which.
    case unknown
    case cancelled

    /// Queued is not finished: it is a request the far side is still holding, and the app has to keep
    /// showing it and offering to cancel it.
    var isFinished: Bool { self != .running && self != .queued }

    var title: String {
        switch self {
        case .running: "running"
        case .succeeded: "done"
        case .noop: "nothing to do"
        case .deferred: "held back"
        case .queued: "waiting for idle"
        case .conflict: "refused, something else was running"
        case .expired: "expired"
        case .failed: "failed"
        case .unknown: "outcome unknown"
        case .cancelled: "cancelled"
        }
    }

    var symbolName: String {
        switch self {
        case .running: "circle.dotted"
        case .succeeded: "checkmark.circle"
        case .noop: "minus.circle"
        case .deferred: "clock"
        case .queued: "clock.arrow.circlepath"
        case .conflict: "exclamationmark.arrow.triangle.2.circlepath"
        case .expired: "clock.badge.xmark"
        case .failed: "exclamationmark.triangle"
        case .unknown: "questionmark.circle"
        case .cancelled: "xmark.circle"
        }
    }

    /// Whether the row deserves amber. "Held back" and "waiting for idle" are the system working as
    /// designed, not warnings.
    var isProblem: Bool {
        switch self {
        case .failed, .unknown, .conflict, .expired: true
        default: false
        }
    }
}

/// One step of an operation, as this app saw it.
struct OperationPhase: Codable, Sendable, Equatable, Identifiable {
    var name: String
    var at: Date
    var detail: String?

    var id: String { "\(name)-\(at.timeIntervalSince1970)" }
}

/// One thing the user asked for, with an identity that outlives the process that asked.
///
/// The point of writing this down is the `unknown` state. Before, a command that timed out simply
/// vanished: the app either invented a reboot that had not happened or reported a failure that may
/// well have been a success, and either way there was nothing left afterwards to check against. Now
/// the id goes over with the command, the record survives the app closing, and what actually became
/// of it can be asked later instead of guessed at now.
struct OperationRecord: Codable, Sendable, Equatable, Identifiable {
    var id: String
    var machineId: String
    var machineName: String
    var systemId: String?
    var systemName: String?
    var kind: OperationKind
    /// What it was done to: a service name, an action name, a system to boot into.
    var subject: String
    /// Who asked, in words. Always a person in this app; kept so a future scheduled cycle can be
    /// told apart from a button press.
    var initiator: String
    var targetVersion: String?
    var startedAt: Date
    var finishedAt: Date?
    var state: OperationState
    /// The one sentence to show.
    var summary: String
    /// Raw text worth keeping: ssh's own words, an exit code, a stack frame.
    var detail: String?
    /// What a configured action printed.
    var output: String?
    var phases: [OperationPhase]
    var route: String?
    var attempts: Int?
    /// True while this app still owes the user an answer: the outcome is unknown and the far side
    /// may be able to say. Cleared when it does, or when the user dismisses it.
    var needsReconciliation: Bool
    /// Whether the agent acknowledged the id, which is the only thing that makes reconciliation
    /// possible at all.
    var agentTracked: Bool
    /// The phase the agent last reported, and its progress note.
    var agentPhase: String?
    var agentProgress: String?
    /// When a queued request stops waiting.
    var expiresAt: Date?
    /// Set once the agent has said anything about the id: a reply that queued it, a status that
    /// listed it, a read by id. Its having no record later is then a record it pruned, not one it
    /// never had. Old records lack the key.
    var agentKnew: Bool?
    /// Set on a helper's wake action that is not a declared `wol`: the machine it was run to wake.
    /// While it is set the record is a wake hold (section 10.2), and no other helper runs a command
    /// for that machine; it is cleared when the hold is released. Old builds ignore the key.
    var wakeTarget: String?

    init(
        id: String = UUID().uuidString,
        machineId: String,
        machineName: String,
        systemId: String? = nil,
        systemName: String? = nil,
        kind: OperationKind,
        subject: String,
        initiator: String = "you",
        targetVersion: String? = nil,
        startedAt: Date = Date(),
        state: OperationState = .running,
        summary: String,
        agentTracked: Bool = false,
        wakeTarget: String? = nil
    ) {
        self.id = id
        self.machineId = machineId
        self.machineName = machineName
        self.systemId = systemId
        self.systemName = systemName
        self.kind = kind
        self.subject = subject
        self.initiator = initiator
        self.targetVersion = targetVersion
        self.startedAt = startedAt
        self.finishedAt = nil
        self.state = state
        self.summary = summary
        self.detail = nil
        self.output = nil
        self.phases = [OperationPhase(name: "requested", at: startedAt, detail: nil)]
        self.route = nil
        self.attempts = nil
        self.needsReconciliation = false
        self.agentTracked = agentTracked
        self.agentPhase = nil
        self.agentProgress = nil
        self.expiresAt = nil
        self.agentKnew = nil
        self.wakeTarget = wakeTarget
    }

    var title: String { "\(kind.title): \(subject)" }

    var duration: TimeInterval { (finishedAt ?? Date()).timeIntervalSince(startedAt) }

    /// The longest any agent holds a queued request: `--expires` is seven days at the most.
    static let longestQueue: TimeInterval = 7 * 24 * 60 * 60

    /// When a queued request stops waiting: its own expiry, or for one that came back without one,
    /// the longest any agent would have held it.
    var queueDeadline: Date { expiresAt ?? startedAt.addingTimeInterval(Self.longestQueue) }

    /// A line for the exported log.
    func logLine() -> String {
        let stamp = ISO8601DateFormatter().string(from: startedAt)
        let finished = finishedAt.map { ISO8601DateFormatter().string(from: $0) } ?? "-"
        return "\(stamp)\t\(finished)\t\(machineName)\t\(systemName ?? "-")\t\(kind.rawValue)\t\(subject)\t\(state.rawValue)\t\(summary.replacingOccurrences(of: "\n", with: " "))"
    }
}

/// Every operation this app has started, kept on disk.
///
/// Serialised through one actor-isolated object on the main actor, so the read-modify-write below
/// is never interleaved with another one. The store the review found losing writes was unlocked and
/// shared a temporary file name; this one has neither problem, and a write that fails is reported
/// rather than dropped.
@MainActor
@Observable
final class OperationStore {
    private(set) var records: [OperationRecord] = []
    /// Set when the history could not be saved. Shown, because a history that is quietly not being
    /// kept is worse than no history at all.
    private(set) var persistenceProblem: String?

    let url: URL
    /// How many to keep. Enough to cover a week of ordinary use, small enough to load instantly.
    private static let limit = 400

    init(url: URL? = nil) {
        self.url = url ?? AppPaths.file("operations.json")
        load()
    }

    // MARK: - Reading

    /// Newest first.
    var newestFirst: [OperationRecord] { records.sorted { $0.startedAt > $1.startedAt } }

    var running: [OperationRecord] { records.filter { $0.state == .running } }

    /// The ones this app still owes an answer for.
    var unresolved: [OperationRecord] {
        records.filter { $0.needsReconciliation && ($0.state == .unknown || $0.state == .running) }
    }

    /// Requests the far side is holding for an idle moment.
    var queued: [OperationRecord] { records.filter { $0.state == .queued } }

    func record(id: String) -> OperationRecord? { records.first { $0.id == id } }

    func records(machineId: String) -> [OperationRecord] {
        newestFirst.filter { $0.machineId == machineId }
    }

    /// The wake holds this device keeps for a machine, oldest first.
    ///
    /// A hold is the record of a helper's command whose run may still be going. It lives in this
    /// device's own history and is never published, and nothing releases it but evidence: the
    /// record read as finished, the helper saying it has no such operation, the target answering,
    /// the helper leaving the setup, or a person choosing to try the other helpers. Time, a restart
    /// and an unreachable helper release nothing.
    func wakeHolds(target: String) -> [OperationRecord] {
        records.filter { $0.wakeTarget == target }.sorted { $0.startedAt < $1.startedAt }
    }

    /// The most recent finished operation, for the one-line summary the panel shows.
    var lastFinished: OperationRecord? {
        records.filter { $0.state.isFinished }.max { $0.finishedAt ?? $0.startedAt < $1.finishedAt ?? $1.startedAt }
    }

    // MARK: - Writing

    @discardableResult
    func begin(_ record: OperationRecord) -> OperationRecord {
        records.append(record)
        trim()
        save()
        return record
    }

    func addPhase(_ id: String, name: String, detail: String? = nil) {
        update(id) { $0.phases.append(OperationPhase(name: name, at: Date(), detail: detail)) }
    }

    func markAgentTracked(_ id: String) {
        update(id) { $0.agentTracked = true }
    }

    func finish(
        _ id: String,
        state: OperationState,
        summary: String,
        detail: String? = nil,
        output: String? = nil,
        route: String? = nil,
        attempts: Int? = nil,
        agentTracked: Bool? = nil,
        /// Set when the caller knows the outcome is owed even though the state is not `unknown`:
        /// an operation the app stopped watching is still running over there.
        forceUnresolved: Bool = false
    ) {
        update(id) { record in
            record.state = state
            record.summary = summary
            record.detail = detail ?? record.detail
            record.output = output ?? record.output
            record.route = route ?? record.route
            record.attempts = attempts ?? record.attempts
            if let agentTracked { record.agentTracked = agentTracked }
            record.finishedAt = Date()
            record.phases.append(OperationPhase(name: state.rawValue, at: Date(), detail: summary))
            // Only a disruptive operation whose fate is genuinely unknown is worth chasing. A
            // setting that may or may not have been saved is answered by the next status.
            record.needsReconciliation = (state == .unknown || forceUnresolved)
                && record.kind.isDisruptive && record.agentTracked
        }
    }

    /// What the far side eventually said about an operation we had lost track of.
    ///
    /// The agent's record is authoritative and this app's guess is not, so a record written down as
    /// "outcome unknown" is corrected here even when that means turning a red row green. A finished
    /// record ends a wake hold on it, whatever its result: the command is no longer running.
    ///
    /// A finished cycle is decided exactly as it was when its reply came back, its services with
    /// it. Decided from its own result alone, which says `cycled` whatever became of them, a cycle
    /// filed unknown for an interrupted service turned green here, and reading the same record
    /// again is no evidence about that service.
    func reconcile(_ id: String, with operation: AgentOperation) {
        update(id) { record in
            let wasQueued = record.state == .queued
            record.agentKnew = true
            if operation.isFinished { Self.release(&record, reason: "the helper's record of it finished") }
            let cycle = record.kind == .updateAll && operation.isFinished
                ? CycleReport(operation, machine: record.machineName) : nil
            switch operation.state {
            case "finished":
                record.state = cycle?.state ?? OperationOutcome.state(
                    for: OperationDriver.finalResult(from: operation, accepted: AgentActionResult()),
                    stillRunning: false
                )
            case "queued":
                record.state = .queued
            case "running":
                record.state = .running
            default:
                break
            }
            record.needsReconciliation = !operation.isFinished && record.state != .queued

            if let cycle {
                record.summary = cycle.summary
                record.detail = cycle.detail ?? record.detail
            } else if let message = operation.result?.message, !message.isEmpty {
                record.summary = message
            } else if let reason = operation.resolvedReason {
                record.summary = reason.sentence(subject: record.subject, message: nil)
            } else if operation.isFinished {
                record.summary = "\(record.title): \(record.state.title), according to \(record.machineName)."
            } else if wasQueued, record.state == .running {
                // Left as it was, the summary went on saying the request was waiting for idle, beside
                // a row that said it was running.
                record.summary = "\(record.title) left the queue and started on \(record.machineName)."
            }
            if let output = operation.result?.output, !output.isEmpty { record.output = output }
            record.agentPhase = operation.phase ?? record.agentPhase
            record.agentProgress = operation.progress?.description ?? record.agentProgress
            if let expires = operation.expiresAt {
                record.expiresAt = ISO8601DateFormatter.lenient.date(from: expires)
            }

            // The agent's own log for this operation is the closest thing to an explanation there is.
            for line in operation.log ?? [] {
                guard let text = line.line, !text.isEmpty else { continue }
                guard !record.phases.contains(where: { $0.detail == text }) else { continue }
                let at = line.at.flatMap { ISO8601DateFormatter.lenient.date(from: $0) } ?? Date()
                record.phases.append(OperationPhase(name: "agent", at: at, detail: text))
            }
            record.phases.sort { $0.at < $1.at }
            if operation.isFinished, record.finishedAt == nil { record.finishedAt = Date() }
        }
    }

    /// The far side is holding this one until the machine is idle.
    func markQueued(_ id: String, expiresAt: Date?, summary: String) {
        update(id) { record in
            record.state = .queued
            record.summary = summary
            record.expiresAt = expiresAt
            record.needsReconciliation = false
            record.agentKnew = true
        }
    }

    /// Where the far side has got to, from a poll.
    func note(_ id: String, phase: String?, progress: String?) {
        update(id) { record in
            if let phase, phase != record.agentPhase {
                record.agentPhase = phase
                record.phases.append(OperationPhase(name: phase, at: Date(), detail: progress))
            }
            if let progress { record.agentProgress = progress }
        }
    }

    /// The far side has never heard of this id. That is itself an answer: an agent that records
    /// every operation it starts and has no record of this one never started it.
    func resolveAsNeverStarted(_ id: String) {
        update(id) { record in
            Self.release(&record, reason: "the helper has no record of it")
            record.state = .failed
            record.needsReconciliation = false
            record.summary = "\(record.title) never reached \(record.machineName): the agent has no record of it."
            if record.finishedAt == nil { record.finishedAt = Date() }
        }
    }

    /// The far side answered this request and has no record of it now: the record was pruned, or
    /// lost under it. Unlike an id it never heard of, that says nothing about whether the work ran,
    /// so the outcome stays unknown. It is no longer owed, because there is nothing left to read,
    /// and a wake hold on it ends, because nothing is still running under an id the helper does not
    /// know (section 10.2).
    func resolveAsLost(_ id: String, summary: String, detail: String? = nil) {
        update(id) { record in
            Self.release(&record, reason: "the helper has no record of it")
            record.state = .unknown
            record.needsReconciliation = false
            record.summary = summary
            record.detail = detail ?? record.detail
            if record.finishedAt == nil { record.finishedAt = Date() }
        }
    }

    /// The far side has no record of an id this app asked it about. What that says depends on what
    /// this app saw of it before: a request still queued here has run out its time or been let go,
    /// one the agent has spoken about before has had its record pruned, and only one it never said
    /// a word about never started.
    func resolveWithNoRecord(_ id: String, now: Date = Date()) {
        guard let unheard = record(id: id) else { return }
        if unheard.state == .queued {
            resolveQueuedAsGone(id, now: now)
        } else if unheard.agentKnew == true {
            resolveAsLost(id, summary: "\(unheard.title) reached \(unheard.machineName), which no longer has a record of it, so how it ended is not known.")
        } else {
            resolveAsNeverStarted(id)
        }
    }

    /// A queued request the far side took and now has no record of at all.
    ///
    /// Past its deadline it is not waiting any more, whatever else became of it, so it is filed as
    /// expired, as of that deadline. Whether it ran in an idle moment before then is not known: the
    /// record that would say so has been pruned, and the summary says that rather than claiming it
    /// never ran. Before its deadline the agent has let go of a request it was still meant to be
    /// holding, which is a lost record like any other.
    func resolveQueuedAsGone(_ id: String, now: Date = Date()) {
        guard let queued = record(id: id), queued.state == .queued else { return }
        let deadline = queued.queueDeadline
        guard deadline <= now else {
            resolveAsLost(id, summary: "\(queued.machineName) no longer holds the queued request and has no record of it, so whether it ran is not known.")
            return
        }
        update(id) { record in
            Self.release(&record, reason: "the helper has no record of it")
            record.state = .expired
            record.needsReconciliation = false
            let ended = record.expiresAt.map { "expired on \($0.formatted(date: .abbreviated, time: .shortened))" }
                ?? "has waited longer than any agent holds one"
            record.summary = "The queued request \(ended). \(record.machineName) no longer has a record of it, so whether it ran before then is not known."
            if record.finishedAt == nil { record.finishedAt = deadline }
        }
    }

    /// The user has read the unknown outcome and does not want to be asked again. A wake hold on it
    /// goes with it: a person has decided they are done with that command.
    func dismissReconciliation(_ id: String) {
        update(id) { record in
            record.needsReconciliation = false
            Self.release(&record, reason: "you dismissed its unknown outcome")
        }
    }

    /// End one wake hold, saying why in the record's own history.
    func releaseWakeHold(id: String, reason: String) {
        guard record(id: id)?.wakeTarget != nil else { return }
        update(id) { Self.release(&$0, reason: reason) }
    }

    /// End every hold for `target` recorded before `date`: the target answered after they were
    /// recorded, so whatever those commands were doing, the machine is up.
    func releaseWakeHolds(target: String, before date: Date, reason: String) {
        for held in wakeHolds(target: target) where held.startedAt < date {
            releaseWakeHold(id: held.id, reason: reason)
        }
    }

    /// End every hold on a helper that is no longer in the setup, which no wake asks any more.
    func releaseWakeHolds(onHelpersOtherThan machines: Set<String>, reason: String) {
        for held in records where held.wakeTarget != nil && !machines.contains(held.machineId) {
            releaseWakeHold(id: held.id, reason: reason)
        }
    }

    private static func release(_ record: inout OperationRecord, reason: String) {
        guard let target = record.wakeTarget else { return }
        record.wakeTarget = nil
        record.phases.append(OperationPhase(name: "wake hold released", at: Date(), detail: "\(target): \(reason)"))
    }

    func clearHistory() {
        records.removeAll()
        save()
    }

    private func update(_ id: String, _ change: (inout OperationRecord) -> Void) {
        guard let index = records.firstIndex(where: { $0.id == id }) else { return }
        change(&records[index])
        save()
    }

    private func trim() {
        guard records.count > Self.limit else { return }
        // Never drop something still unanswered, however old it is, or a wake hold, which only
        // evidence releases.
        let sorted = records.sorted { $0.startedAt > $1.startedAt }
        let keep = sorted.prefix(Self.limit)
        let owed = sorted.dropFirst(Self.limit).filter { $0.needsReconciliation || $0.state == .running || $0.wakeTarget != nil }
        records = Array(keep) + owed
    }

    // MARK: - Disk

    private func load() {
        guard let data = try? Data(contentsOf: url) else { return }
        do {
            records = try JSONDecoder().decode([OperationRecord].self, from: data)
        } catch {
            // A history that cannot be read is not a reason to refuse to run, but it is a reason to
            // say so: the alternative is silently starting a fresh one and losing what was owed.
            persistenceProblem = "The operation history at \(url.path(percentEncoded: false)) could not be read, so it starts empty. \(error.localizedDescription)"
        }
        // Anything that was still running when the app went away has an unknown outcome, not a
        // successful one. Marked as such on the way back in so it can be chased.
        // A queued request lives on the machine, not here, so it is left exactly as it is: the next
        // status either shows it still waiting, or its record read by id says what became of it.
        for index in records.indices where records[index].state == .running {
            records[index].state = .unknown
            records[index].summary = "\(records[index].title) was still running when Legion Control closed, so its outcome is not known."
            records[index].needsReconciliation = records[index].kind.isDisruptive && records[index].agentTracked
        }
    }

    private func save() {
        do {
            let data = try JSONEncoder().encode(records)
            try AtomicFile.write(data, to: url)
            persistenceProblem = nil
        } catch let failure as AtomicFile.WriteFailure {
            persistenceProblem = "The operation history could not be saved. \(failure.message)"
        } catch {
            persistenceProblem = "The operation history could not be saved. \(error.localizedDescription)"
        }
    }

    // MARK: - Exporting

    /// The whole history as tab separated text, for attaching to a bug report.
    func exportText() -> String {
        var lines = ["# Legion Control operation history",
                     "# started\tfinished\tmachine\tsystem\tkind\tsubject\tstate\tsummary"]
        for record in newestFirst {
            lines.append(record.logLine())
            if let detail = record.detail, !detail.isEmpty {
                lines.append("\tdetail: " + detail.replacingOccurrences(of: "\n", with: "\n\t"))
            }
            if let output = record.output, !output.isEmpty {
                lines.append("\toutput: " + output.replacingOccurrences(of: "\n", with: "\n\t"))
            }
        }
        return lines.joined(separator: "\n") + "\n"
    }
}
