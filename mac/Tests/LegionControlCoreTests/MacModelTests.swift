import Foundation
import Testing
@testable import LegionControlCore

/// The device the app runs on, driven through a fake local agent.
///
/// The point of these is parity: the local side used to have one Update button and nothing else,
/// which made the machine you are sitting in front of the one you could do least to. It now takes the
/// same commands with the same confirmations and the same refusals.
@MainActor
struct MacModelTests {

    final class FakeRunner: @unchecked Sendable {
        private let lock = NSLock()
        private var replies: [String: [CommandResult]] = [:]
        private(set) var argvs: [[String]] = []

        func answer(_ verb: String, json: String, exit: Int32 = 0) {
            lock.lock(); defer { lock.unlock() }
            replies[verb, default: []].append(CommandResult(exitCode: exit, standardOutput: json,
                                                            standardError: "", timedOut: false,
                                                            launchFailure: nil))
        }

        func answerTimeout(_ verb: String) {
            lock.lock(); defer { lock.unlock() }
            replies[verb, default: []].append(CommandResult(exitCode: -1, standardOutput: "",
                                                            standardError: "", timedOut: true,
                                                            launchFailure: nil))
        }

        func run(_ arguments: [String]) -> CommandResult {
            lock.lock(); defer { lock.unlock() }
            argvs.append(arguments)
            // The script path is the first argument; the verb is the next one.
            let verb = arguments.count > 1 ? arguments[1] : ""
            guard var queued = replies[verb], !queued.isEmpty else {
                return CommandResult(exitCode: 1, standardOutput: "", standardError: "no scripted reply for \(verb)",
                                     timedOut: false, launchFailure: nil)
            }
            let next = queued.removeFirst()
            replies[verb] = queued
            return next
        }

        var sent: [[String]] {
            lock.lock(); defer { lock.unlock() }
            return argvs
        }
    }

    /// A readable script file, so the agent's own "is it there" check passes.
    static func scriptFile() throws -> (URL, URL) {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-mac-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let script = directory.appending(path: "index.mjs")
        try Data("// stand-in".utf8).write(to: script)
        return (script, directory)
    }

    static func model(_ runner: FakeRunner) throws -> (MacModel, OperationStore, URL) {
        let (script, directory) = try scriptFile()
        // `/bin/echo` only has to exist: the runner never launches it.
        let agent = MacAgent(scriptPath: script.path(percentEncoded: false), interpreter: "/bin/echo") { _, arguments, _, _ in
            runner.run(arguments)
        }
        let config = LocalConfig(enabled: true, name: "This Mac", agent: script.path(percentEncoded: false))
        let model = MacModel(config: config, agent: agent)
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        model.operations = store
        return (model, store, directory)
    }

    static let status = """
    { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "mac", "name": "macOS" },
      "agent": { "version": "3.0.0", "contract": 3 },
      "services": [ { "id": "t3", "name": "T3 Code", "installed": "1", "latest": "2",
                      "upToDate": false, "canRestart": true,
                      "busy": { "busy": false, "unknown": false, "monitored": true } } ],
      "actions": [ { "id": "reindex", "name": "Reindex", "kind": "command" } ],
      "updates": { "automatic": true, "pauseUntil": null, "maintenanceWindows": [] } }
    """

