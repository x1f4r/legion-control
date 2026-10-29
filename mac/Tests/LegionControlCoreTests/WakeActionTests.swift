import Foundation
import Testing
@testable import LegionControlCore

/// A helper's wake action, and the wake hold an ambiguous one leaves behind (section 10.2).
@MainActor
struct WakeActionTests {
    static func store() -> (OperationStore, URL) {
        let directory = FileManager.default.temporaryDirectory.appending(path: "legion-wake-\(UUID().uuidString)")
        return (OperationStore(url: directory.appending(path: "operations.json")), directory)
    }

    static func operation(_ json: String) throws -> AgentOperationResult {
        try JSONDecoder().decode(AgentOperationResult.self, from: Data(json.utf8))
    }

    static func run(declared: Bool = false, store: OperationStore,
                    send: @escaping @MainActor (AgentRequest) async throws -> AgentActionResult,
                    read: @escaping @MainActor (AgentRequest) async throws -> AgentOperationResult) async -> WakeActionOutcome {
        await WakeAction.run(action: "wake-tower", target: "Tower", targetId: "tower", machineId: "pi", machineName: "Pi",
                             declaredPacket: declared, dialect: .v3, operations: store, send: send, read: read)
    }

    @Test("an ambiguous command reads its own record once, by the same id, and a finished one ends the hold")
    func reconcilesSameIdentity() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        var sent: [String] = []
        var read: [String] = []
        let outcome = await Self.run(store: store, send: { request in
            sent = request.arguments
            throw AgentFailure(.linkLost, dispatch: .unknown)
        }, read: { request in
            read = request.arguments
            return try Self.operation("""
            {"ok":true,"op":{"id":"\(request.arguments[1])","state":"finished","kind":"run","result":{"action":"ran","ok":true}}}
            """)
        })
        #expect(outcome.sent)
        #expect(!outcome.isAmbiguous)
        // Read once, waiting up to twenty seconds for it, and never sent again.
        #expect(read == ["op", sent.last!, "--wait", "20"])
        #expect(store.records.first?.id == read[1])
        #expect(store.records.first?.state == .succeeded)
        #expect(store.wakeHolds(target: "tower").isEmpty)
        #expect(store.unresolved.isEmpty)
    }

    @Test("an unreachable helper after an ambiguous command leaves a hold that outlives the app")
    func durableHold() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let outcome = await Self.run(store: store,
                                     send: { _ in throw AgentFailure(.linkLost, dispatch: .unknown) },
                                     read: { _ in throw AgentFailure(.hostUnreachable, dispatch: .never) })
        #expect(!outcome.sent)
        #expect(outcome.isAmbiguous)
        let hold = try #require(store.wakeHolds(target: "tower").first)
        #expect(hold.machineId == "pi")
        #expect(hold.subject == "wake-tower")
        #expect(store.unresolved.count == 1)
        // A restart releases nothing.
        let reopened = OperationStore(url: directory.appending(path: "operations.json"))
        #expect(reopened.wakeHolds(target: "tower").map(\.id) == [hold.id])
    }

    @Test("a reply still running when this app stops watching is ambiguous too")
    func stillRunningIsHeld() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let outcome = await Self.run(store: store, send: { request in
            try JSONDecoder().decode(AgentActionResult.self, from: Data("""
            {"ok":true,"contract":3,"action":"queued","op":{"id":"\(request.arguments.last!)","state":"queued","kind":"run"}}
            """.utf8))
        }, read: { request in
            try Self.operation(#"{"ok":true,"op":{"id":"\#(request.arguments[1])","state":"queued","kind":"run"}}"#)
        })
        #expect(outcome.isAmbiguous)
        #expect(store.wakeHolds(target: "tower").count == 1)
        #expect(store.records.first?.state == .running)
    }

    @Test("an answer that says nobody knows whether the command ran keeps the hold, and only its finished record ends it")
    func unknownAnswerIsHeld() async throws {
        // The agent failed while it wrote the record, and says the outcome is unknown. Filed as an
        // answer, that released the hold and the next helper's command ran beside this one, which
        // may well be running (section 10.2).
        let internalError = try ContractCorpus.data("fixtures/error.internal.json")
        // A 3.0.x agent states no outcome; `internal` reduces to the same unknown.
        let olderAgent = Data(#"{"ok":false,"contract":3,"reasonCode":"internal","message":"the agent failed"}"#.utf8)
        for reply in [internalError, olderAgent] {
            let (store, directory) = Self.store()
            defer { try? FileManager.default.removeItem(at: directory) }
            var reads: [[String]] = []
            let outcome = await Self.run(store: store, send: { _ in
                try JSONDecoder().decode(AgentActionResult.self, from: reply)
            }, read: { request in
                reads.append(request.arguments)
                return try Self.operation(#"{"ok":true,"op":{"id":"\#(request.arguments[1])","state":"running","kind":"run"}}"#)
            })
            #expect(!outcome.sent)
            #expect(outcome.isAmbiguous)
            #expect(store.wakeHolds(target: "tower").count == 1)
            #expect(store.records.first?.state == .unknown)
            #expect(store.unresolved.count == 1)
            #expect(outcome.summary.hasPrefix("wake-tower may or may not have run: the agent failed"))
            // Its own record is read once, by its id; the command is never sent again.
            #expect(reads == [["op", store.records.first!.id, "--wait", "20"]])
        }

        // The same answer, and the record it reads afterwards has finished: that ends the hold,
        // whatever the result.
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let settled = await Self.run(store: store, send: { _ in
            try JSONDecoder().decode(AgentActionResult.self, from: internalError)
        }, read: { request in
            try Self.operation("""
            {"ok":true,"op":{"id":"\(request.arguments[1])","state":"finished","kind":"run","result":{"ok":true,"action":"ran"}}}
            """)
        })
        #expect(settled.sent)
        #expect(!settled.isAmbiguous)
        #expect(store.wakeHolds(target: "tower").isEmpty)
    }

    @Test("a record lost while it was followed is read once more, and the helper having none ends the hold")
    func lostRecordIsReadOnceMore() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        var reads = 0
        let outcome = await Self.run(store: store, send: { request in
            try JSONDecoder().decode(AgentActionResult.self, from: Data("""
            {"ok":true,"contract":3,"action":"accepted","outcome":"pending",
             "op":{"id":"\(request.arguments.last!)","state":"running","kind":"run","outcome":"pending"}}
            """.utf8))
        }, read: { _ in
            reads += 1
            return try JSONDecoder().decode(AgentOperationResult.self, from: try ContractCorpus.data("fixtures/op.not-found.json"))
        })
        // Once by the poll, which learns the record is gone, and once more for the hold, which the
        // same answer ends (section 10.2).
        #expect(reads == 2)
        #expect(!outcome.sent)
        #expect(!outcome.isAmbiguous)
        #expect(store.wakeHolds(target: "tower").isEmpty)
    }

    @Test("a stated unknown that a finished record backs is final, and ends the hold at once")
    func finishedUnknownIsFinal() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        var reads = 0
        let outcome = await Self.run(store: store, send: { request in
            try JSONDecoder().decode(AgentActionResult.self, from: Data("""
            {"ok":false,"contract":3,"action":"failed","reasonCode":"timed-out","outcome":"unknown",
             "op":{"id":"\(request.arguments.last!)","state":"finished","kind":"run","outcome":"unknown",
                   "result":{"ok":false,"action":"failed","reasonCode":"timed-out","outcome":"unknown"}}}
            """.utf8))
        }, read: { _ in
            reads += 1
            throw AgentFailure(.hostUnreachable, dispatch: .never)
        })
        #expect(reads == 0)
        #expect(!outcome.isAmbiguous)
        #expect(store.wakeHolds(target: "tower").isEmpty)
        #expect(store.records.first?.state == .unknown)
    }

    @Test("a declared magic packet never holds anything, however it ends")
    func wolNeverHolds() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let outcome = await Self.run(declared: true, store: store,
                                     send: { _ in throw AgentFailure(.linkLost, dispatch: .unknown) },
                                     read: { _ in throw AgentFailure(.hostUnreachable, dispatch: .never) })
        #expect(!outcome.isAmbiguous)
        #expect(store.wakeHolds(target: "tower").isEmpty)
    }

    @Test("a terminal answer and a request that never left both end the hold at once")
    func terminalEndsTheHold() async throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let failed = await Self.run(store: store, send: { _ in
            try JSONDecoder().decode(AgentActionResult.self, from: Data(#"{"ok":false,"contract":3,"action":"failed","reasonCode":"not-running"}"#.utf8))
        }, read: { _ in throw AgentFailure(.hostUnreachable, dispatch: .never) })
        #expect(!failed.sent)
        #expect(!failed.isAmbiguous)
        let never = await Self.run(store: store,
                                   send: { _ in throw AgentFailure(.connectionRefused, dispatch: .never) },
                                   read: { _ in throw AgentFailure(.hostUnreachable, dispatch: .never) })
        #expect(!never.isAmbiguous)
        #expect(store.wakeHolds(target: "tower").isEmpty)
        #expect(store.records.allSatisfy { $0.phases.contains { $0.name == "wake hold released" } })
    }

    @Test("a hold is released by the helper's answer, the target answering, the helper leaving, or a person")
    func releases() throws {
        let (store, directory) = Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        func hold(_ helper: String, at date: Date = Date()) -> OperationRecord {
            store.begin(OperationRecord(machineId: helper, machineName: helper, kind: .action, subject: "wake-tower",
                                        startedAt: date, summary: "Asking", agentTracked: true, wakeTarget: "tower"))
        }
        let early = hold("pi", at: Date(timeIntervalSinceNow: -60))
        let late = hold("nas")
        store.releaseWakeHolds(target: "tower", before: Date(timeIntervalSinceNow: -30), reason: "Tower answered")
        #expect(store.wakeHolds(target: "tower").map(\.id) == [late.id])
        #expect(store.record(id: early.id)?.phases.last?.detail == "tower: Tower answered")

        store.releaseWakeHolds(onHelpersOtherThan: ["nas"], reason: "gone")
        #expect(store.wakeHolds(target: "tower").map(\.id) == [late.id])
        store.releaseWakeHolds(onHelpersOtherThan: ["pi"], reason: "gone")
        #expect(store.wakeHolds(target: "tower").isEmpty)

        let third = hold("pi")
        store.dismissReconciliation(third.id)
        #expect(store.wakeHolds(target: "tower").isEmpty)

        // The record finishing ends it, whatever the result; still running does not.
        let fourth = hold("pi")
        var running = AgentOperation()
        running.rawId = fourth.id
        running.state = "running"
        store.reconcile(fourth.id, with: running)
        #expect(store.wakeHolds(target: "tower").count == 1)
        running.state = "finished"
        running.result = AgentOperation.Result(ok: false, action: "failed", reasonCode: "apply-failed")
        store.reconcile(fourth.id, with: running)
        #expect(store.wakeHolds(target: "tower").isEmpty)

        let fifth = hold("pi")
        store.resolveAsNeverStarted(fifth.id)
        #expect(store.wakeHolds(target: "tower").isEmpty)
    }

    @Test("a hold is never trimmed from the history, however old")
    func holdsSurviveTrimming() throws {
        let directory = FileManager.default.temporaryDirectory.appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let url = directory.appending(path: "operations.json")
        // A history already as long as the store keeps, written once, with the hold the oldest.
        var old = OperationRecord(machineId: "pi", machineName: "Pi", kind: .action, subject: "wake-tower",
                                  startedAt: Date(timeIntervalSince1970: 0), state: .failed, summary: "Asking", wakeTarget: "tower")
        old.finishedAt = old.startedAt
        let newer = (0..<410).map { index in
            OperationRecord(machineId: "pi", machineName: "Pi", kind: .policy, subject: "\(index)",
                            startedAt: Date(timeIntervalSince1970: Double(1000 + index)), state: .succeeded, summary: "x")
        }
        try JSONEncoder().encode([old] + newer).write(to: url)
        let store = OperationStore(url: url)
        store.begin(OperationRecord(machineId: "pi", machineName: "Pi", kind: .policy, subject: "one more", summary: "x"))
        #expect(store.records.count < 412)
        #expect(store.wakeHolds(target: "tower").map(\.id) == [old.id])
    }
}
