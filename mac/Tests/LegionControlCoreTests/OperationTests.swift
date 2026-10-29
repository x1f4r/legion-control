import Foundation
import Testing
@testable import LegionControlCore

/// Operations that outlive the process that started them.
///
/// The point of the whole mechanism is the `unknown` state: before, a command that timed out left
/// nothing behind, so the app either invented an outcome or reported a failure that may have been a
/// success, and there was nothing afterwards to check against.
@MainActor
struct OperationTests {

    static func store() throws -> (OperationStore, URL) {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-ops-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let url = directory.appending(path: "operations.json")
        return (OperationStore(url: url), directory)
    }

    static func record(kind: OperationKind = .update, machineId: String = "pi",
                       agentTracked: Bool = true) -> OperationRecord {
        OperationRecord(id: AgentToken.newOperationId(), machineId: machineId, machineName: "Pi",
                        kind: kind, subject: "demo", summary: "Updating demo.", agentTracked: agentTracked)
    }

    @Test("a finished operation is written down and read back")
    func persistence() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        let record = Self.record()
        store.begin(record)
        store.finish(record.id, state: .succeeded, summary: "demo updated from 1 to 2.")

        let reopened = OperationStore(url: store.url)
        let restored = try #require(reopened.record(id: record.id))
        #expect(restored.state == .succeeded)
        #expect(restored.summary == "demo updated from 1 to 2.")
        #expect(reopened.persistenceProblem == nil)
    }

    @Test("an operation still running when the app closes becomes an unknown outcome, not a success")
    func runningBecomesUnknownOnReload() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        let record = Self.record()
        store.begin(record)   // left running

        let reopened = OperationStore(url: store.url)
        let restored = try #require(reopened.record(id: record.id))
        #expect(restored.state == .unknown)
        #expect(restored.needsReconciliation)
        #expect(reopened.unresolved.contains { $0.id == record.id })
    }

    @Test("a legacy bootstrap becomes tracked before the v3 installer starts")
    func bootstrapBecomesTracked() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let record = Self.record(kind: .appUpdate, agentTracked: false)
        store.begin(record)
        store.markAgentTracked(record.id)
        let reopened = OperationStore(url: store.url)
        #expect(reopened.unresolved.contains { $0.id == record.id })
        store.finish(record.id, state: .unknown, summary: "Installer outcome unknown")
        #expect(store.unresolved.contains { $0.id == record.id })
    }

    @Test("only a disruptive operation the agent was tracking is chased afterwards")
    func onlyTrackedDisruptiveWorkIsOwed() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        // A setting that may or may not have saved is answered by the next status; nobody has to be
        // asked about it.
        let setting = Self.record(kind: .autoUpdate)
        store.begin(setting)
        store.finish(setting.id, state: .unknown, summary: "unknown")
        #expect(store.record(id: setting.id)?.needsReconciliation == false)

        // An agent that never understood the id cannot be asked about it either.
        let untracked = Self.record(agentTracked: false)
        store.begin(untracked)
        store.finish(untracked.id, state: .unknown, summary: "unknown")
        #expect(store.record(id: untracked.id)?.needsReconciliation == false)

        let update = Self.record()
        store.begin(update)
        store.finish(update.id, state: .unknown, summary: "unknown")
        #expect(store.record(id: update.id)?.needsReconciliation == true)
    }

    @Test("what the agent eventually says replaces this app's guess")
    func reconcile() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        let record = Self.record()
        store.begin(record)
        store.finish(record.id, state: .unknown, summary: "The update may or may not have happened.")
        #expect(store.unresolved.count == 1)

        var operation = AgentOperation()
        operation.rawId = record.id
        operation.kind = "update"
        operation.state = "finished"
        operation.phase = "done"
        operation.result = AgentOperation.Result(ok: true, action: "updated", reasonCode: nil,
                                                 message: "demo updated from 1 to 2.", from: "1",
                                                 to: "2", exitCode: 0, output: nil)
        operation.log = [AgentOperation.LogLine(at: "2026-09-05T09:00:00Z", line: "stopped demo")]

        store.reconcile(record.id, with: operation)
        let settled = try #require(store.record(id: record.id))
        #expect(settled.state == .succeeded)
        #expect(settled.summary == "demo updated from 1 to 2.")
        #expect(!settled.needsReconciliation)
        #expect(settled.phases.contains { $0.detail == "stopped demo" })
        #expect(store.unresolved.isEmpty)
    }

    @Test("an agent with no record of the id means it never started")
    func neverStarted() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        let record = Self.record()
        store.begin(record)
        store.finish(record.id, state: .unknown, summary: "unknown")
        store.resolveAsNeverStarted(record.id)

        let settled = try #require(store.record(id: record.id))
        #expect(settled.state == .failed)
        #expect(!settled.needsReconciliation)
        #expect(settled.summary.contains("never reached"))
    }

    @Test("a queued request is not finished and is not chased")
    func queuedIsHeld() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        let record = Self.record()
        store.begin(record)
        let expiry = Date().addingTimeInterval(4 * 60 * 60)
        store.markQueued(record.id, expiresAt: expiry, summary: "queued until idle")

        let queued = try #require(store.record(id: record.id))
        #expect(queued.state == .queued)
        #expect(!queued.state.isFinished)
        #expect(!queued.needsReconciliation)
        #expect(store.queued.count == 1)

        // It lives on the machine, so reopening the app leaves it exactly as it was.
        let reopened = OperationStore(url: store.url)
        #expect(reopened.record(id: record.id)?.state == .queued)
    }

    @Test("the outcome decides the state, and the action only says which kind of it each one is")
    func outcomeMapping() {
        func state(_ action: String?, reason: String? = nil, ok: Bool? = nil, outcome: String? = nil) -> OperationState {
            var result = AgentActionResult()
            result.action = action
            result.reasonCode = reason
            result.ok = ok
            result.outcome = outcome
            return OperationOutcome.state(for: result, stillRunning: false)
        }
        #expect(state("updated") == .succeeded)
        #expect(state("noop") == .noop)
        #expect(state("deferred") == .deferred)
        #expect(state("queued") == .queued)
        #expect(state("conflict") == .conflict)
        #expect(state("expired") == .expired)
        #expect(state("cancelled", reason: "cancelled") == .cancelled)
        // A cancel that worked is a success of its own, not a cancelled request.
        #expect(state("cancelled") == .succeeded)
        #expect(state("rolled-back", reason: "rolled-back") == .failed)
        #expect(state("rolled-back") == .succeeded)
        // Interrupted is the agent saying it does not know either, and so is a command it stopped.
        #expect(state("interrupted") == .unknown)
        #expect(state("failed", reason: "timed-out") == .unknown)
        #expect(state("failed", reason: "apply-failed") == .failed)
        // A refusal that names its reason is a known failure, and a lock is a hold.
        #expect(state(nil, reason: "unknown-service", ok: false) == .failed)
        #expect(state(nil, reason: "lock-held", ok: false) == .deferred)
        // An action from a newer agent is no evidence either way, whatever ok says beside it: it
        // was a failure here, a success on the desktop and a failure on the phone (C5).
        #expect(state("something-new", ok: false) == .unknown)
        #expect(state("something-new", ok: true) == .unknown)
        #expect(state(nil) == .unknown)
        // The agent's own word comes first, and a word this build does not know is unknown.
        #expect(state("failed", reason: "apply-failed", ok: false, outcome: "unknown") == .unknown)
        #expect(state("updated", ok: true, outcome: "partial") == .unknown)
        #expect(state("accepted") == .running)
        #expect(state("already-running", reason: "already-running") == .running)
        #expect(OperationOutcome.state(for: AgentActionResult(), stillRunning: true) == .running)
    }

    @Test("signed agent installation and intentional rollback use the actual contract outcomes")
    func signedAgentOutcomes() throws {
        let reply = try JSONDecoder().decode(AgentActionResult.self, from: ContractCorpus.data("fixtures/self-update.installed.json"))
        #expect(reply.didInstallAgent)
        #expect(OperationOutcome.state(for: reply, stillRunning: false) == .succeeded)
        let installedOperation = try #require(reply.op)
        let replay = OperationDriver.finalResult(from: installedOperation, accepted: AgentActionResult())
        #expect(replay.didInstallAgent)
        #expect(OperationOutcome.state(for: replay, stillRunning: false) == .succeeded)
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        let record = Self.record(kind: .appUpdate)
        store.begin(record)
        store.finish(record.id, state: .unknown, summary: "Awaiting installer result")
        store.reconcile(record.id, with: installedOperation)
        #expect(store.record(id: record.id)?.state == .succeeded)

        // Back on the kept agent and nothing went wrong: a success, whichever way it was asked for.
        let rollback = try JSONDecoder().decode(AgentActionResult.self, from: ContractCorpus.data("fixtures/self-update.rolled-back.json"))
        #expect(!rollback.didInstallAgent)
        #expect(OperationOutcome.state(for: rollback, stillRunning: false) == .succeeded)
        // An install that was put back reads as the failure its reason code says it was.
        var putBack = rollback
        putBack.outcome = nil
        putBack.reasonCode = "rolled-back"
        #expect(OperationOutcome.state(for: putBack, stillRunning: false) == .failed)
        // And a stated outcome wins over the action beside it.
        var invalid = reply
        invalid.ok = false
        invalid.outcome = "failed"
        #expect(!invalid.didInstallAgent)
        #expect(OperationOutcome.state(for: invalid, stillRunning: false) == .failed)
        invalid.ok = true; invalid.action = "checked"
        #expect(!invalid.didInstallAgent)
    }

    // MARK: - Following an operation

    static func reply(_ json: String) throws -> AgentActionResult {
        try JSONDecoder().decode(AgentActionResult.self, from: Data(json.utf8))
    }

    static func poll(_ json: String) throws -> AgentOperationResult {
        try JSONDecoder().decode(AgentOperationResult.self, from: Data(json.utf8))
    }

    static let opId = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"

    static let accepted = """
    { "ok": true, "contract": 3, "action": "accepted", "detached": true,
      "op": { "id": "\(opId)", "kind": "update", "service": "demo", "state": "running", "phase": "installing" } }
    """

    static let finished = """
    { "ok": true, "contract": 3,
      "op": { "id": "\(opId)", "kind": "update", "service": "demo", "state": "finished", "phase": "done",
              "result": { "ok": true, "action": "updated", "from": "1", "to": "2" } } }
    """

    @Test("a poll that does not get through keeps the operation running, and the next one finishes it")
    func failedPollKeepsTheLastState() async throws {
        // One failed reconnect used to be thrown back as the failure of the whole update, and a read
        // that never left this device reads as "never sent", so an update running perfectly well
        // over there was filed here as one that did not happen (C5).
        var polls = 0
        var pauses = 0
        var driver = OperationDriver(send: { _ in try Self.reply(Self.accepted) }, readOperation: { _ in
            polls += 1
            if polls == 1 { throw AgentFailure(.hostUnreachable, dispatch: .never) }
            return try Self.poll(Self.finished)
        })
        driver.pause = { _ in pauses += 1 }
        let request = try AgentRequest.update(service: "demo", intent: .init(detach: true, operationId: Self.opId), dialect: .v3)
        let conclusion = try await driver.perform(request, operationId: Self.opId, dialect: .v3)
        #expect(!conclusion.stillRunning)
        #expect(conclusion.result.action == "updated")
        #expect(Outcome.of(conclusion.result) == .succeeded)
        #expect(polls == 2)
        #expect(pauses == 1)
    }

    @Test("a link that stays down leaves the operation pending, never failed")
    func pollsThatKeepFailingLeaveItPending() async throws {
        var polls = 0
        var driver = OperationDriver(send: { _ in try Self.reply(Self.accepted) }, readOperation: { _ in
            polls += 1
            throw AgentFailure(.noRoute, dispatch: .never)
        })
        driver.pause = { _ in }
        let request = try AgentRequest.update(service: "demo", intent: .init(detach: true, operationId: Self.opId), dialect: .v3)
        let conclusion = try await driver.perform(request, operationId: Self.opId, dialect: .v3)
        #expect(conclusion.stillRunning)
        #expect(OperationOutcome.state(for: conclusion.result, stillRunning: conclusion.stillRunning) == .running)
        #expect(conclusion.operation?.phase == "installing")
        #expect(polls == OperationDriver.pollTries)
    }

    @Test("a replay of a request still running is followed by its id like an accepted one")
    func replayStillRunningIsFollowed() async throws {
        let replay = try String(decoding: ContractCorpus.data("fixtures/update.replayed.json"), as: UTF8.self)
        let id = "6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3"
        var asked: [[String]] = []
        let driver = OperationDriver(send: { _ in try Self.reply(replay) }, readOperation: { request in
            asked.append(request.arguments)
            return try Self.poll(Self.finished.replacingOccurrences(of: Self.opId, with: id))
        })
        let request = try AgentRequest.update(service: "t3", intent: .init(detach: true, operationId: id), dialect: .v3)
        let conclusion = try await driver.perform(request, operationId: id, dialect: .v3)
        #expect(asked == [["op", id, "--wait", "20"]])
        #expect(conclusion.replayed)
        #expect(conclusion.result.action == "updated")
        #expect(!conclusion.stillRunning)
    }

    @Test("pending work that is not the agent's to report on yet is not polled")
    func queuedAndTransitionsAreNotPolled() async throws {
        for fixture in ["update.queued.json", "boot.rebooting.json", "sleep.sleeping.json"] {
            let text = try String(decoding: ContractCorpus.data("fixtures/\(fixture)"), as: UTF8.self)
            var polls = 0
            let driver = OperationDriver(send: { _ in try Self.reply(text) }, readOperation: { _ in
                polls += 1
                return try Self.poll(Self.finished)
            })
            let request = try AgentRequest.update(service: "demo", intent: .init(operationId: Self.opId), dialect: .v3)
            let conclusion = try await driver.perform(request, operationId: Self.opId, dialect: .v3)
            #expect(Outcome.of(conclusion.result) == .pending, "\(fixture)")
            #expect(!conclusion.stillRunning, "\(fixture)")
            #expect(polls == 0, "\(fixture)")
        }
    }

    @Test("an agent that took a request and then has no record of it leaves the outcome unknown")
    func lostRecordIsUnknown() async throws {
        let driver = OperationDriver(send: { _ in try Self.reply(Self.accepted) }, readOperation: { _ in
            try Self.poll(#"{"ok":false,"contract":3,"reasonCode":"not-configured","op":null}"#)
        })
        let request = try AgentRequest.update(service: "demo", intent: .init(detach: true, operationId: Self.opId), dialect: .v3)
        let conclusion = try await driver.perform(request, operationId: Self.opId, dialect: .v3)
        #expect(!conclusion.stillRunning)
        #expect(OperationOutcome.state(for: conclusion.result, stillRunning: false) == .unknown)
    }

    @Test("only a present op null is the agent saying it has no record, whatever the code")
    func unknownOperation() throws {
        // Agent 3.1.0 answers not-configured, 3.0.x answered bad-argument, and both mean the same.
        #expect(try Self.poll(#"{"ok":false,"contract":3,"reasonCode":"not-configured","op":null}"#).isUnknownOperation)
        #expect(try Self.poll(#"{"ok":false,"contract":3,"reasonCode":"bad-argument","command":"op","op":null}"#).isUnknownOperation)
        // A refusal in the error shape carries no op at all, and says nothing about the record.
        #expect(!(try Self.poll(#"{"ok":false,"contract":3,"reasonCode":"restricted","command":"op","error":"denied"}"#).isUnknownOperation))
        #expect(!(try Self.poll(Self.finished).isUnknownOperation))
    }

    @Test("the history exports as text with the detail and the output in it")
    func export() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        let record = Self.record(kind: .action)
        store.begin(record)
        store.finish(record.id, state: .succeeded, summary: "sunshine-restart ran.",
                     detail: "exit 0", output: "Restarted sunshine.service")

        let text = store.exportText()
        #expect(text.contains("sunshine-restart ran."))
        #expect(text.contains("Restarted sunshine.service"))
        #expect(text.contains("succeeded"))
    }

    @Test("a failed write is reported rather than dropped")
    func persistenceFailureIsVisible() throws {
        // A path inside a file, which cannot be a directory. The old store discarded write failures
        // outright, so a history that was not being kept looked exactly like one that was.
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-ops-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let blocker = directory.appending(path: "blocker")
        try Data("not a directory".utf8).write(to: blocker)

        let store = OperationStore(url: blocker.appending(path: "operations.json"))
        store.begin(Self.record())
        #expect(store.persistenceProblem != nil)
    }

    @Test("concurrent writers do not lose each other's records")
    func concurrentWrites() async throws {
        // The review found the agent's own state file losing writes this way. The same shape of
        // mistake here would lose exactly the records that exist to answer "did that update run".
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        var ids: [String] = []
        for index in 0..<40 {
            let record = Self.record(machineId: "machine-\(index % 4)")
            ids.append(record.id)
            store.begin(record)
            store.finish(record.id, state: .succeeded, summary: "done \(index)")
        }

        let reopened = OperationStore(url: store.url)
        for id in ids {
            #expect(reopened.record(id: id) != nil, "\(id) was lost")
        }
    }

    @Test("the file is replaced atomically through a uniquely named temporary")
    func atomicWrite() throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-atomic-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let target = directory.appending(path: "thing.json")

        try AtomicFile.write(Data("one".utf8), to: target)
        #expect(try String(contentsOf: target, encoding: .utf8) == "one")
        try AtomicFile.write(Data("two".utf8), to: target)
        #expect(try String(contentsOf: target, encoding: .utf8) == "two")

        // No temporary left behind for the next writer to collide with.
        let leftovers = try FileManager.default.contentsOfDirectory(atPath: directory.path)
            .filter { $0.hasSuffix(".tmp") }
        #expect(leftovers.isEmpty)
    }
}
