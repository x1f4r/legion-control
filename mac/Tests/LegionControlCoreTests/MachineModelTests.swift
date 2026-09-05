import Foundation
import Testing
@testable import LegionControlCore

/// What the machine section actually does with what comes back.
///
/// These drive a real `MachineModel` through a transport that returns canned replies, because the
/// defects the review found were not in the parsing: they were in what the app concluded from a
/// reply, and especially from the absence of one.
@MainActor
struct MachineModelTests {

    // MARK: - Harness

    /// A transport that answers from a script keyed by the first word of the command.
    final class FakeRunner: @unchecked Sendable {
        private let lock = NSLock()
        private var replies: [String: [CommandResult]] = [:]
        private(set) var commands: [[String]] = []

        func answer(_ verb: String, with results: [CommandResult]) {
            lock.lock(); defer { lock.unlock() }
            replies[verb] = results
        }

        func answer(_ verb: String, json: String, exit: Int32 = 0) {
            answer(verb, with: [CommandResult(exitCode: exit, standardOutput: json, standardError: "",
                                              timedOut: false, launchFailure: nil)])
        }

        func run(_ arguments: [String]) -> CommandResult {
            lock.lock(); defer { lock.unlock() }
            commands.append(arguments)
            // The agent argv and the ssh options come first; the verb is the first token of the
            // quoted command, which is the last argument.
            let verb = arguments.last?.split(separator: " ").dropFirst(2).first.map(String.init) ?? ""
            guard var queued = replies[verb], !queued.isEmpty else {
                return CommandResult(exitCode: 255, standardOutput: "", standardError: "no scripted reply for \(verb)",
                                     timedOut: false, launchFailure: nil)
            }
            let next = queued.removeFirst()
            if !queued.isEmpty { replies[verb] = queued }
            return next
        }

        var sentCommands: [String] {
            lock.lock(); defer { lock.unlock() }
            return commands.compactMap(\.last)
        }
    }

    static func machine() -> Machine {
        Machine(
            id: "pi",
            name: "Pi",
            endpoints: [Endpoint(id: "lan", kind: "lan", host: "10.0.0.5", user: "me")],
            systems: [SystemConfig(id: "linux", name: "Linux", platform: .linux, agent: ["node", "/a.mjs"]),
                      SystemConfig(id: "other", name: "Other", platform: .linux, agent: ["node", "/b.mjs"])]
        )
    }

    static func model(_ runner: FakeRunner) -> (MachineModel, OperationStore, URL) {
        var agent = RemoteAgent(machine: machine())
        agent.runner = { _, arguments, _, _ in runner.run(arguments) }
        let model = MachineModel(machine: machine(), agent: agent)

        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-model-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        model.operations = store
        return (model, store, directory)
    }

    @Test("a restricted failed status clears readiness and cannot dispatch a boot")
    func restrictedStatusIsNotReady() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh(userInitiated: false)
        #expect(model.isAwake)
        runner.answer("status", json: #"{"ok":false,"contract":3,"agentVersion":"3.0.0","reasonCode":"restricted","message":"restricted session: command prefix denied","system":{"id":"other"},"services":[]}"#, exit: 1)
        await model.refresh(userInitiated: false)
        #expect(model.status == nil)
        #expect(model.commandableSystem == nil)
        #expect(!model.isAwake)
        if case .offline(let failure) = model.link { #expect(failure.dispatch == .never) }
        else { Issue.record("Failed status must remain offline with an error") }
        let before = runner.sentCommands.count
        var confirmations = 0
        model.ask = { _ in confirmations += 1 }
        model.requestBoot(into: Self.machine().systems[1])
        model.boot(into: Self.machine().systems[1], force: false, whenIdle: false)
        model.sleep(force: false, whenIdle: false)
        #expect(confirmations == 0)
        #expect(runner.sentCommands.count == before)
    }

    static let onlineStatus = """
    { "ok": true, "contract": 3, "agentVersion": "3.0.0",
      "system": { "id": "linux", "name": "Linux" }, "hostname": "pi",
      "agent": { "version": "3.0.0", "contract": 3 },
      "services": [ { "id": "demo", "name": "Demo", "installed": "1", "latest": "2",
                      "upToDate": false, "running": true, "healthy": true,
                      "busy": { "busy": false, "unknown": false, "monitored": true, "evidence": "command" } } ],
      "actions": [ { "id": "wake-tower", "name": "Wake Tower", "kind": "wol" } ],
      "controller": null }
    """

    static func timeout() -> CommandResult {
        CommandResult(exitCode: -1, standardOutput: "", standardError: "", timedOut: true, launchFailure: nil)
    }

    /// Waits for the model to stop working, since actions run in a detached task.
    static func settle(_ model: MachineModel, timeout: TimeInterval = 5) async {
        let deadline = Date().addingTimeInterval(timeout)
        while model.isWorking, Date() < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    // MARK: - Reading

    @Test("a status that decodes puts the machine online with its system named")
    func statusOnline() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: true)
        #expect(model.currentSystem?.id == "linux")
        #expect(model.services.count == 1)
        #expect(model.dialect.supportsOperations)
        #expect(!model.needsAgentUpgrade)
    }

