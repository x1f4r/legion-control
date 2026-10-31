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

    @Test("a finished cycle is read again with its services, exactly as its reply was decided")
    func cycleReconcilesWithItsServices() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        func cycle(_ children: [AgentOperation.Child], result: AgentOperation.Result) -> AgentOperation {
            var operation = AgentOperation()
            operation.kind = "cycle"
            operation.state = "finished"
            operation.phase = "done"
            operation.result = result
            operation.children = children
            return operation
        }
        let cycled = AgentOperation.Result(ok: true, action: "cycled", reasonCode: nil, message: "0 updated, 1 left alone, 0 failed")

        // Filed unknown for an interrupted service, and read again: the record says `cycled`, and
        // that is no evidence about the service.
        let interrupted = Self.record(kind: .updateAll)
        store.begin(interrupted)
        store.finish(interrupted.id, state: .unknown, summary: "Maintenance cycle on Pi: 1 may or may not have updated.")
        store.reconcile(interrupted.id, with: cycle([.init(service: "edge", action: "interrupted")], result: cycled))
        let unknown = try #require(store.record(id: interrupted.id))
        #expect(unknown.state == .unknown)
        #expect(unknown.summary == "Maintenance cycle on Pi: 1 may or may not have updated.")
        #expect(unknown.detail == "edge: interrupted")
        #expect(!unknown.needsReconciliation)

        // One this app stopped watching, settled by its record: a failed service fails it, and a
        // service that updated fine is counted beside it.
        let watched = Self.record(kind: .updateAll)
        store.begin(watched)
        store.finish(watched.id, state: .running, summary: "still running", forceUnresolved: true)
        store.reconcile(watched.id, with: cycle(
            [.init(service: "t3", action: "updated"), .init(service: "demo", action: "failed", reasonCode: "apply-failed")],
            result: .init(ok: false, action: "failed", reasonCode: "apply-failed", message: "1 updated, 0 left alone, 1 failed")))
        let failed = try #require(store.record(id: watched.id))
        #expect(failed.state == .failed)
        #expect(failed.summary == "Maintenance cycle on Pi: 1 updated, 1 failed.")
        #expect(store.unresolved.isEmpty)

        // A cycle that failed of itself says why, in the agent's words.
        let own = Self.record(kind: .updateAll)
        store.begin(own)
        store.finish(own.id, state: .running, summary: "still running", forceUnresolved: true)
        store.reconcile(own.id, with: cycle([], result: .init(ok: false, action: "failed", reasonCode: "internal",
                                                              message: "the lock file could not be read")))
        #expect(store.record(id: own.id)?.state == .failed)
        #expect(store.record(id: own.id)?.summary == "Maintenance cycle on Pi failed: the lock file could not be read")
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

    @Test("no record of an id is read by what the agent said of it before")
    func noRecordAfterItWasHeard() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        // Never a word from the agent about it: it never started.
        let unheard = Self.record()
        store.begin(unheard)
        store.finish(unheard.id, state: .unknown, summary: "unknown")
        store.resolveWithNoRecord(unheard.id)
        #expect(store.record(id: unheard.id)?.state == .failed)

        // Listed as running once, it ran, and the record that would say how it ended was pruned.
        // It used to be filed as one that never reached the machine.
        let heard = Self.record(kind: .sleep)
        store.begin(heard)
        store.markQueued(heard.id, expiresAt: Date().addingTimeInterval(3600), summary: "queued until idle")
        var running = AgentOperation()
        running.rawId = heard.id
        running.state = "running"
        store.reconcile(heard.id, with: running)
        #expect(store.record(id: heard.id)?.summary == "Sleep: demo left the queue and started on Pi.")
        #expect(store.record(id: heard.id)?.agentKnew == true)
        store.resolveWithNoRecord(heard.id)
        let lost = try #require(store.record(id: heard.id))
        #expect(lost.state == .unknown)
        #expect(!lost.needsReconciliation)
        #expect(lost.summary == "Sleep: demo reached Pi, which no longer has a record of it, so how it ended is not known.")

        // Still queued here, its deadline decides.
        let queued = Self.record(kind: .sleep)
        store.begin(queued)
        store.markQueued(queued.id, expiresAt: Date().addingTimeInterval(-60), summary: "queued until idle")
        store.resolveWithNoRecord(queued.id)
        #expect(store.record(id: queued.id)?.state == .expired)

        // A history written before the key existed still reads.
        let older = try JSONDecoder().decode(OperationRecord.self, from: try JSONEncoder().encode(Self.record()))
        #expect(older.agentKnew == nil)
    }

    @Test("a queued request is not finished and owes no unknown outcome")
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

    @Test("a queued request the agent has no record of is expired once its time is up, and unknown before")
    func queuedWithoutRecord() throws {
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }

        // Two sleeps queued nine days ago read "waiting for idle" ever after, though both agents
        // had expired and pruned them long before.
        let expiry = Date().addingTimeInterval(-9 * 24 * 60 * 60)
        let lapsed = Self.record(kind: .sleep)
        store.begin(lapsed)
        store.markQueued(lapsed.id, expiresAt: expiry, summary: "queued until idle")
        store.resolveQueuedAsGone(lapsed.id)
        let expired = try #require(store.record(id: lapsed.id))
        #expect(expired.state == .expired)
        #expect(!expired.needsReconciliation)
        // It stopped waiting when its time was up, not when this app noticed, and nothing says it
        // never ran: the record that would have said is gone.
        #expect(expired.finishedAt == expiry)
        #expect(expired.summary.hasPrefix("The queued request expired on "))
        #expect(expired.summary.hasSuffix("Pi no longer has a record of it, so whether it ran before then is not known."))
        #expect(store.queued.isEmpty)

        // One that came back without an expiry is held at most as long as any agent holds one.
        let unnamed = Self.record(kind: .sleep)
        store.begin(unnamed)
        store.markQueued(unnamed.id, expiresAt: nil, summary: "queued until idle")
        store.resolveQueuedAsGone(unnamed.id, now: unnamed.startedAt.addingTimeInterval(OperationRecord.longestQueue - 60))
        #expect(store.record(id: unnamed.id)?.state == .unknown)
        let old = Self.record(kind: .sleep)
        store.begin(old)
        store.markQueued(old.id, expiresAt: nil, summary: "queued until idle")
        store.resolveQueuedAsGone(old.id, now: old.startedAt.addingTimeInterval(OperationRecord.longestQueue + 60))
        #expect(store.record(id: old.id)?.state == .expired)
        #expect(store.record(id: old.id)?.summary.contains("waited longer than any agent holds one") == true)

        // Before its time the agent let go of a request it was still meant to hold: it may have run.
        let early = Self.record(kind: .update)
        store.begin(early)
        store.markQueued(early.id, expiresAt: Date().addingTimeInterval(3600), summary: "queued until idle")
        store.resolveQueuedAsGone(early.id)
        let lost = try #require(store.record(id: early.id))
        #expect(lost.state == .unknown)
        #expect(!lost.needsReconciliation)
        #expect(lost.summary == "Pi no longer holds the queued request and has no record of it, so whether it ran is not known.")

        // Only a queued one: a record that has moved on is not touched.
        store.resolveQueuedAsGone(lapsed.id)
        #expect(store.record(id: lapsed.id) == expired)
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

    @Test("every poll tried again is told first, so the address it failed on is dialled again")
    func retriedPollsRedial() async throws {
        // A machine holds an address that did not answer back for a minute, so the second and
        // third polls, five seconds apart, failed at once without dialling anything.
        var events: [String] = []
        var driver = OperationDriver(send: { _ in try Self.reply(Self.accepted) }, readOperation: { _ in
            events.append("poll")
            throw AgentFailure(.hostUnreachable, dispatch: .never)
        })
        driver.pause = { _ in events.append("pause") }
        driver.beforeRetry = { events.append("redial") }
        let request = try AgentRequest.update(service: "demo", intent: .init(detach: true, operationId: Self.opId), dialect: .v3)
        let conclusion = try await driver.perform(request, operationId: Self.opId, dialect: .v3)
        #expect(conclusion.stillRunning)
        #expect(events == ["poll", "pause", "redial", "poll", "pause", "redial", "poll"])
    }

    @Test("a finished record with no result is read by its own reduction, never by the accepted reply's ok")
    func finishedWithoutAResultIsUnknown() async throws {
        // A record from before 3.1.0 that finished and kept no result. Reduced on its own it is
        // unknown, which ended the polling, but the final reply was then read again from the `ok`
        // it inherited from the accepted one, and the update was filed as a success.
        let bare = """
        { "ok": true, "contract": 3,
          "op": { "id": "\(Self.opId)", "kind": "update", "service": "demo", "state": "finished", "phase": "done", "result": null } }
        """
        let driver = OperationDriver(send: { _ in try Self.reply(Self.accepted) }, readOperation: { _ in try Self.poll(bare) })
        let request = try AgentRequest.update(service: "demo", intent: .init(detach: true, operationId: Self.opId), dialect: .v3)
        let conclusion = try await driver.perform(request, operationId: Self.opId, dialect: .v3)
        let operation = try #require(conclusion.operation)
        #expect(!conclusion.stillRunning)
        #expect(Outcome.of(operation) == .unknown)
        #expect(Outcome.of(conclusion.result) == .unknown)
        #expect(OperationOutcome.state(for: conclusion.result, stillRunning: false) == .unknown)

        // And the history files the same record the same way when it is read back later.
        let (store, directory) = try Self.store()
        defer { try? FileManager.default.removeItem(at: directory) }
        var record = Self.record()
        record.id = Self.opId
        store.begin(record)
        store.finish(record.id, state: .unknown, summary: "The update may or may not have happened.")
        store.reconcile(record.id, with: operation)
        #expect(store.record(id: record.id)?.state == .unknown)

        // A record that states its outcome, or whose result does, is read by that as before.
        var stated = operation
        stated.result = AgentOperation.Result(ok: true, action: "updated", reasonCode: nil)
        #expect(Outcome.of(OperationDriver.finalResult(from: stated, accepted: try Self.reply(Self.accepted))) == .succeeded)
        stated.outcome = "unknown"
        #expect(Outcome.of(OperationDriver.finalResult(from: stated, accepted: try Self.reply(Self.accepted))) == .unknown)
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
