import Foundation
import Testing
@testable import LegionControlCore

@MainActor
struct WakeActionTests {
    @Test("ambiguous helper action reconciles the same operation before failover")
    func reconcilesSameIdentity() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = OperationStore(url: dir.appending(path: "operations.json"))
        var sent: [String] = []
        var read: [String] = []
        let outcome = await WakeAction.run(action: "wake-tower", target: "Tower", machineId: "pi", machineName: "Pi",
                                           declaredPacket: false, dialect: .v3, operations: store,
                                           send: { request in
            sent = request.arguments
            throw AgentFailure(.linkLost, dispatch: .unknown)
        }, read: { request in
            read = request.arguments
            return try JSONDecoder().decode(AgentOperationResult.self, from: Data("""
            {"ok":true,"op":{"id":"\(request.arguments[1])","state":"finished","action":"ran","kind":"run","result":{"action":"ran","ok":true}}}
            """.utf8))
        })
        #expect(outcome.sent)
        #expect(read.first == "op")
        #expect(read[1] == sent.last)
        #expect(store.records.first?.id == read[1])
        #expect(store.records.first?.state == .succeeded)
        #expect(store.unresolved.isEmpty)
    }

    @Test("unreachable helper after ambiguous action retains an operation for later reconciliation")
    func durableUnknown() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = OperationStore(url: dir.appending(path: "operations.json"))
        let outcome = await WakeAction.run(action: "wake-tower", target: "Tower", machineId: "pi", machineName: "Pi",
                                           declaredPacket: false, dialect: .v3, operations: store,
                                           send: { _ in throw AgentFailure(.linkLost, dispatch: .unknown) },
                                           read: { _ in throw AgentFailure(.hostUnreachable, dispatch: .never) })
        #expect(!outcome.sent)
        #expect(store.unresolved.count == 1)
        #expect(!WakePlan.mayTryNextHelper(after: try #require(outcome.failure), actionIsDeclaredWakePacket: false))
        let reopened = OperationStore(url: dir.appending(path: "operations.json"))
        #expect(reopened.unresolved.first?.id == store.unresolved.first?.id)
    }
}