    static func settle(_ model: MacModel, timeout: TimeInterval = 5) async {
        let deadline = Date().addingTimeInterval(timeout)
        while model.isWorking, Date() < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    @Test("bound self operations reconcile under the shared machine identity")
    func boundSelfReconciliation() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        model.boundMachineId = "workstation"
        let record = OperationTests.record(kind: .action, machineId: "workstation")
        store.begin(record)
        store.finish(record.id, state: .unknown, summary: "No result received")
        runner.answer("op", json: """
        {"ok":true,"op":{"id":"\(record.id)","requestedAt":"2026-09-05T00:00:00Z","state":"finished","result":{"ok":true,"action":"ran"}}}
        """)
        await model.refresh()
        let deadline = Date().addingTimeInterval(15)
        while !store.unresolved.isEmpty, Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }
        #expect(store.unresolved.isEmpty)
        #expect(store.record(id: record.id)?.state == .succeeded)
        #expect(runner.sent.contains { $0.contains("op") && $0.contains(record.id) })
    }

    @Test("a failed local status never preserves command readiness")
    func failedStatusIsNotReady() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        model.boundMachine = MachineModelTests.machine()
        await model.refresh()
        #expect(model.isReachable)
        runner.answer("status", json: #"{"ok":false,"contract":3,"reasonCode":"restricted","message":"command denied","services":[]}"#, exit: 1)
        await model.refresh()
        #expect(model.status == nil)
        #expect(!model.isReachable)
        #expect(model.failure?.dispatch == .never)
        let before = runner.sent.count
        model.boot(into: MachineModelTests.machine().systems[1], force: false, whenIdle: false)
        model.sleep(force: false, whenIdle: false)
        #expect(runner.sent.count == before)
    }

    @Test("this Mac's row says when its agent looks after nothing, rather than checking forever")
    func rowOfAnAgentWithNoServices() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "mac", "name": "macOS" },
          "agent": { "version": "3.1.0", "contract": 3 }, "services": [], "actions": [],
          "updates": { "automatic": false, "pauseUntil": null, "maintenanceWindows": [] } }
        """)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        #expect(model.sidebarSummary == "checking")
        await model.refresh()
        #expect(model.isReachable)
        #expect(model.sidebarSummary == "no services configured")
    }

    @Test("this Mac's row says when its agent is older than the one this app carries")
    func rowOfAnOlderAgent() async throws {
        let runner = FakeRunner()
        let status = """
        { "ok": true, "contract": 3, "agentVersion": "3.0.1", "system": { "id": "mac", "name": "macOS" },
          "agent": { "version": "3.0.1", "contract": 3 },
          "services": [ { "id": "t3", "name": "T3 Code", "installed": "2", "latest": "2", "upToDate": true,
                          "busy": { "busy": false, "unknown": false, "monitored": true } } ],
          "actions": [], "updates": { "automatic": false, "pauseUntil": null, "maintenanceWindows": [] } }
        """
        runner.answer("status", json: status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        model.core.bundledAgentVersion = "3.1.0"
        await model.refresh()
        #expect(model.sidebarSummary == "agent 3.1.0 available")
        #expect(model.newerBundledAgentLine == "Agent 3.0.1 is older than the 3.1.0 this app carries.")
        #expect(model.agentInstallRefusal == nil)
        model.core.bundledAgentVersion = "3.0.1"
        #expect(model.sidebarSummary == "up to date")

        // The install beside the line says why it cannot be pressed, as it does for a machine.
        runner.answer("status", json: status.replacingOccurrences(
            of: #""agent": { "version": "3.0.1", "contract": 3 }"#,
            with: #""agent": { "version": "3.0.1", "contract": 3, "restrictedSession": true }"#))
        await model.refresh()
        #expect(model.agentInstallRefusal == "The agent is installed from here only from an administrator session, and this one is restricted.")
    }

    @Test("a queued request that started stays in sight on this Mac's page, which draws nothing running")
    func startedQueuedRequestStaysInSight() async throws {
        // A machine's page lists what its status says is running, and leaves an owed record among
        // them out of the outcomes nobody knows. This page lists nothing running, so it keeps it.
        let runner = FakeRunner()
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let record = OperationRecord(id: AgentToken.newOperationId(), machineId: model.historyMachineId, machineName: "This Mac",
                                     kind: .sleep, subject: "This Mac", summary: "Putting This Mac to sleep.", agentTracked: true)
        store.begin(record)
        store.markQueued(record.id, expiresAt: Date().addingTimeInterval(3600), summary: "The sleep is queued.")
        runner.answer("status", json: Self.status.replacingOccurrences(
            of: #""agent": { "version": "3.0.0", "contract": 3 },"#,
            with: #""agent": { "version": "3.0.0", "contract": 3 }, "operations": { "running": [ { "id": "\#(record.id)", "kind": "sleep", "state": "running", "outcome": "pending" } ] },"#))
        await model.refresh()

        #expect(store.record(id: record.id)?.state == .running)
        #expect(!model.isShownRunning(record))
        #expect(model.unresolvedOperations.map(\.id) == [record.id])
        #expect(model.unresolvedOperations.first?.summary == "Sleep: This Mac left the queue and started on This Mac.")
    }

    // MARK: - Parity

    @Test("the local device offers restart and configured actions, like every other machine")
    func localParity() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        #expect(model.isReachable)
        #expect(model.services.count == 1)
        #expect(model.actions.count == 1)
        let service = try #require(model.services.first)
        // Both were missing entirely before: the local side had update and nothing else.
        #expect(model.restartUnavailableReason(service) == nil)
        #expect(model.actionUnavailableReason() == nil)
    }

    @Test("restarting a service here goes through the same command as anywhere else")
    func restartSendsTheSameCommand() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("restart", json: """
        { "ok": true, "contract": 3, "action": "restarted", "service": "t3" }
        """)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        let service = try #require(model.services.first)
        model.restart(service, force: false, whenIdle: false)
        await Self.settle(model)

        let restart = try #require(runner.sent.first { $0.contains("restart") })
        #expect(restart.contains("--service"))
        #expect(restart.contains("t3"))
        #expect(store.newestFirst.first { $0.kind == .restart }?.state == .succeeded)
    }

    @Test("a busy local machine holds the work back and offers the override")
    func deferredOffersForceHere() async throws {
        // The old comment said the app "must not have a way to switch off" the refusal, which meant
        // a stuck busy probe made this Mac impossible to update by any means. The refusal is kept;
        // what is added is the ability to say "I know, do it anyway" after the agent has looked.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "deferred", "reasonCode": "busy",
          "service": "t3", "message": "1 turn running" }
        """)
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        var asked: PendingDialog?
        model.ask = { asked = $0 }

        await model.refresh()
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(asked?.confirmTitle == "Do it anyway")
        #expect(model.note?.contains("Nothing was interrupted") == true)
        // The first attempt never carries force.
        let update = try #require(runner.sent.first { $0.contains("update") })
        #expect(!update.contains("--force"))
    }

    @Test("a local timeout is an unknown outcome, not a failure")
    func localTimeoutIsUnknown() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answerTimeout("update")
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(store.newestFirst.first { $0.kind == .update }?.state == .unknown)
        #expect(model.note?.contains("may or may not have happened") == true)
    }

    @Test("a no-op here says which reason it was, exactly as a remote one does")
    func localNoopReason() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "noop", "reasonCode": "latest-unknown", "service": "t3" }
        """)
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.note?.contains("could not be read") == true)
        // Not benign: the source could not be reached, which is not "there is nothing to install".
        #expect(model.noteIsError)
    }

    // MARK: - One core with the machines

    @Test("a failed action here is kept as the last outcome, exactly as one over ssh is")
    func localFailureIsTheLastOutcome() async throws {
        // The local side had no last outcome at all, so the panel's error notice, which reads it,
        // never showed a failure on the device the app runs on.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("update", json: """
        { "ok": false, "contract": 3, "action": "failed", "message": "The updater refused this release." }
        """, exit: 1)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        let outcome = try #require(model.lastActionOutcome)
        #expect(outcome.isError)
        #expect(outcome.text.contains("refused this release"))
        #expect(outcome.operationId == store.newestFirst.first { $0.kind == .update }?.id)
    }

    @Test("a setup the local agent cannot hand over is said, not swallowed")
    func unreadableLocalSetupIsSaid() async throws {
        // Reading the local agent's copy used to return nothing on any error without a word, so a
        // setup this Mac could not read looked exactly like one that was never asked for.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answerTimeout("config")
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        model.adoptRemoteSetup()
        let deadline = Date().addingTimeInterval(15)
        while model.note == nil, Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }

        #expect(model.note?.contains("could not be read") == true)
        #expect(model.note?.contains("did not finish within") == true)
        #expect(model.noteIsError)
    }

    @Test("the local maintenance cycle keeps what happened to each service")
    func localCycleKeepsDetails() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("cycle", json: """
        { "ok": true, "contract": 3, "action": "cycled",
          "children": [ { "service": "t3", "action": "updated" },
                        { "service": "edge", "action": "deferred", "reasonCode": "busy" } ] }
        """)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        model.runCycle()
        await Self.settle(model)

        let record = try #require(store.newestFirst.first { $0.kind == .updateAll })
        #expect(record.state == .succeeded)
        #expect(record.summary == "Maintenance cycle on This Mac: 1 updated, 1 held back.")
        // The counts used to be all there was. Which service was held back, and why, is the part
        // anyone acting on the result needs.
        #expect(record.detail == "t3: updated\nedge: deferred (busy)")
        #expect(model.noteDetail == record.detail)
    }

    @Test("a cycle reads each service's part by its outcome, and one nobody can say the end of leaves it owed")
    func cycleChildrenByOutcome() async throws {
        // Counted by action word, an update that timed out half-way was a plain failure and an
        // interrupted one was not counted at all, so this cycle was filed as failed, and one with
        // only the interrupted service in it as a success (C5).
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("cycle", json: """
        { "ok": true, "contract": 3, "action": "cycled",
          "children": [ { "service": "t3", "action": "updated" },
                        { "service": "edge", "action": "interrupted" },
                        { "service": "demo", "action": "failed", "reasonCode": "timed-out" },
                        { "service": "cache", "action": "failed", "reasonCode": "apply-failed" },
                        { "service": "web", "action": "queued" } ] }
        """)
        runner.answer("status", json: Self.status)
        runner.answer("cycle", json: """
        { "ok": true, "contract": 3, "action": "cycled", "children": [ { "service": "edge", "action": "interrupted" } ] }
        """)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        model.runCycle()
        await Self.settle(model)
        let record = try #require(store.newestFirst.first { $0.kind == .updateAll })
        #expect(record.state == .unknown)
        #expect(record.needsReconciliation)
        #expect(record.summary == "Maintenance cycle on This Mac: 1 updated, 1 held back, 1 failed, 2 may or may not have updated.")
        #expect(model.noteIsError)

        model.runCycle()
        await Self.settle(model)
        let second = try #require(store.newestFirst.first { $0.kind == .updateAll })
        #expect(second.id != record.id)
        #expect(second.state == .unknown)
        #expect(second.summary == "Maintenance cycle on This Mac: 1 may or may not have updated.")
    }

    @Test("a cycle starts from its own outcome, so one the agent failed or held back is never nothing to do")
    func cycleOwnOutcomeFirst() async throws {
        // Decided from its services alone, a cycle the agent failed before it reached any of them
        // had no failed service in it, and each of these was filed as a green "nothing to do" (C5).
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        // The agent's own catch, when the cycle record cannot be written.
        runner.answer("cycle", json: """
        {"ok":false,"contract":3,"action":"failed","reasonCode":"internal","children":[],
         "message":"the cycle record could not be written (EACCES), so no maintenance was attempted"}
        """, exit: 1)
        runner.answer("status", json: Self.status)
        // A 3.1.0 refusal that states its outcome and names no action.
        runner.answer("cycle", json: """
        {"ok":false,"contract":3,"reasonCode":"bad-argument","outcome":"failed","children":[],
         "message":"--op names a different cycle"}
        """, exit: 1)
        runner.answer("status", json: Self.status)
        // Held back by the lock, which names no action either.
        runner.answer("cycle", json: #"{"ok":false,"contract":3,"reasonCode":"lock-held","message":"an update holds the lock"}"#, exit: 1)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh()

        var records: [OperationRecord] = []
        var errors: [Bool] = []
        for _ in 0..<3 {
            model.runCycle()
            await Self.settle(model)
            records.append(try #require(store.newestFirst.first { $0.kind == .updateAll }))
            errors.append(model.noteIsError)
        }
        #expect(Set(records.map(\.id)).count == 3)
        #expect(records.map(\.state) == [.failed, .failed, .deferred])
        #expect(errors == [true, true, false])
        #expect(records.allSatisfy { !$0.summary.contains("nothing to do") && !$0.needsReconciliation })
        #expect(records[0].summary == "Maintenance cycle on This Mac failed: the cycle record could not be written (EACCES), so no maintenance was attempted")
        #expect(records[1].summary == "Maintenance cycle on This Mac failed: The agent refused an argument this app sent. --op names a different cycle")
        #expect(records[2].summary == "The maintenance lock is held by something else on that machine.")
    }

    @Test("a cycle left unknown by a service stays unknown when its finished record is read again")
    func cycleVerdictSurvivesReconciliation() async throws {
        // The record says `cycled` whatever became of its services, and read on its own it turned
        // this cycle green within one pass, with the agent's "1 left alone" for an interrupted update.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh()

        model.runCycle()
        let id = try #require(store.newestFirst.first { $0.kind == .updateAll }).id
        runner.answer("cycle", json: """
        { "ok": true, "contract": 3, "action": "cycled", "children": [ { "service": "edge", "action": "interrupted" } ] }
        """)
        // The status after it carries the cycle's summary, which names none of its services, so the
        // record itself is read by its id.
        runner.answer("status", json: Self.status.replacingOccurrences(of: #""actions":"#, with: """
        "operations": { "recent": [ { "id": "\(id)", "kind": "cycle", "state": "finished", "phase": "done",
                                      "action": "cycled", "reasonCode": null, "outcome": "succeeded" } ] },
        "actions":
        """))
        runner.answer("op", json: """
        {"ok":true,"contract":3,"op":{"id":"\(id)","kind":"cycle","state":"finished","phase":"done","outcome":"succeeded",
         "result":{"ok":true,"action":"cycled","reasonCode":null,"message":"0 updated, 1 left alone, 0 failed","outcome":"succeeded"},
         "children":[{"opId":null,"service":"edge","action":"interrupted","reasonCode":null}]}}
        """)
        await Self.settle(model)
        let deadline = Date().addingTimeInterval(15)
        while store.record(id: id)?.needsReconciliation == true, Date() < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }

        let record = try #require(store.record(id: id))
        #expect(runner.sent.contains { $0.contains("op") && $0.contains(id) })
        #expect(record.state == .unknown)
        #expect(record.summary == "Maintenance cycle on This Mac: 1 may or may not have updated.")
        // Read once: the same finished record has nothing more to say about that service.
        #expect(!record.needsReconciliation)
    }

    @Test("a policy change is decided on its outcome: held back by the lock, unknown, or refused")
    func policyByOutcome() async throws {
        // Read off `ok`, all three of these were one "not changed", and the one the agent failed
        // on half-way through writing may well have changed it.
        let runner = FakeRunner()
        for _ in 0..<4 { runner.answer("status", json: Self.status) }
        runner.answer("policy", json: #"{"ok":false,"contract":3,"reasonCode":"lock-held","message":"an update holds the lock"}"#, exit: 1)
        runner.answer("policy", json: #"{"ok":false,"contract":3,"reasonCode":"internal","message":"the agent failed while writing the policy"}"#, exit: 1)
        runner.answer("policy", json: #"{"ok":false,"contract":3,"reasonCode":"bad-argument","outcome":"failed","message":"that is not a window"}"#, exit: 1)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh()

        var states: [OperationState] = []
        for _ in 0..<3 {
            model.pause(for: 4 * 60 * 60, service: nil)
            await Self.settle(model)
            states.append(try #require(store.newestFirst.first { $0.kind == .policy }).state)
        }
        #expect(states == [.deferred, .unknown, .failed])
        #expect(model.note == "that is not a window")
        #expect(store.newestFirst.filter { $0.kind == .policy }.map(\.summary).contains {
            $0.hasPrefix("The policy change may or may not have taken effect: the agent failed")
        })
    }

    @Test("replacing the setup is decided on its outcome: a push the lock held back is not a refusal")
    func setupReplacementByOutcome() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("config", json: String(decoding: try ContractCorpus.data("fixtures/config.set.locked.json"), as: UTF8.self), exit: 1)
        runner.answer("status", json: Self.status)
        runner.answer("config", json: #"{"ok":false,"contract":3,"reasonCode":"internal","outcome":"unknown","message":"ENOSPC"}"#, exit: 1)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let document = Self.document
        model.controllerConfig = { document }
        await model.refresh()

        model.replaceSetup(over: nil, from: document.hash)
        await Self.settle(model)
        let held = try #require(store.newestFirst.first { $0.kind == .shareSetup })
        #expect(held.state == .deferred)
        #expect(held.summary.contains("was not replaced yet"))
        #expect(!model.noteIsError)

        model.replaceSetup(over: nil, from: document.hash)
        await Self.settle(model)
        let unknown = try #require(store.newestFirst.first { $0.kind == .shareSetup })
        #expect(unknown.id != held.id)
        #expect(unknown.state == .unknown)
        #expect(model.note == "The setup may or may not have been replaced: ENOSPC")
    }

    @Test("a replacement is done only when the agent names the document sent, as every client reads it")
    func setupReplacementNamesTheDocumentSent() async throws {
        // Any success used to do, so a reply naming no document, which an automatic push of the
        // same bytes reads as a failure, was taken for a replacement here and on Android.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("config", json: #"{ "ok": true, "contract": 3, "action": "replaced" }"#)
        runner.answer("status", json: Self.status)
        runner.answer("config", json: #"{ "ok": true, "contract": 3, "action": "replaced", "hash": "fedcba9876543210" }"#)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let document = Self.document
        model.controllerConfig = { document }
        await model.refresh()

        model.replaceSetup(over: nil, from: document.hash)
        await Self.settle(model)
        let unnamed = try #require(store.newestFirst.first { $0.kind == .shareSetup })
        #expect(unnamed.state == .unknown)
        #expect(model.note == "The setup may or may not have been replaced: its agent did not say which document it holds now.")
        #expect(model.noteIsError)

        model.replaceSetup(over: nil, from: document.hash)
        await Self.settle(model)
        let other = try #require(store.newestFirst.first { $0.kind == .shareSetup })
        #expect(other.id != unnamed.id)
        #expect(other.state == .unknown)
        #expect(model.note == "The setup may or may not have been replaced: its agent answered with a document other than the one sent.")
    }

    @Test("a replacement goes only over the copy the question showed, from the setup it was asked over")
    func setupReplacementOverTheCopyShown() async throws {
        // The desktop's rule: `--replace` overwrites whatever the agent holds when it arrives, so a
        // copy written there since the question was drawn, which nobody here has seen, is not sent
        // over on a decision about the one before it.
        let shown = String(repeating: "a", count: 64)
        let newer = String(repeating: "b", count: 64)
        let holding: (String) -> String = { hash in
            Self.status.replacingOccurrences(of: #""updates":"#, with: #""controller": { "hash": "\#(hash)", "id": "setup-other", "revision": 4 }, "updates":"#)
        }
        let runner = FakeRunner()
        runner.answer("status", json: holding(shown))
        runner.answer("status", json: holding(newer))
        runner.answer("status", json: holding(Self.document.hash))
        runner.answer("status", json: holding(shown))
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let document = Self.document
        model.controllerConfig = { document }
        var asked: PendingDialog?
        model.ask = { asked = $0 }

        await model.refresh()
        model.core.setup.verdict = SetupSync.Verdict(.differentSetup)
        model.requestSetupReplacement()
        let question = try #require(asked)
        await model.refresh()
        question.perform()
        await Self.settle(model)
        #expect(model.note == "Nothing was sent: the local agent holds another document now, not the one shown. Read it again first.")
        #expect(model.noteIsError)

        await model.refresh()
        model.replaceSetup(over: shown, from: document.hash)
        #expect(model.note == "Nothing needed sending: the local agent holds this device's setup already.")

        // And not from a setup here that changed since the question was asked.
        await model.refresh()
        var edited = document
        edited.hash = "fedcba9876543210"
        model.controllerConfig = { edited }
        model.replaceSetup(over: shown, from: document.hash)
        #expect(model.note?.hasPrefix("The setup here changed since that was shown") == true)

        // Nor while a hand edit of the file waits for its revision, which leaves no document here
        // at all. That returned without a word.
        model.controllerConfig = { nil }
        model.replaceSetup(over: shown, from: document.hash)
        #expect(model.note == "The setup here changed since that was shown, so nothing was sent.")
        #expect(model.noteIsError)

        await Self.settle(model)
        #expect(!runner.sent.contains { $0.contains("--replace") })
        #expect(!store.newestFirst.contains { $0.kind == .shareSetup })
    }

    @Test("the different-setup sheet stays up until a Replace settles it, and says why when it does not")
    func setupReplacementFromTheSheet() async throws {
        // The sheet closed before anything was sent, so a refusal, a push the lock held back and a
        // write nobody can say the end of were said only on a line behind it, and the question the
        // configuration guide says they leave open was gone.
        let shown = String(repeating: "a", count: 64)
        let newer = String(repeating: "b", count: 64)
        let holding: (String) -> String = { hash in
            Self.status.replacingOccurrences(of: #""updates":"#, with: #""controller": { "hash": "\#(hash)", "id": "setup-other", "revision": 4 }, "updates":"#)
        }
        let runner = FakeRunner()
        runner.answer("status", json: holding(shown))
        runner.answer("config", json: String(decoding: try ContractCorpus.data("fixtures/config.set.locked.json"), as: UTF8.self), exit: 1)
        runner.answer("status", json: holding(shown))
        runner.answer("config", json: #"{ "ok": true, "contract": 3, "action": "replaced", "hash": "0123456789abcdef" }"#)
        runner.answer("status", json: holding(Self.document.hash))
        runner.answer("status", json: holding(newer))
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let home = try AppModelTests.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try AppModelTests.app(AppModelTests.twoMachines, in: home)
        let document = Self.document
        model.controllerConfig = { document }
        let divergence = SetupDivergence(machineId: model.historyMachineId, machineName: "This Mac", mine: document,
                                         theirsBytes: Data("{}\n".utf8), theirsHash: shown,
                                         theirsIdentity: ControllerIdentity(id: "setup-other", revision: 4, lineage: []),
                                         baseBytes: nil, differences: [], isDifferentSetup: true, mergeBlocker: nil)
        var said: [String] = []
        let replaces = { runner.sent.filter { $0.contains("--replace") }.count }
        await model.refresh()

        // Held back by the lock: the sheet stays and says so. A second press while the first is out
        // sends nothing more.
        app.divergence = divergence
        app.replaceSetup(on: model, for: divergence) { said.append($0) }
        #expect(app.replacingSetupOn == model.historyMachineId)
        app.replaceSetup(on: model, for: divergence) { said.append($0) }
        await Self.settle(model)
        #expect(replaces() == 1)
        #expect(app.replacingSetupOn == nil)
        #expect(app.divergence != nil)
        #expect(said.count == 1)
        #expect(said.first?.contains("was not replaced yet") == true)

        // The agent's latest status names the copy the sheet showed, so `--replace` goes, and the
        // agent taking the document is what closes the sheet.
        app.replaceSetup(on: model, for: divergence) { said.append($0) }
        await Self.settle(model)
        #expect(replaces() == 2)
        #expect(app.divergence == nil)
        #expect(said.count == 1)
        #expect(model.note == "Replaced the setup the local agent was holding with this device's.")

        // A question out of date closes without sending, the device's line saying why, and the
        // device's row asks it again from what both hold now.
        await model.refresh()
        app.divergence = divergence
        app.replaceSetup(on: model, for: divergence) { said.append($0) }
        #expect(app.divergence == nil)
        #expect(app.replacingSetupOn == nil)
        #expect(model.note == "Nothing was sent: the local agent holds another document now, not the one shown. Read it again first.")
        #expect(replaces() == 2)
        #expect(said.count == 1)
    }

    @Test("a local action that never reached the agent is followed by a fresh read straight away")
    func localNeverDispatchedRereads() async throws {
        // A failure that proves nothing ran asks for no read over ssh. Here the read costs one local
        // process, and without it this Mac went on showing an agent that had gone, until the next
        // poll, which never comes while nothing is on screen.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        #expect(model.isReachable)
        let service = try #require(model.services.first)
        try FileManager.default.removeItem(at: directory.appending(path: "index.mjs"))
        model.restart(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(store.newestFirst.first { $0.kind == .restart }?.state == .failed)
        #expect(model.failure?.kind == .agentMissing)
        #expect(!model.isReachable)
        // The read does not take the action's own sentence away.
        #expect(model.note?.contains("did not happen") == true)
    }

    @Test("what a configured action printed here stays with this Mac's own line")
    func localActionOutputStaysWithTheLine() async throws {
        // It used to be folded into the line itself. It is the line's detail now, which the local
        // section offers behind Details rather than leaving it to the window's status bar.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("run", json: """
        { "ok": true, "contract": 3, "action": "ran", "output": "reindexed 412 documents" }
        """)
        runner.answer("status", json: Self.status)
        let (model, store, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        let action = try #require(model.actions.first)
        model.run(action, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.note == "Reindex ran.")
        #expect(model.noteDetail == "reindexed 412 documents")
        #expect(store.newestFirst.first { $0.kind == .action }?.output == "reindexed 412 documents")
    }

    @Test("replacing the copy the local agent holds names the local agent, not this Mac")
    func localSetupReplacementWording() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("config", json: #"{ "ok": true, "contract": 3, "action": "replaced", "hash": "0123456789abcdef" }"#)
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let document = Self.document
        model.controllerConfig = { document }
        var asked: PendingDialog?
        model.ask = { asked = $0 }

        await model.refresh()
        model.core.setup.verdict = SetupSync.Verdict(.diverged)
        model.requestSetupReplacement()
        let question = try #require(asked)
        #expect(question.title == "Replace the setup the local agent is holding?")

        question.perform()
        await Self.settle(model)
        #expect(model.note == "Replaced the setup the local agent was holding with this device's.")
        #expect(runner.sent.contains { $0.contains("config") && $0.contains("--replace") })
    }

    @Test("a setup with an error replaces nothing, asked from the sheet or straight away")
    func setupWithErrorReplacesNothing() async throws {
        // Section 9.5: a device whose setup has a section 9.7 error pushes nothing until it is
        // fixed, and a replacement least of all, because an agent before 3.1.0 stores whatever it
        // is sent.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        var broken = Self.document
        broken.errors = [DocumentAcceptance.Problem(code: "helper-unknown", path: "machines[0].wake.helpers[0]",
                                                    message: "the helper names no machine")]
        model.controllerConfig = { broken }
        var asked: PendingDialog?
        model.ask = { asked = $0 }

        await model.refresh()
        model.core.setup.verdict = SetupSync.Verdict(.diverged)
        model.requestSetupReplacement()
        #expect(asked == nil)
        #expect(model.noteIsError)
        #expect(model.note?.contains("the helper names no machine") == true)

        model.replaceSetup(over: nil, from: broken.hash)
        await Self.settle(model)
        #expect(!runner.sent.contains { $0.contains("--replace") })

        let divergence = SetupDivergence(machineId: "mac", machineName: "This Mac", mine: broken, theirsBytes: Data("{}\n".utf8),
                                         theirsHash: "fedcba9876543210", theirsIdentity: ControllerIdentity(id: "setup-other", revision: 4, lineage: []),
                                         baseBytes: nil, differences: [], isDifferentSetup: true, mergeBlocker: nil)
        #expect(divergence.replaceBlocker?.contains("the helper names no machine") == true)
        var fixed = divergence
        fixed.mine = Self.document
        #expect(fixed.replaceBlocker == nil)
    }

    // MARK: - A different agent

    /// A setup with an identity, so it takes part in reconciliation (section 9.5, rule 3).
    static let document = ControllerDocument(bytes: Data("{}\n".utf8), hash: "0123456789abcdef",
                                             identity: ControllerIdentity(id: "setup-test", revision: 1, lineage: []))

    /// A status from an agent that can hold the setup and holds none yet, so the first read pushes.
    static let statusHoldingNoSetup = """
    { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "mac", "name": "macOS" },
      "agent": { "version": "3.0.0", "contract": 3 },
      "services": [ { "id": "t3", "name": "T3 Code", "installed": "1", "latest": "1", "upToDate": true } ],
      "controller": {} }
    """

    /// The argv a binding gives, run through a fake. `/bin/echo` only has to exist.
    static func boundAgent(_ runner: FakeRunner) -> (argv: [String], agent: MacAgent) {
        let argv = ["/bin/echo", "/opt/agent/index.mjs"]
        return (argv, MacAgent(argv: argv) { _, arguments, _, _ in runner.run(arguments) })
    }

    /// Holds every command with one verb until the test lets it go, and says when one has arrived.
    final class Gate: @unchecked Sendable {
        private let lock = NSLock()
        private let verb: String
        private var open = false
        private var arrived = false

        init(holding verb: String) { self.verb = verb }

        func release() { lock.withLock { open = true } }

        private var isOpen: Bool { lock.withLock { open } }

        private var hasArrived: Bool { lock.withLock { arrived } }

        private func arrive() { lock.withLock { arrived = true } }

        /// Called by the runner with every command, and returns once the command may go on.
        func pass(_ arguments: [String]) async {
            guard arguments.count > 1, arguments[1] == verb else { return }
            arrive()
            while !isOpen { try? await Task.sleep(for: .milliseconds(5)) }
        }

        func waitUntilHolding(timeout: TimeInterval = 5) async -> Bool {
            let deadline = Date().addingTimeInterval(timeout)
            while !hasArrived, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
            return hasArrived
        }
    }

    /// A model whose agent waits at the gate.
    static func gatedModel(_ runner: FakeRunner, gate: Gate) throws -> (MacModel, URL) {
        let (script, directory) = try scriptFile()
        let path = script.path(percentEncoded: false)
        let agent = MacAgent(scriptPath: path, interpreter: "/bin/echo") { _, arguments, _, _ in
            await gate.pass(arguments)
            return runner.run(arguments)
        }
        return (MacModel(config: LocalConfig(enabled: true, name: "This Mac", agent: path), agent: agent), directory)
    }

    @Test("a new agent command starts from what the new agent says, and gets the push the old one refused")
    func newAgentCommandStartsAfresh() async throws {
        // The model is kept across the change now, and it used to keep everything the old agent had
        // said with it: its services until the next read, its doctor run, and a refusal of the
        // current document that held the push to the new agent back for a minute.
        let old = FakeRunner()
        old.answer("status", json: Self.statusHoldingNoSetup)
        old.answer("config", json: #"{ "ok": false, "contract": 3, "error": "this agent keeps the copy it has" }"#, exit: 1)
        old.answer("doctor", json: #"{ "ok": true, "contract": 3, "checks": [] }"#)
        old.answer("status", json: Self.statusHoldingNoSetup)
        let (model, _, directory) = try Self.model(old)
        defer { try? FileManager.default.removeItem(at: directory) }
        let document = Self.document
        model.controllerConfig = { document }

        await model.refresh()
        let deadline = Date().addingTimeInterval(15)
        while model.core.setup.lastShare == nil, Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }
        guard case .failed = model.setupSharing else {
            Issue.record("the old agent's refusal was not shown: \(model.setupSharing)")
            return
        }
        model.runDoctor()
        await Self.settle(model)
        #expect(model.doctor != nil)
        let outcome = try #require(model.lastActionOutcome)

        let fresh = FakeRunner()
        fresh.answer("status", json: Self.statusHoldingNoSetup)
        fresh.answer("config", json: #"{ "ok": true, "contract": 3, "action": "stored", "hash": "0123456789abcdef" }"#)
        let bound = Self.boundAgent(fresh)
        model.update(config: LocalConfig(enabled: true, name: "This Mac", agent: bound.argv.last ?? ""),
                     agent: bound.agent, boundMachineId: nil, boundAgentArgv: bound.argv)

        // Nothing the old agent said is shown as the new one's, and what the user did stays.
        #expect(model.status == nil)
        #expect(model.failure == nil)
        #expect(model.sidebarSummary == "checking")
        #expect(model.doctor == nil)
        #expect(model.setupSharing == .unknown)
        #expect(model.lastActionOutcome == outcome)

        await model.refresh()
        let pushed = Date().addingTimeInterval(15)
        while model.core.setup.lastShare == nil, Date() < pushed { try? await Task.sleep(for: .milliseconds(10)) }
        #expect(fresh.sent.contains { $0.contains("config") && $0.contains("set") })
        #expect(model.core.setup.lastShare?.failure == nil)
    }

    @Test("a read that was out when the agent command changed is not taken as the new agent's")
    func readAcrossAnAgentChangeIsDropped() async throws {
        let old = FakeRunner()
        old.answer("status", json: Self.status)
        let gate = Gate(holding: "status")
        let (model, directory) = try Self.gatedModel(old, gate: gate)
        defer { try? FileManager.default.removeItem(at: directory) }

        let read = Task { await model.refresh() }
        #expect(await gate.waitUntilHolding())
        let fresh = FakeRunner()
        fresh.answer("status", json: Self.statusHoldingNoSetup)
        let bound = Self.boundAgent(fresh)
        model.update(config: LocalConfig(enabled: true, name: "This Mac", agent: bound.argv.last ?? ""),
                     agent: bound.agent, boundMachineId: nil, boundAgentArgv: bound.argv)
        gate.release()
        await read.value

        // The old agent answered, and its answer went nowhere.
        #expect(old.sent.count == 1)
        #expect(model.status == nil)
        #expect(model.lastChecked == nil)

        // Nor does it count as a read: the next chance to read asks the new agent at once.
        await model.refreshIfNeeded()
        #expect(fresh.sent.count == 1)
        #expect(model.services.first?.upToDate == true)
    }

    @Test("a push that was out when the agent command changed is not held against the new agent")
    func pushAcrossAnAgentChangeIsDropped() async throws {
        let old = FakeRunner()
        old.answer("status", json: Self.statusHoldingNoSetup)
        old.answer("config", json: #"{ "ok": false, "contract": 3, "error": "this agent keeps the copy it has" }"#, exit: 1)
        let gate = Gate(holding: "config")
        let (model, directory) = try Self.gatedModel(old, gate: gate)
        defer { try? FileManager.default.removeItem(at: directory) }
        let document = Self.document
        model.controllerConfig = { document }

        await model.refresh()
        #expect(await gate.waitUntilHolding())
        let fresh = FakeRunner()
        fresh.answer("status", json: Self.statusHoldingNoSetup)
        fresh.answer("status", json: Self.statusHoldingNoSetup)
        fresh.answer("config", json: #"{ "ok": true, "contract": 3, "action": "stored", "hash": "0123456789abcdef" }"#)
        let bound = Self.boundAgent(fresh)
        model.update(config: LocalConfig(enabled: true, name: "This Mac", agent: bound.argv.last ?? ""),
                     agent: bound.agent, boundMachineId: nil, boundAgentArgv: bound.argv)

        // The old push is still out, so the new agent's first read starts no second one beside it.
        await model.refresh()
        #expect(model.isReachable)
        #expect(!fresh.sent.contains { $0.contains("config") })

        gate.release()
        let deadline = Date().addingTimeInterval(15)
        while model.core.setup.isSharing, Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }
        // The old agent's refusal is about the old agent. The new one holds nothing, so the row says
        // a push is on its way rather than that one failed.
        #expect(model.core.setup.lastShare == nil)
        #expect(model.setupSharing == .publishing)
        #expect(!model.noteIsError)

        await model.refresh()
        let pushed = Date().addingTimeInterval(15)
        while model.core.setup.lastShare == nil, Date() < pushed { try? await Task.sleep(for: .milliseconds(10)) }
        #expect(fresh.sent.contains { $0.contains("config") && $0.contains("set") })
        #expect(model.core.setup.lastShare?.failure == nil)
    }

    @Test("a new name for this device keeps what its agent said")
    func renameKeepsTheReading() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        #expect(model.isReachable)
        let path = model.config.agentPath
        model.update(config: LocalConfig(enabled: true, name: "Desk", agent: path),
                     agent: MacAgent(scriptPath: path, interpreter: "/bin/echo") { _, arguments, _, _ in runner.run(arguments) },
                     boundMachineId: nil, boundAgentArgv: nil)

        #expect(model.name == "Desk")
        #expect(model.isReachable)
        #expect(model.lastChecked != nil)
    }

    @Test("the local agent is run through the bound argv when the device is a machine in the setup")
    func boundLocalAgent() async throws {
        // The `self` binding: this device is one of the machines in the shared document, and it is
        // driven by spawning an agent here rather than by ssh to itself.
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let agent = MacAgent(argv: ["/bin/echo", "/opt/agent/index.mjs"]) { executable, arguments, _, _ in
            #expect(executable == "/bin/echo")
            #expect(arguments.first == "/opt/agent/index.mjs")
            return runner.run(arguments)
        }
        let model = MacModel(config: LocalConfig(enabled: true, name: "Tower", agent: ""), agent: agent)
        model.boundMachineId = "tower"
        await model.refresh()
        #expect(model.isReachable)
        #expect(model.boundMachineId == "tower")
    }

    @Test("a bound argv whose binary is not there says so rather than looking for node")
    func boundArgvMustExist() async throws {
        let agent = MacAgent(argv: ["/nowhere/at/all/node", "/opt/agent/index.mjs"])
        let model = MacModel(config: LocalConfig(enabled: true, name: "Tower", agent: ""), agent: agent)
        await model.refresh()
        let failure = try #require(model.failure)
        #expect(failure.kind == .interpreterMissing)
        #expect(failure.dispatch == .never)
    }

    @Test("the update policy here is the same model as everywhere else")
    func localPolicy() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        let policy = try #require(model.policy)
        #expect(policy.automatic == true)
        #expect(model.autoUpdate == true)
        #expect(model.dialect.supportsPolicy)
    }

    @Test("a policy change sends the patch on stdin and only the keys that changed")
    func policyPatch() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.status)
        runner.answer("policy", json: """
        { "ok": true, "contract": 3, "updates": { "automatic": true, "pauseUntil": "2030-01-01T00:00:00.000Z" } }
        """)
        runner.answer("status", json: Self.status)
        let (model, _, directory) = try Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh()
        model.pause(for: 4 * 60 * 60, service: nil)
        await Self.settle(model)

        let policy = try #require(runner.sent.first { $0.contains("policy") })
        #expect(policy.contains("set"))
        #expect(model.note?.contains("paused") == true)
    }

    @Test("the patch itself carries only what was asked to change")
    func patchShape() throws {
        let onlyPause = try #require(AgentUpdatePolicy.patch(pauseUntil: .some(Date(timeIntervalSince1970: 0))))
        let object = try #require((try JSONSerialization.jsonObject(with: onlyPause)) as? [String: Any])
        #expect(object.keys.sorted() == ["pauseUntil"])

        // An explicit null is how a service is put back on the system's answer, and it has to reach
        // the agent as a null rather than as an absent key.
        let inherit = try #require(AgentUpdatePolicy.patch(automatic: .some(nil), pauseUntil: .some(nil),
                                                           windows: .some(nil)))
        let inherited = try #require((try JSONSerialization.jsonObject(with: inherit)) as? [String: Any])
        #expect(inherited["automatic"] is NSNull)
        #expect(inherited["pauseUntil"] is NSNull)
        #expect(inherited["maintenanceWindows"] is NSNull)

        // Nothing to change is no request at all.
        #expect(AgentUpdatePolicy.patch() == nil)
    }

    @Test("a maintenance window is checked before it is sent")
    func windowValidation() {
        #expect(MaintenanceWindow(days: nil, from: "02:00", to: "06:00").looksValid)
        // Overnight is normal and is named as such rather than read as a mistake.
        let overnight = MaintenanceWindow(days: nil, from: "22:00", to: "02:00")
        #expect(overnight.looksValid)
        #expect(overnight.crossesMidnight)
        #expect(overnight.summary.contains("overnight"))

        #expect(!MaintenanceWindow(days: nil, from: "2:00", to: "06:00").looksValid)
        #expect(!MaintenanceWindow(days: nil, from: "25:00", to: "06:00").looksValid)
        #expect(!MaintenanceWindow(days: nil, from: "02:00", to: "02:00").looksValid)
        #expect(!MaintenanceWindow(days: ["funday"], from: "02:00", to: "06:00").looksValid)
    }
}
