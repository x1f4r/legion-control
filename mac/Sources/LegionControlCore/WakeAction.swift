import Foundation

/// A helper action has its own durable identity, so a dropped connection can be reconciled without
/// executing a second command. Local and remote helpers follow exactly the same path.
///
/// An action that is not a declared `wol` is written down as a wake hold for its target before it
/// is sent (section 10.2), so a run that ends ambiguously — a dropped link, a timeout, an unexplained
/// ssh failure after dispatch, a reply still running when this app stops watching — leaves the hold
/// behind, on disk, and no other helper runs a command for that target until something settles it.
/// A terminal answer ends the hold at once, and a run that never left this device never had one.
///
/// A reply is not a terminal answer just because it came back. One whose outcome is `unknown` —
/// the agent failing while it wrote the record, an older agent's `internal`, a record that vanished
/// while it was followed — says in so many words that nobody knows whether the command ran, and
/// is ambiguous until a finished record says otherwise.
@MainActor
enum WakeAction {
    static func run(action: String, target: String, targetId: String, machineId: String, machineName: String,
                    declaredPacket: Bool, dialect: AgentDialect, operations: OperationStore?,
                    send: @escaping @MainActor (AgentRequest) async throws -> AgentActionResult,
                    read: @escaping @MainActor (AgentRequest) async throws -> AgentOperationResult,
                    redial: @escaping @MainActor () -> Void = {}) async -> WakeActionOutcome {
        let id = AgentToken.newOperationId()
        operations?.begin(OperationRecord(id: id, machineId: machineId, machineName: machineName,
                                          kind: .action, subject: action,
                                          summary: "Asking \(machineName) to wake \(target).",
                                          agentTracked: dialect.supportsOperations,
                                          wakeTarget: declaredPacket ? nil : targetId))

        /// The helper answered, and how it went is known: nothing of this run is still going.
        func settled(_ result: AgentActionResult) -> WakeActionOutcome {
            let outcome = Outcome.of(result)
            let state = OperationOutcome.state(outcome, action: result.action)
            let summary = result.output?.trimmingCharacters(in: .whitespacesAndNewlines)
                .nilIfEmpty ?? result.message ?? "\(action): \(state.title)"
            operations?.finish(id, state: state, summary: summary, agentTracked: dialect.supportsOperations)
            operations?.releaseWakeHold(id: id, reason: "\(machineName) answered")
            return .init(sent: outcome == .succeeded && result.action == "ran", summary: summary, failure: nil,
                         isDeclaredWakePacket: declaredPacket, isAmbiguous: false)
        }

        /// The run may have been dispatched and no terminal record has been read. On a helper that
        /// keeps records, its record is read once, waiting up to twenty seconds for it to finish,
        /// and only a finished record or the helper having none ends the hold.
        func ambiguous(_ failure: AgentFailure, running: Bool, summary said: String? = nil) async -> WakeActionOutcome {
            if dialect.supportsOperations,
               let reply = try? await read(try AgentRequest.operation(id: id, wait: OperationDriver.pollSeconds)) {
                if let operation = reply.operation, operation.isFinished {
                    operations?.reconcile(id, with: operation)
                    return settled(OperationDriver.finalResult(from: operation, accepted: AgentActionResult()))
                }
                if reply.isUnknownOperation {
                    operations?.resolveAsNeverStarted(id)
                    return .init(sent: false, summary: "The helper has no record of the request.", failure: nil,
                                 isDeclaredWakePacket: declaredPacket, isAmbiguous: false)
                }
            }
            let summary = running
                ? "\(action) is still running on \(machineName), so how it ends is not known yet."
                : said ?? failure.shortReason
            operations?.finish(id, state: running ? .running : .unknown, summary: summary,
                               detail: running ? nil : failure.detail,
                               agentTracked: dialect.supportsOperations, forceUnresolved: running)
            return .init(sent: false, summary: summary, failure: failure, isDeclaredWakePacket: declaredPacket,
                         isAmbiguous: !declaredPacket)
        }

        do {
            let request = try AgentRequest.run(action: action, intent: .init(operationId: id), dialect: dialect)
            var driver = OperationDriver(send: send, readOperation: read)
            driver.beforeRetry = redial
            let conclusion = try await driver.perform(request, operationId: id, dialect: dialect)
            let outcome = Outcome.of(conclusion.result)
            if conclusion.stillRunning || outcome == .pending {
                let summary = "\(action) is still running on \(machineName)."
                return await ambiguous(AgentFailure(.agentFailed, detail: summary, dispatch: .acknowledged), running: true)
            }
            // An answer that says nobody knows whether the command ran is no ending. Filed as one,
            // it released the hold, and the next helper's command ran while this one might still
            // be running. Only a finished record makes an unknown final.
            if outcome == .unknown, conclusion.operation?.isFinished != true {
                let result = conclusion.result
                let said = result.message ?? result.reason?.sentence(subject: action, message: nil)
                let summary = "\(action) may or may not have run\(said.map { ": \($0)" } ?? ".")"
                return await ambiguous(AgentFailure(.agentFailed, detail: said ?? "", dispatch: .acknowledged),
                                       running: false, summary: summary)
            }
            return settled(conclusion.result)
        } catch {
            let failure = (error as? AgentFailure)
                ?? (error as? AgentRequest.Invalid).map { AgentFailure(.badRequest, detail: $0.message, dispatch: .never) }
                ?? AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown)
            guard failure.dispatch != .never else {
                // Nothing left this device, so nothing can be running.
                operations?.finish(id, state: .failed, summary: failure.shortReason, detail: failure.detail,
                                   agentTracked: dialect.supportsOperations)
                operations?.releaseWakeHold(id: id, reason: "the request never left this device")
                return .init(sent: false, summary: failure.shortReason, failure: failure,
                             isDeclaredWakePacket: declaredPacket, isAmbiguous: false)
            }
            return await ambiguous(failure, running: false)
        }
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
