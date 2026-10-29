import Foundation

/// What became of a request, in the five words every client decides on (section 5.6 of the contract).
///
/// Agent 3.1.0 states it on every mutating reply, every operation record and every summary. Older
/// agents do not, and before this the three clients each guessed it from `ok`, `action` and
/// `reasonCode`, and guessed differently: a refusal with a code was a failure here and unknown on
/// the desktop, and an action nobody had written a case for was unknown here, a success there and a
/// failure on the phone. The guess is now the contract's own reducer, ported line for line from
/// `contract/tools/outcome.mjs` and held to it by the shared outcome vectors.
///
/// Two rules shape it. A reduction never invents optimism: where the evidence does not say, the
/// answer is `unknown`, and `unknown` is never sent again on its own. And `pending` belongs only to
/// work the agent still owns, because it is the one outcome a client goes on polling.
enum Outcome: String, Sendable, Equatable, CaseIterable {
    /// Finished, and what was asked for holds, "it already did" included.
    case succeeded
    /// Finished, not in effect, and the state is known. A new attempt is a new request.
    case failed
    /// Finished without acting, by design: busy, held back by policy, or refused by a lock.
    case deferred
    /// The agent still owns the work. Followed with `op ID --wait 20`, and nothing that happens to
    /// the link while it is followed turns it into anything else.
    case pending
    /// No evidence either way. Never sent again automatically: the same id is read, never resent.
    case unknown

    /// The fields the reduction reads, wherever they came from: a reply, a record read in the light
    /// of its result, a summary or a cycle child.
    struct Evidence: Decodable, Sendable, Equatable {
        var outcome: String?
        var state: String?
        var ok: Bool?
        var action: String?
        var reasonCode: String?
    }

    private static let pendingActions: Set<String> = ["accepted", "already-running", "queued", "rebooting", "sleeping"]
    private static let successActions: Set<String> = [
        "updated", "restarted", "ran", "rebooted", "slept", "armed", "installed",
        "cycled", "noop", "checked", "skipped", "stored", "replaced",
    ]
    private static let deferredActions: Set<String> = ["deferred", "conflict", "expired"]
    /// Codes after which nobody knows whether the work took effect.
    private static let unknownFailures: Set<String> = ["timed-out", "interrupted"]

    /// The reducer of section 5.6, step for step.
    static func reduce(_ x: Evidence) -> Outcome {
        // 1. A stated outcome wins, but only one this build knows. A value from a later contract is
        //    not a reason to believe anything.
        if let stated = x.outcome { return Outcome(rawValue: stated) ?? .unknown }

        // 2. Work the agent still owns is pending whatever else it says; a running record may
        //    already carry the action it is aiming at.
        if x.state == "queued" || x.state == "running" { return .pending }

        // 3. Otherwise the action decides, with the reason code where one action covers two
        //    different endings.
        guard let action = x.action else {
            switch x.ok {
            case true?:
                return x.reasonCode == nil ? .succeeded : .unknown
            case false?:
                if let code = x.reasonCode, ["operation-in-progress", "lock-held"].contains(code) { return .deferred }
                if let code = x.reasonCode, ["timed-out", "interrupted", "internal"].contains(code) { return .unknown }
                return .failed
            case nil:
                return .unknown
            }
        }
        if pendingActions.contains(action) { return .pending }
        if successActions.contains(action) { return .succeeded }
        if deferredActions.contains(action) { return .deferred }
        switch action {
        // A cancelled request was itself deferred; a cancel that worked succeeded.
        case "cancelled": return x.reasonCode == "cancelled" ? .deferred : .succeeded
        // The old version is back and running: a success only when nothing went wrong.
        case "rolled-back": return x.reasonCode == nil ? .succeeded : .failed
        case "interrupted": return .unknown
        case "failed": return x.reasonCode.map(unknownFailures.contains) == true ? .unknown : .failed
        default: return .unknown
        }
    }

    /// The outcome of a reply to a mutating command: the one it states, or the reduction of it.
    static func of(_ reply: AgentActionResult) -> Outcome {
        reduce(Evidence(outcome: reply.outcome, state: nil, ok: reply.ok, action: reply.action, reasonCode: reply.reasonCode))
    }

    /// The outcome of an operation, as a record or as the summary `status` and `history` carry.
    ///
    /// A record is read from its result in the light of its state, with the outcome the record
    /// states before the one its result does: `{ ...op.result, state: op.state, outcome: op.outcome
    /// ?? op.result.outcome }`. A summary carries the result's fields flat beside its own and has no
    /// result at all, so the same expression read off those fields is the summary's own reduction.
    static func of(_ operation: AgentOperation) -> Outcome {
        guard let result = operation.result else {
            return reduce(Evidence(outcome: operation.outcome, state: operation.state, ok: nil,
                                   action: operation.action, reasonCode: operation.reasonCode))
        }
        return reduce(Evidence(outcome: operation.outcome ?? result.outcome, state: operation.state, ok: result.ok,
                               action: result.action, reasonCode: result.reasonCode))
    }

    /// The outcome of one service's part in a cycle.
    static func of(_ child: AgentOperation.Child) -> Outcome {
        reduce(Evidence(outcome: child.outcome, state: nil, ok: nil, action: child.action, reasonCode: child.reasonCode))
    }

    /// Whether polling stops here. Only `pending` is followed.
    var isTerminal: Bool { self != .pending }
}
