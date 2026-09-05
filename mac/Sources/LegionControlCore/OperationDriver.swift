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

    /// Send the request and, when the agent detaches, follow the operation until it finishes.
    func perform(_ request: AgentRequest, operationId: String?, dialect: AgentDialect) async throws -> Conclusion {
        let reply = try await send(request)

        // A synchronous agent, or a short command: the reply is the whole answer.
        guard reply.isAccepted, dialect.supportsOperations, let id = operationId ?? reply.operationId else {
            return Conclusion(result: reply, operation: reply.op, replayed: reply.replayed == true, stillRunning: false)
        }

        if let op = reply.op { onProgress(op) }

        let deadline = Date().addingTimeInterval(Self.watchLimit)
        var lastSeen: AgentOperation? = reply.op

        while Date() < deadline {
            if Task.isCancelled {
                return Conclusion(result: reply, operation: lastSeen, replayed: false, stillRunning: true)
            }
            let poll = try await readOperation(try AgentRequest.operation(id: id, wait: Self.pollSeconds))
            guard let op = poll.operation else {
                // The agent answered and has no record of the id. That is an answer, not a gap: an
                // agent that files every operation it starts never started this one.
                return Conclusion(result: reply, operation: nil, replayed: false, stillRunning: false)
            }
            lastSeen = op
            onProgress(op)
            if op.isFinished {
                return Conclusion(result: Self.finalResult(from: op, accepted: reply),
                                  operation: op,
                                  replayed: reply.replayed == true,
                                  stillRunning: false)
            }
        }

        return Conclusion(result: reply, operation: lastSeen, replayed: false, stillRunning: true)
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
        result.op = operation
        return result
    }
}

/// What an agent reply means for the operation record this app keeps.
///
/// One function, used by both models, so the local Mac and a remote machine can never disagree about
/// what "deferred" or "conflict" does to the history.
enum OperationOutcome {
    static func state(for result: AgentActionResult, stillRunning: Bool) -> OperationState {
        if stillRunning { return .running }
        switch result.action {
        case "updated", "restarted", "rebooting", "rebooted", "armed", "sleeping", "slept",
             "ran", "cycled":
            return .succeeded
        case "noop", "skipped":
            return .noop
        case "deferred":
            return .deferred
        case "queued":
            return .queued
        case "cancelled":
            return .cancelled
        case "expired":
            return .expired
        case "conflict":
            return .conflict
        case "interrupted":
            return .unknown
        case "accepted":
            // Accepted with nothing following it means the app stopped watching, which is a running
            // operation and not a finished one.
            return .running
        case "installed":
            return result.didInstallAgent ? .succeeded : (result.ok == false ? .failed : .unknown)
        case "rolled-back":
            return result.ok == true && result.op?.kind == "self-update"
                && result.op?.target?.hasPrefix("rollback:") == true ? .succeeded : .failed
        case "failed":
            return .failed
        default:
            return result.ok == false ? .failed : .unknown
        }
    }
}
