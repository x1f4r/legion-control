import Foundation

/// Running one mutation to a conclusion, whatever the far side does in the middle.
///
/// A long operation is not one round trip any more. The agent takes the request, answers `accepted`
/// straight away and does the work in a process that outlives the ssh session; the client then asks
/// `op ID --wait 20` until it finishes. That is what makes an update survive a dropped link, and it
/// is the same dance for the machines over ssh and for the local Mac, so it is written once here.
///
/// The driver never decides what anything means. It returns the agent's own final reply, plus what
/// it saw on the way, and the models turn that into sentences.
@MainActor
struct OperationDriver {
    /// How a request reaches the far side. The two transports differ in everything except this.
    var send: @MainActor (AgentRequest) async throws -> AgentActionResult
    /// How an operation record is read back.
    var readOperation: @MainActor (AgentRequest) async throws -> AgentOperationResult
    /// Called as the far side moves through its phases, so the UI can show where it has got to.
    var onProgress: @MainActor (AgentOperation) -> Void = { _ in }
    /// How the driver waits before asking again after a poll that did not get through. The tests
    /// hand in one that returns at once.
    var pause: @MainActor (Duration) async -> Void = { try? await Task.sleep(for: $0) }
    /// Called just before a poll is tried again. A machine lets its addresses be dialled again here:
    /// one that did not answer is otherwise left alone for a minute, so the second and third tries
    /// failed at once without leaving this device, and the driver stopped watching ten seconds
    /// after the link dropped rather than after three real attempts.
    var beforeRetry: @MainActor () -> Void = {}

    /// What the whole thing came to.
    struct Conclusion: Sendable {
        /// The agent's final word: either the synchronous reply, or the result inside the finished
        /// operation record.
        var result: AgentActionResult
        /// The record, when there was one.
        var operation: AgentOperation?
        /// True when the far side had already done this exact request under this id and returned the
        /// record unchanged. The proof that a retry did not run the work twice.
        var replayed: Bool
        /// True when the app stopped waiting before the far side finished. Not a failure and not a
        /// success: the record is still there and can be asked about later.
        var stillRunning: Bool
    }

    /// How long the app is prepared to sit watching one operation before it leaves it to the
    /// background reconciliation. Long enough for an ordinary update, short enough that a window
    /// left open on a stuck machine is not held forever.
    static let watchLimit: TimeInterval = 20 * 60
    /// How long each long poll asks the far side to hold the line for.
    static let pollSeconds = 20
    /// How many polls in a row may fail before the app stops watching and leaves the operation to
    /// the background reconciliation, and how long it waits between them.
    static let pollTries = 3
    static let pollRetry: Duration = .seconds(5)

