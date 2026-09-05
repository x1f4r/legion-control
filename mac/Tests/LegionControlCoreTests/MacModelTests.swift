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
        let deadline = Date().addingTimeInterval(2)
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