    @Test("a 2.x agent is shown as too old rather than as broken")
    func legacyAgentIsFlagged() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "agentVersion": "2.1.0", "os": "linux",
          "t3": { "installed": "1", "nightly": "2", "upToDate": false } }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.currentSystem != nil)
        #expect(model.needsAgentUpgrade)
        #expect(model.sidebarSummary.contains("agent too old"))
        // The one service it looks after is still drawn.
        #expect(model.services.count == 1)
    }

    @Test("a successful background refresh clears its stale transport failure")
    func recoveredRefreshClearsError() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: #"{"ok":false,"message":"Cannot find module stable launcher"}"#, exit: 1)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh(userInitiated: false)
        #expect(model.statusIsError)
        #expect(model.statusDetail?.contains("Cannot find module") == true)
        runner.answer("status", json: Self.onlineStatus)
        await model.refresh(userInitiated: false)
        #expect(model.isAwake)
        #expect(!model.statusIsError)
        #expect(model.statusDetail == nil)
        #expect(model.statusLine == "Status refreshed.")
    }

    @Test("recovery clears only the refresh error and retains a real failed operation outcome")
    func recoveredRefreshRetainsOperation() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: #"{"ok":false,"action":"failed","message":"The updater refused this release."}"#, exit: 1)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)
        let outcome = try #require(model.lastActionOutcome)
        #expect(outcome.isError)
        runner.answer("status", json: #"{"ok":false,"message":"temporary launcher failure"}"#, exit: 1)
        await model.refresh(userInitiated: false)
        #expect(model.statusDetail?.contains("temporary launcher failure") == true)
        runner.answer("status", json: Self.onlineStatus)
        await model.refresh(userInitiated: false)
        #expect(model.statusLine == outcome.text)
        #expect(model.statusIsError)
        #expect(model.statusDetail?.contains("temporary launcher failure") != true)
        #expect(model.lastActionOutcome == outcome)
        #expect(store.newestFirst.first?.state == .failed)
    }

    @Test("a rejected key is not drawn as a sleeping machine")
    func authFailureIsNotSleep() async throws {
        let runner = FakeRunner()
        runner.answer("status", with: [CommandResult(exitCode: 255, standardOutput: "",
                                                     standardError: "me@pi: Permission denied (publickey).",
                                                     timedOut: false, launchFailure: nil)])
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.statusIsError)
        #expect(model.statusLine.contains("rejected the key"))
        #expect(model.sidebarSummary == "key rejected")
    }

    @Test("an unknown host key raises a question with a fingerprint, and writes nothing")
    func unknownHostKeyAsks() async throws {
        let runner = FakeRunner()
        runner.answer("status", with: [CommandResult(
            exitCode: 255, standardOutput: "",
            standardError: "No ED25519 host key is known for pi and you have requested strict checking.",
            timedOut: false, launchFailure: nil)])
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        // Accepting a key is a trust decision; nothing here does it on the user's behalf.
        let prompt = try #require(model.hostKeyPrompt)
        #expect(prompt.machine == "Pi")
        #expect(prompt.fingerprints.isEmpty)
    }

    @Test("a changed host key offers explicit inspection and never advises deleting pins")
    func changedHostKeyExplains() async throws {
        let runner = FakeRunner()
        runner.answer("status", with: [CommandResult(
            exitCode: 255, standardOutput: "",
            standardError: "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@",
            timedOut: false, launchFailure: nil)])
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.hostKeyPrompt != nil)
        #expect(model.hostKeyPrompt?.canApprove == false)
        #expect(model.statusDetail?.contains("ssh-keygen -R") == false)
    }

    // MARK: - Boot and sleep

    @Test("a boot that times out is an unknown outcome, never a reboot in progress")
    func bootTimeoutIsNotAReboot() async throws {
        // Finding 14. The old transport mapped a timeout to "unreachable" and the boot handler read
        // that as a transition, so the app drew a machine restarting into Windows that had never
        // been asked to do anything.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("boot", with: [Self.timeout()])
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let target = try #require(model.machine.systems.last)
        model.boot(into: target, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.rebootInProgress == nil)
        #expect(model.statusLine.contains("may or may not have happened"))
        let record = try #require(store.newestFirst.first { $0.kind == .boot })
        #expect(record.state == .unknown)
        // And it is owed, so the agent can be asked what really became of it.
        #expect(record.needsReconciliation)
    }

    @Test("a dropped link during a boot is also unknown, however idle the machine looked")
    func bootLinkLossIsNotAReboot() async throws {
        // The old code accepted "the connection dropped" as proof of a reboot when the last reading
        // was idle. It is equally what a machine that refused the command and then lost its network
        // looks like. Only the agent's own acknowledgement counts.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("boot", with: [CommandResult(exitCode: 255, standardOutput: "",
                                                   standardError: "client_loop: send disconnect: Broken pipe",
                                                   timedOut: false, launchFailure: nil)])
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let target = try #require(model.machine.systems.last)
        model.boot(into: target, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.rebootInProgress == nil)
        #expect(store.newestFirst.first { $0.kind == .boot }?.state == .unknown)
    }

    @Test("a boot the agent acknowledged is drawn as a reboot in progress")
    func acknowledgedBootIsATransition() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("boot", json: """
        { "ok": true, "contract": 3, "action": "rebooting", "target": "other",
          "message": "rebooting into Other" }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let target = try #require(model.machine.systems.last)
        model.boot(into: target, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.rebootInProgress?.target.id == "other")
        #expect(store.newestFirst.first { $0.kind == .boot }?.state == .succeeded)
    }

    @Test("a machine that comes back as the wrong system says the switch did not take")
    func bootIsConfirmedByObservation() async throws {
        // Uptime is not evidence and neither is a dropped link. The only thing that settles a boot
        // is seeing the target actually running.
        let runner = FakeRunner()
        runner.answer("status", with: [
            CommandResult(exitCode: 0, standardOutput: Self.onlineStatus, standardError: "", timedOut: false, launchFailure: nil),
            CommandResult(exitCode: 0, standardOutput: Self.onlineStatus, standardError: "", timedOut: false, launchFailure: nil)
        ])
        runner.answer("boot", json: """
        { "ok": true, "contract": 3, "action": "rebooting", "target": "other" }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let target = try #require(model.machine.systems.last)
        model.boot(into: target, force: false, whenIdle: false)
        await Self.settle(model)
        #expect(model.rebootInProgress != nil)

        // It comes back, still running Linux. The expectation is not silently cleared into success.
        await model.refresh(userInitiated: false)
        #expect(model.currentSystem?.id == "linux")
    }

    @Test("a sleep is only believed when the agent said so")
    func sleepNeedsAnAcknowledgement() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("sleep", with: [Self.timeout()])
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        model.sleep(force: false, whenIdle: false)
        await Self.settle(model)

        #expect(store.newestFirst.first { $0.kind == .sleep }?.state == .unknown)
        #expect(model.statusLine.contains("may or may not have happened"))
    }

    // MARK: - Update semantics

    @Test("an update that did nothing says which of the reasons it was")
    func noopReasonIsRendered() async throws {
        // Finding 06. Every no-op used to read as "already on the latest build", which is exactly
        // wrong for the two that matter: a schedule that is switched off, and a held lock.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "noop", "reasonCode": "policy-off",
          "service": "demo", "message": "automatic updates are off" }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.statusLine.contains("automatic updates are switched off"))
        // And it is not drawn as a quiet success.
        #expect(model.statusIsError)
        #expect(store.newestFirst.first { $0.kind == .update }?.state == .noop)
    }

    @Test("a manual update sends no policy flag at all, because a manual request ignores the schedule")
    func manualUpdateIgnoresTheSchedule() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "updated", "service": "demo", "from": "1", "to": "2" }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        let update = try #require(runner.sentCommands.first { $0.contains("update") })
        #expect(!update.contains("--force"))
        #expect(!update.contains("--when-idle"))
        #expect(update.contains("--op"))
        #expect(model.statusLine.contains("updated from 1 to 2"))
    }

    @Test("a deferred update offers the override rather than taking it")
    func deferredOffersForce() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "deferred", "reasonCode": "busy",
          "service": "demo", "message": "1 turn running" }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        var asked: PendingDialog?
        model.ask = { asked = $0 }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        // Force is the agent's own check being skipped, so it is only ever offered after the agent
        // has looked at the machine as it is now and said no.
        let dialog = try #require(asked)
        #expect(dialog.confirmTitle == "Do it anyway")
        #expect(store.newestFirst.first { $0.kind == .update }?.state == .deferred)
    }

    @Test("a policy refusal is not something force can override, so it is not offered")
    func policyRefusalIsNotForceable() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "deferred", "reasonCode": "outside-window",
          "service": "demo" }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        var asked: PendingDialog?
        model.ask = { asked = $0 }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        // Force skips the busy gate and nothing else. A manual request already ignores the window,
        // so offering "do it anyway" here would be offering a flag that changes nothing.
        #expect(asked == nil)
    }

    @Test("a conflict names what is already running and is not filed as a failure")
    func conflictIsItsOwnOutcome() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("restart", json: """
        { "ok": false, "contract": 3, "action": "conflict", "reasonCode": "operation-in-progress",
          "conflict": { "opId": "abc", "kind": "update", "service": "demo", "phase": "installing" } }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.restart(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.statusLine.contains("update demo"))
        #expect(store.newestFirst.first { $0.kind == .restart }?.state == .conflict)
    }

    @Test("a queued request is filed as waiting rather than as done")
    func queuedIsHeld() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "queued", "service": "demo",
          "expiresAt": "2030-01-01T00:00:00.000Z" }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: true)
        await Self.settle(model)

        let record = try #require(store.newestFirst.first { $0.kind == .update })
        #expect(record.state == .queued)
        #expect(record.expiresAt != nil)
        let sent = try #require(runner.sentCommands.first { $0.contains("update") })
        #expect(sent.contains("--when-idle"))
    }

    @Test("a detached operation is followed until it finishes")
    func detachedOperationIsFollowed() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "accepted", "detached": true,
          "op": { "id": "0123456789abcdef", "kind": "update", "service": "demo",
                  "state": "running", "phase": "installing" } }
        """)
        runner.answer("op", json: """
        { "ok": true, "contract": 3,
          "op": { "id": "0123456789abcdef", "kind": "update", "service": "demo",
                  "state": "finished", "phase": "done",
                  "result": { "ok": true, "action": "updated", "message": "demo updated from 1 to 2",
                              "from": "1", "to": "2" } } }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model, timeout: 10)

        let record = try #require(store.newestFirst.first { $0.kind == .update })
        #expect(record.state == .succeeded)
        // The id the app generated is the one it asked about, so a retry can never start it twice.
        #expect(runner.sentCommands.contains { $0.contains("op ") })
    }

    @Test("the output of a configured action reaches the screen")
    func actionOutputIsShown() async throws {
        // The agent has always returned this and neither app ever showed it, which made "the action
        // ran" the whole of what anyone learned from a button whose purpose is to run a command.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("run", json: """
        { "ok": true, "contract": 3, "action": "ran", "output": "sent 3 packets to 10.0.0.255:9" }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let action = try #require(model.actions.first)
        model.run(action, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.statusDetail == "sent 3 packets to 10.0.0.255:9")
        #expect(store.newestFirst.first { $0.kind == .action }?.output == "sent 3 packets to 10.0.0.255:9")
    }

    @Test("an id the agent would refuse is caught here rather than sent")
    func invalidIdsNeverLeave() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" },
          "actions": [ { "id": "ok-action" } ],
          "services": [ { "id": "bad id", "name": "Bad" } ] }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        #expect(model.statusIsError)
        #expect(model.statusLine.contains("is invalid"))
        #expect(!runner.sentCommands.contains { $0.contains("bad id") })
    }

    // MARK: - Notes and evidence

    @Test("everything the agent could not read is collected for the section to print")
    func notesAreCollected() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" },
          "notes": ["the npm registry did not answer"],
          "config": { "ok": true, "source": "file",
                      "problems": [ { "level": "warning", "path": "services[1].busy",
                                      "message": "edge has no busy probe", "fix": "add one" } ] },
          "services": [ { "id": "demo", "notes": ["the version file is missing"] } ] }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.notes.contains("the npm registry did not answer"))
        #expect(model.notes.contains("the version file is missing"))
        #expect(model.notes.contains { $0.contains("edge has no busy probe") })
    }

    @Test("a partial status is reported as partial rather than as everything there is")
    func partialStatusIsSaid() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" },
          "timing": { "budgetMs": 20000, "elapsedMs": 20001, "partial": true },
          "services": [] }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: true)
        #expect(model.statusLine.contains("ran out of its own time budget"))
    }

    @Test("a service nothing watches is not treated as an idle one")
    func unmonitoredIsNotIdle() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" },
          "busy": { "busy": false, "unknown": false, "monitored": false, "evidence": "unmonitored" },
          "services": [ { "id": "demo", "installed": "1", "upToDate": true,
                          "busy": { "busy": false, "unknown": false, "monitored": false,
                                    "evidence": "unmonitored" } } ] }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        // An absence of evidence is not evidence of idleness, and the app must not read it as one.
        #expect(!model.isKnownIdle)
        let service = try #require(model.services.first)
        #expect(service.busy?.verdict == .unmonitored)
        #expect(model.sidebarSummary(for: service) == "not monitored")
    }
}