    /// Send the request and, while the agent still owns the work, follow the operation by its id
    /// until it finishes.
    ///
    /// What is followed is decided by the outcome (section 5.6), not by one action word: an
    /// accepted request and a replay of one still running are both `pending`, and so is anything
    /// else the agent says it still holds. Two pending replies are not followed here. A queued
    /// request waits for an idle moment that may be hours away and has a row of its own, and a
    /// machine on its way into a reboot or a suspend cannot be asked anything until it is back.
    ///
    /// A poll that does not get through is not news about the operation. It used to be thrown to
    /// the caller as the failure of the whole request, and a read that never left this device reads
    /// as "never sent", so one dropped poll turned an update that was running perfectly well over
    /// there into a failed one here. The last state the agent reported stands, the poll is tried
    /// again, really dialled each time, and when the link stays down the app stops watching with
    /// the work still pending.
    func perform(_ request: AgentRequest, operationId: String?, dialect: AgentDialect) async throws -> Conclusion {
        let reply = try await send(request)
        let replayed = reply.replayed == true

        guard Outcome.of(reply) == .pending, !reply.isQueued, !reply.isTransition else {
            return Conclusion(result: reply, operation: reply.op, replayed: replayed, stillRunning: false)
        }
        // Pending with no id to follow it by is still pending: nothing here knows it finished.
        guard dialect.supportsOperations, let id = operationId ?? reply.operationId else {
            return Conclusion(result: reply, operation: reply.op, replayed: replayed, stillRunning: true)
        }
        let poll = try AgentRequest.operation(id: id, wait: Self.pollSeconds)

        if let op = reply.op { onProgress(op) }

        let deadline = Date().addingTimeInterval(Self.watchLimit)
        var lastSeen: AgentOperation? = reply.op
        var failedPolls = 0

        while Date() < deadline {
            if Task.isCancelled {
                return Conclusion(result: reply, operation: lastSeen, replayed: replayed, stillRunning: true)
            }
            if failedPolls > 0 { beforeRetry() }
            let answer: AgentOperationResult
            do {
                answer = try await readOperation(poll)
            } catch is CancellationError {
                return Conclusion(result: reply, operation: lastSeen, replayed: replayed, stillRunning: true)
            } catch {
                failedPolls += 1
                if failedPolls >= Self.pollTries {
                    return Conclusion(result: reply, operation: lastSeen, replayed: replayed, stillRunning: true)
                }
                await pause(Self.pollRetry)
                continue
            }
            guard let op = answer.operation else {
                if answer.isUnknownOperation {
                    // The agent took the request and now has no record of it. That is an answer,
                    // and what it says is that nobody can tell any more how the work went: the
                    // record was pruned, or the agent's state was lost under it.
                    var lost = reply
                    lost.outcome = Outcome.unknown.rawValue
                    return Conclusion(result: lost, operation: nil, replayed: replayed, stillRunning: false)
                }
                // Some other refusal of the read, which says nothing about the operation either.
                failedPolls += 1
                if failedPolls >= Self.pollTries {
                    return Conclusion(result: reply, operation: lastSeen, replayed: replayed, stillRunning: true)
                }
                await pause(Self.pollRetry)
                continue
            }
            failedPolls = 0
            lastSeen = op
            onProgress(op)
            if Outcome.of(op).isTerminal {
                return Conclusion(result: Self.finalResult(from: op, accepted: reply),
                                  operation: op,
                                  replayed: replayed,
                                  stillRunning: false)
            }
        }

        return Conclusion(result: reply, operation: lastSeen, replayed: replayed, stillRunning: true)
    }

    /// The finished record read back as though it had been the synchronous reply, so the models have
    /// exactly one shape to interpret however the work was actually run.
    static func finalResult(from operation: AgentOperation, accepted: AgentActionResult) -> AgentActionResult {
        var result = accepted
        result.action = operation.result?.action ?? operation.action
        result.reasonCode = operation.result?.reasonCode ?? operation.reasonCode
        result.message = operation.result?.message ?? accepted.message
        result.from = operation.result?.from ?? operation.from
        result.to = operation.result?.to ?? operation.to
        result.output = operation.result?.output
        result.exitCode = operation.result?.exitCode
        result.ok = operation.result?.ok ?? accepted.ok
        // The record's outcome, never the accepted reply's `pending`: the record is what finished.
        // Its own reduction when it states none, because the fields above are not all the record's.
        // A finished record from before 3.1.0 with no result reduces to `unknown`, and read again
        // from the `ok` it inherited from the accepted reply, it was filed as a success.
        result.outcome = operation.outcome ?? operation.result?.outcome ?? Outcome.of(operation).rawValue
        result.op = operation
        return result
    }
}

/// What an agent reply means for the operation record this app keeps.
///
/// One function, used by both models, so the local Mac and a remote machine can never disagree about
/// what "deferred" or "conflict" does to the history. The outcome decides (section 5.6); the action
/// only says which kind of each one it was, so the history can still tell "nothing to do" from
/// "done", and a conflict, a lapsed queue and a cancelled request from an ordinary hold.
enum OperationOutcome {
    static func state(for result: AgentActionResult, stillRunning: Bool) -> OperationState {
        if stillRunning { return .running }
        return state(Outcome.of(result), action: result.action)
    }

    static func state(_ outcome: Outcome, action: String?) -> OperationState {
        switch outcome {
        case .pending:
            return action == "queued" ? .queued : .running
        case .succeeded:
            return action == "noop" || action == "skipped" ? .noop : .succeeded
        case .deferred:
            switch action {
            case "conflict": return .conflict
            case "expired": return .expired
            case "cancelled": return .cancelled
            default: return .deferred
            }
        case .failed:
            return .failed
        case .unknown:
            return .unknown
        }
    }
}

