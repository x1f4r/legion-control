import Foundation

/// A helper action has its own durable identity, so a dropped connection can be reconciled without
/// executing a second command. Local and remote helpers follow exactly the same path.
@MainActor
enum WakeAction {
    static func run(action: String, target: String, machineId: String, machineName: String,
                    declaredPacket: Bool, dialect: AgentDialect, operations: OperationStore?,
                    send: @escaping @MainActor (AgentRequest) async throws -> AgentActionResult,
                    read: @escaping @MainActor (AgentRequest) async throws -> AgentOperationResult) async -> MachineModel.WakeActionOutcome {
        if !declaredPacket, let previous = operations?.unresolved.first(where: { $0.machineId == machineId && $0.subject == action && $0.kind == .action }) {
            if let reply = try? await read(try AgentRequest.operation(id: previous.id, wait: nil)) {
                if let operation = reply.operation, operation.isFinished {
                    operations?.reconcile(previous.id, with: operation)
                    let final = OperationDriver.finalResult(from: operation, accepted: AgentActionResult())
                    if final.action == "ran", final.ok != false {
                        return .init(sent: true, summary: "The previous helper request completed.", failure: nil,
                                     isDeclaredWakePacket: false)
                    }
                } else if reply.isUnknownOperation {
                    operations?.resolveAsNeverStarted(previous.id)
                }
            }
            if operations?.record(id: previous.id)?.needsReconciliation == true {
                let summary = "The previous \(action) request still has an unknown outcome; it was not repeated."
                return .init(sent: false, summary: summary,
                             failure: AgentFailure(.agentFailed, detail: summary, dispatch: .unknown),
                             isDeclaredWakePacket: false)
            }
        }
        let id = AgentToken.newOperationId()
        operations?.begin(OperationRecord(id: id, machineId: machineId, machineName: machineName,
                                          kind: .action, subject: action,
                                          summary: "Asking \(machineName) to wake \(target).",
                                          agentTracked: dialect.supportsOperations))
        func result(_ result: AgentActionResult, running: Bool = false) -> MachineModel.WakeActionOutcome {
            let state = OperationOutcome.state(for: result, stillRunning: running)
            let summary = result.output?.trimmingCharacters(in: .whitespacesAndNewlines)
                .nilIfEmpty ?? result.message ?? "\(action): \(state.title)"
            operations?.finish(id, state: state, summary: summary,
                               agentTracked: dialect.supportsOperations, forceUnresolved: running)
            let uncertain = state == .running || state == .unknown || state == .queued
            return .init(sent: state == .succeeded && result.action == "ran", summary: summary,
                         failure: uncertain ? AgentFailure(.agentFailed, detail: summary, dispatch: .acknowledged) : nil,
                         isDeclaredWakePacket: declaredPacket)
        }
        do {
            let request = try AgentRequest.run(action: action, intent: .init(operationId: id), dialect: dialect)
            let conclusion = try await OperationDriver(send: send, readOperation: read).perform(request, operationId: id, dialect: dialect)
            return result(conclusion.result, running: conclusion.stillRunning)
        } catch {
            let failure = error as? AgentFailure
                ?? AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown)
            if failure.dispatch != .never, dialect.supportsOperations,
               let reply = try? await read(try AgentRequest.operation(id: id, wait: nil)) {
                if let operation = reply.operation, operation.isFinished {
                    return result(OperationDriver.finalResult(from: operation, accepted: AgentActionResult()))
                }
                if reply.isUnknownOperation {
                    operations?.resolveAsNeverStarted(id)
                    return .init(sent: false, summary: "The helper has no record of the request.", failure: nil,
                                 isDeclaredWakePacket: declaredPacket)
                }
            }
            operations?.finish(id, state: failure.dispatch == .never ? .failed : .unknown,
                               summary: failure.shortReason, detail: failure.detail,
                               agentTracked: dialect.supportsOperations)
            return .init(sent: false, summary: failure.shortReason, failure: failure,
                         isDeclaredWakePacket: declaredPacket)
        }
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