/// What a maintenance cycle came to, decided in one place for its reply and for its record read
/// again by its id.
///
/// The verdict starts from the cycle's own outcome, the one the agent states or the reduction of
/// it, and each service's part, read by its own outcome, can only make it less certain. Decided
/// from the services alone, a cycle the agent failed before it reached any of them had no failed
/// service in it and was filed as "nothing to do". Decided from the record alone, a cycle filed
/// unknown for an interrupted service turned green the first time its finished record was read
/// again, because that record says `cycled` whatever became of the services in it.
///
/// A service nobody can say the end of makes the whole cycle unknown, even one the agent calls
/// failed: a failure says the machine is in a known state, and that service may be half updated.
/// The agent reasons the same way about a cycle that throws once it has reached the services.
struct CycleReport: Equatable {
    var outcome: Outcome
    var state: OperationState
    var summary: String
    /// Which service went which way and why, or the agent's own words when no service was reached.
    var detail: String?

    /// Nil when the cycle did not act: one held back, refused or still running has no services to
    /// count, and is said the way any other request is.
    init?(outcome own: Outcome, action: String?, reason: AgentReason?, message: String?,
          children: [AgentOperation.Child], machine: String) {
        guard own == .succeeded || own == .failed || own == .unknown else { return nil }
        let outcomes = children.map { Outcome.of($0) }
        let updated = zip(children, outcomes).filter { $1 == .succeeded && $0.action == "updated" }.count
        let held = outcomes.filter { $0 == .deferred || $0 == .pending }.count
        let failed = outcomes.filter { $0 == .failed }.count
        let unknown = outcomes.filter { $0 == .unknown }.count
        outcome = own == .unknown || unknown > 0 ? .unknown : own == .failed || failed > 0 ? .failed : .succeeded
        state = OperationOutcome.state(outcome, action: action)

        var parts: [String] = []
        if updated > 0 { parts.append("\(updated) updated") }
        if held > 0 { parts.append("\(held) held back") }
        if failed > 0 { parts.append("\(failed) failed") }
        if unknown > 0 { parts.append("\(unknown) may or may not have updated") }
        if parts.isEmpty { parts.append("nothing to do") }
        switch outcome {
        case .failed where failed == 0:
            // The cycle failed of itself, so no service's line says why; the agent's words do.
            let why = reason?.sentence(subject: machine, message: message) ?? message
            summary = "Maintenance cycle on \(machine) failed: \(why ?? "\(machine) gave no reason.")"
        case .unknown where unknown == 0:
            // The agent's own words first: a reason code's sentence was written for an ending that
            // is known, and this one is not.
            let said = message ?? reason?.sentence(subject: machine, message: nil)
            summary = "Maintenance cycle on \(machine) may or may not have taken effect: \(said ?? "\(machine) could not say.")"
        default:
            summary = "Maintenance cycle on \(machine): \(parts.joined(separator: ", "))."
        }
        // The counts say how it went; the lines say which service it went that way for, and why,
        // which is the part anyone acting on a failure needs.
        let lines = children.map { child in
            "\(child.service ?? "service"): \(child.action ?? "no result")\(child.reasonCode.map { " (\($0))" } ?? "")"
        }.joined(separator: "\n")
        detail = lines.isEmpty ? message : lines
    }

    /// A reply, or a finished record read back as one, with the services it names.
    init?(_ result: AgentActionResult, children: [AgentOperation.Child], machine: String) {
        self.init(outcome: Outcome.of(result), action: result.action, reason: result.reason,
                  message: result.message, children: children, machine: machine)
    }

    /// A finished cycle record, read by its id. Only a whole record names its services; a summary
    /// from `status` or `history` carries none of them and cannot decide a cycle.
    init?(_ operation: AgentOperation, machine: String) {
        self.init(outcome: Outcome.of(operation), action: operation.resolvedAction, reason: operation.resolvedReason,
                  message: operation.resolvedMessage, children: operation.children ?? [], machine: machine)
    }
}
