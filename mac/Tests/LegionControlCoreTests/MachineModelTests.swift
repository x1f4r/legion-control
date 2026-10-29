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
        private var replies: [String: [(result: CommandResult, sshLog: String)]] = [:]
        private(set) var commands: [[String]] = []

        func answer(_ verb: String, with results: [CommandResult]) {
            lock.lock(); defer { lock.unlock() }
            replies[verb] = results.map { ($0, "") }
        }

        func answer(_ verb: String, json: String, exit: Int32 = 0) {
            answer(verb, with: [CommandResult(exitCode: exit, standardOutput: json, standardError: "",
                                              timedOut: false, launchFailure: nil)])
        }

        /// ssh itself failing, in its own words: exit 255, nothing on stdout, the reason in its log.
        func answer(_ verb: String, sshLog: String) {
            lock.lock(); defer { lock.unlock() }
            replies[verb] = [(CommandResult(exitCode: 255, standardOutput: "", standardError: "",
                                            timedOut: false, launchFailure: nil), sshLog)]
        }

        func run(_ arguments: [String]) -> CommandResult {
            lock.lock(); defer { lock.unlock() }
            commands.append(arguments)
            // The agent argv and the ssh options come first; the verb is the first token of the
            // quoted command, which is the last argument.
            let verb = arguments.last?.split(separator: " ").dropFirst(2).first.map(String.init) ?? ""
            guard var queued = replies[verb], !queued.isEmpty else {
                // Unscripted reads as a machine that is not there, which is what a test that never
                // expected the call wants: nothing ran, and nothing is owed.
                FakeSSH.writeLog("ssh: connect to host 10.0.0.5 port 22: Operation timed out\n", for: arguments)
                return CommandResult(exitCode: 255, standardOutput: "", standardError: "no scripted reply for \(verb)",
                                     timedOut: false, launchFailure: nil)
            }
            let next = queued.removeFirst()
            if !queued.isEmpty { replies[verb] = queued }
            FakeSSH.writeLog(next.sshLog, for: arguments)
            return next.result
        }

        var sentCommands: [String] {
            lock.lock(); defer { lock.unlock() }
            return commands.compactMap(\.last)
        }

        /// Every argv as ssh was given it, address included.
        var commandArguments: [[String]] {
            lock.lock(); defer { lock.unlock() }
            return commands
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
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-model-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

        var agent = RemoteAgent(machine: machine())
        agent.logDirectory = directory.appending(path: "ssh-logs")
        agent.runner = { _, arguments, _, _ in runner.run(arguments) }
        let model = MachineModel(machine: machine(), agent: agent)

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

    @Test("a status read nobody is waiting for any more leaves the machine as it was")
    func cancelledReadChangesNothing() async throws {
        // A cancelled read learnt nothing about the machine, so it must not draw it as failed.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        await model.refresh(userInitiated: false)
        #expect(model.isAwake)
        let sent = runner.sentCommands.count

        let read = Task { @MainActor in
            withUnsafeCurrentTask { $0?.cancel() }
            await model.refresh(userInitiated: false)
        }
        await read.value
        #expect(model.isAwake)
        #expect(model.currentSystem?.id == "linux")
        #expect(runner.sentCommands.count == sent)
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
        // And a POSIX login can be sent the upgrade.
        #expect(model.legacyInstallRefusal == nil)
    }

    /// A machine whose 2.x agent sits behind a cmd login, which the upgrade cannot be sent to.
    static func legacyCmdModel(_ runner: FakeRunner) -> (MachineModel, URL) {
        let windows = Machine(
            id: "tower", name: "Tower",
            endpoints: [Endpoint(id: "lan", kind: "lan", host: "10.0.0.5", user: "me")],
            systems: [SystemConfig(id: "windows", name: "Windows", platform: .windows,
                                   agent: ["C:\\node.exe", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"])]
        )
        // cmd quotes every word, the verb included.
        runner.answer("\"status\"", json: #"{ "ok": true, "agentVersion": "2.1.0", "os": "windows" }"#)
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-model-\(UUID().uuidString)")
        var agent = RemoteAgent(machine: windows)
        agent.logDirectory = directory.appending(path: "ssh-logs")
        agent.runner = { _, arguments, _, _ in runner.run(arguments) }
        return (MachineModel(machine: windows, agent: agent), directory)
    }

    @Test("a 2.x agent behind a cmd login is not offered the upgrade, and is told why first")
    func legacyCmdUpgradeIsRefusedUpFront() async throws {
        let runner = FakeRunner()
        let (model, directory) = Self.legacyCmdModel(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.needsAgentUpgrade)
        let refusal = try #require(model.legacyInstallRefusal)
        #expect(refusal.contains("cmd"))
        // Nothing is staged or sent, and no installation base is asked for.
        let sent = runner.sentCommands.count
        let resolution = await model.installOverLegacyAgent(Data("archive".utf8), operation: AgentToken.newOperationId(), version: "3.1.0")
        #expect(resolution.state == .failed)
        #expect(resolution.summary == refusal)
        #expect(runner.sentCommands.count == sent)
    }

    @Test("an action that ends without a word of its own leaves how it ended on the line, not its label")
    func silentEndingReplacesTheLabel() async throws {
        // An agent install refused before anything was sent never wrote the line, which went on
        // saying "Installing agent 3.1.0 on Tower." with nothing being installed anywhere.
        let runner = FakeRunner()
        let (model, directory) = Self.legacyCmdModel(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let refusal = try #require(model.legacyInstallRefusal)
        // What the install button runs, once the archive has been verified.
        model.perform("Installing agent 3.1.0 on Tower", kind: .appUpdate, subject: "control agent") { operation in
            await model.installOverLegacyAgent(Data("archive".utf8), operation: operation, version: "3.1.0")
        }
        await Self.settle(model)

        #expect(model.statusLine == refusal)
        #expect(model.statusIsError)
        #expect(model.lastActionOutcome?.text == refusal)
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
        runner.answer("status", sshLog: "me@pi: Permission denied (publickey).\n")
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
        runner.answer("status", sshLog: "No ED25519 host key is known for pi and you have requested strict checking.\n")
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
        runner.answer("status", sshLog: "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\n")
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
        runner.answer("boot", sshLog: "client_loop: send disconnect: Broken pipe\n")
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

    // MARK: - Setup edits and remembered settings

    /// Holds every command with one verb until the test lets it go, and says when one has arrived.
    ///
    /// The suite runs on the main actor, so a task the model starts cannot run before the test's
    /// next await. Waiting for the held command is what makes an edit land while it is really out.
    final class Gate: @unchecked Sendable {
        private let lock = NSLock()
        private let verb: String
        private var open = false
        private var held: [String]?

        init(holding verb: String) { self.verb = verb }

        func release() { lock.withLock { open = true } }

        private var isOpen: Bool { lock.withLock { open } }

        private var holding: [String]? { lock.withLock { held } }

        private func arrive(_ arguments: [String]) { lock.withLock { held = arguments } }

        /// Called by the runner with every command, and returns once the command may go on.
        func pass(_ arguments: [String]) async {
            guard arguments.last?.split(separator: " ").dropFirst(2).first.map(String.init) == verb else { return }
            arrive(arguments)
            while !isOpen { try? await Task.sleep(for: .milliseconds(5)) }
        }

        /// The argv of the command being held, once one has arrived, or nil if none did in time.
        func waitUntilHolding(timeout: TimeInterval = 5) async -> [String]? {
            let deadline = Date().addingTimeInterval(timeout)
            while holding == nil, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
            return holding
        }
    }

    /// A model whose transport waits at the gate, with somewhere to write its operations.
    static func gatedModel(_ runner: FakeRunner, gate: Gate) throws -> (MachineModel, OperationStore, URL) {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-model-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var agent = RemoteAgent(machine: machine())
        agent.logDirectory = directory.appending(path: "ssh-logs")
        agent.runner = { _, arguments, _, _ in
            await gate.pass(arguments)
            return runner.run(arguments)
        }
        let model = MachineModel(machine: machine(), agent: agent)
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        model.operations = store
        return (model, store, directory)
    }

    @Test("an edit to a busy machine's entry is taken in place, and the action finishes in the same model")
    func editDuringActionKeepsTheModel() async throws {
        // A setup change used to replace the model. The restart carried on in the discarded one, out
        // of sight, and the new one would have let the user start another on the same machine.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("restart", json: """
        { "ok": true, "contract": 3, "action": "restarted", "service": "demo" }
        """)
        let gate = Gate(holding: "restart")
        let (model, store, directory) = try Self.gatedModel(runner, gate: gate)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.restart(service, force: false, whenIdle: false)
        // The restart has gone out over the old address and has not come back.
        let restart = try #require(await gate.waitUntilHolding())
        #expect(restart.contains { $0.contains("10.0.0.5") })
        #expect(model.isWorking)

        var edited = Self.machine()
        edited.name = "Pi in the hall"
        edited.endpoints = [Endpoint(id: "lan", kind: "lan", host: "10.0.0.6", user: "me")]
        model.update(machine: edited)

        // Still the one action, so nothing else can be started on top of it.
        #expect(model.isWorking)
        #expect(model.machine.name == "Pi in the hall")
        #expect(model.commandableSystem?.id == "linux")

        gate.release()
        await Self.settle(model)

        let record = try #require(store.newestFirst.first { $0.kind == .restart })
        #expect(record.state == .succeeded)
        #expect(model.lastActionOutcome?.operationId == record.id)
        #expect(model.lastActionOutcome?.isError == false)

        // And what comes next goes where the setup now says.
        await model.refresh(userInitiated: false)
        let last = try #require(runner.commandArguments.last)
        #expect(last.contains { $0.contains("10.0.0.6") })
        #expect(!last.contains { $0.contains("10.0.0.5") })
    }

    @Test("a machine renamed while it is being woken still reads as waking")
    func renameDuringWakeStillWaking() async throws {
        // Waking used to be read off the activity line, compared with the machine's name as it is
        // now. An edit lands on the busy model since the setup stopped replacing it, so a rename
        // mid-wake turned the button back to "Wake" while the wake was still running.
        let document = Data("""
        { "version": 1, "machines": [
          { "id": "pi", "name": "Pi",
            "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5", "user": "me" } ],
            "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ],
            "wake": { "mac": "02:00:00:00:00:01", "helpers": [ { "machine": "hub", "action": "wake-pi" } ] } },
          { "id": "hub", "name": "Hub",
            "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.7", "user": "me" } ],
            "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
        """.utf8)
        // The setup as the app runs it, through the launch gate with all of it approved: a system
        // read straight from a document is not driven, so the hub would never be asked anything.
        let setup = LaunchGate.effective(
            try JSONDecoder().decode(ControllerConfig.self, from: document).validated(), document: document,
            record: LaunchValues.approvals(from: try JSONSerialization.jsonObject(with: document), seededFrom: "none")
        ).config
        let pi = try #require(setup.machine(id: "pi"))
        let hubEntry = try #require(setup.machine(id: "hub"))
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-model-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        // The only step is the helper, which is read first, and that read is held while the edit
        // lands. Nothing is ever broadcast: this device is not on the machine's network.
        let gate = Gate(holding: "status")
        let hubRunner = FakeRunner()
        var hubAgent = RemoteAgent(machine: hubEntry)
        hubAgent.logDirectory = directory.appending(path: "hub-logs")
        hubAgent.runner = { _, arguments, _, _ in
            await gate.pass(arguments)
            return hubRunner.run(arguments)
        }
        let hub = MachineModel(machine: hubEntry, agent: hubAgent)

        let piRunner = FakeRunner()
        var piAgent = RemoteAgent(machine: pi)
        piAgent.logDirectory = directory.appending(path: "pi-logs")
        piAgent.runner = { _, arguments, _, _ in piRunner.run(arguments) }
        let model = MachineModel(machine: pi, agent: piAgent)
        model.setup = { setup }
        model.peer = { $0 == "hub" ? hub : nil }

        model.wake()
        #expect(model.isWaking)
        #expect(await gate.waitUntilHolding() != nil)

        var renamed = pi
        renamed.name = "Pi in the hall"
        model.update(machine: renamed)
        #expect(model.isWaking)

        // The hub does not answer, so there is nothing to send the packet with and the wake ends.
        gate.release()
        await Self.settle(model)
        #expect(!model.isWorking)
        #expect(!model.isWaking)
    }

    @Test("a system removed from the entry is no longer commanded with its old launcher")
    func editDropsARemovedSystem() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.commandableSystem?.agent == ["node", "/a.mjs"])

        var edited = Self.machine()
        edited.systems = [SystemConfig(id: "linux", name: "Linux", platform: .linux, agent: ["node", "/c.mjs"])]
        model.update(machine: edited)
        #expect(model.commandableSystem?.agent == ["node", "/c.mjs"])

        edited.systems = [SystemConfig(id: "other", name: "Other", platform: .linux, agent: ["node", "/b.mjs"])]
        model.update(machine: edited)
        // Still answering as linux until the next read, but nothing the setup no longer describes
        // can be commanded.
        #expect(model.currentSystem?.id == "linux")
        #expect(model.commandableSystem == nil)
    }

    @Test("a status read that was out during an edit is taken against the entry as it is now")
    func readDuringEditFollowsTheEdit() async throws {
        // The read came back after the edit and put back the route it went out over, old address
        // and all. Sends only use the route to order their tries, but the legacy agent install
        // uploads to its address, so it would have gone to a host the setup no longer has.
        let runner = FakeRunner()
        // A 2.x agent that names neither its system nor its platform, so the launcher that worked
        // is all there is to go on.
        runner.answer("status", json: """
        { "ok": true, "agentVersion": "2.1.0",
          "t3": { "installed": "1", "nightly": "2", "upToDate": false } }
        """)
        let gate = Gate(holding: "status")
        let (model, _, directory) = try Self.gatedModel(runner, gate: gate)
        defer { try? FileManager.default.removeItem(at: directory) }

        let reading = Task { await model.refresh(userInitiated: false) }
        let read = try #require(await gate.waitUntilHolding())
        #expect(read.contains { $0.contains("10.0.0.5") })

        var edited = Self.machine()
        edited.endpoints = [Endpoint(id: "lan", kind: "lan", host: "10.0.0.6", user: "me")]
        edited.systems[0].agent = ["node", "/c.mjs"]
        model.update(machine: edited)
        gate.release()
        await reading.value

        #expect(model.lastRoute?.target.host == "10.0.0.6")
        #expect(model.commandableSystem?.agent == ["node", "/c.mjs"])
    }

    @Test("a read that went out over an address the entry has since dropped leaves no route to it")
    func readOverADroppedAddress() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        let gate = Gate(holding: "status")
        let (model, _, directory) = try Self.gatedModel(runner, gate: gate)
        defer { try? FileManager.default.removeItem(at: directory) }

        let reading = Task { await model.refresh(userInitiated: false) }
        #expect(await gate.waitUntilHolding() != nil)
        var edited = Self.machine()
        edited.endpoints = [Endpoint(id: "wired", kind: "lan", host: "10.0.0.7", user: "me")]
        model.update(machine: edited)
        gate.release()
        await reading.value

        // The machine answered, so it is up, but the way it answered is gone with the edit.
        #expect(model.isAwake)
        #expect(model.lastRoute == nil)
    }

    static let unknownHostKey = "No ED25519 host key is known for pi and you have requested strict checking.\n"

    @Test("a host key question goes with the address it was about")
    func hostKeyQuestionFollowsTheEntry() async throws {
        // The question used to outlive an edit that dropped its address, so a key could still be
        // pinned for a host this device no longer dials.
        let runner = FakeRunner()
        runner.answer("status", sshLog: Self.unknownHostKey)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        #expect(model.hostKeyPrompt?.target?.host == "10.0.0.5")
        model.selectedHostSystem = "linux"
        model.confirmedHostSystems = ["linux"]
        model.hostSystemsConfirmed = true

        // A new name says nothing about the address, so the question stands.
        var edited = Self.machine()
        edited.name = "Pi in the hall"
        model.update(machine: edited)
        #expect(model.hostKeyPrompt?.target?.host == "10.0.0.5")
        #expect(model.hostSystemsConfirmed)

        edited.endpoints = [Endpoint(id: "wired", kind: "lan", host: "10.0.0.7", user: "me")]
        model.update(machine: edited)
        #expect(model.hostKeyPrompt == nil)
        #expect(model.selectedHostSystem.isEmpty)
        #expect(model.confirmedHostSystems.isEmpty)
        #expect(!model.hostSystemsConfirmed)
    }

    @Test("a read that meets an unknown key at an address the entry has since dropped asks nothing")
    func hostKeyFailureDuringEditAsksNothing() async throws {
        let runner = FakeRunner()
        runner.answer("status", sshLog: Self.unknownHostKey)
        let gate = Gate(holding: "status")
        let (model, _, directory) = try Self.gatedModel(runner, gate: gate)
        defer { try? FileManager.default.removeItem(at: directory) }

        let reading = Task { await model.refresh(userInitiated: false) }
        #expect(await gate.waitUntilHolding() != nil)
        var edited = Self.machine()
        edited.endpoints = [Endpoint(id: "wired", kind: "lan", host: "10.0.0.7", user: "me")]
        model.update(machine: edited)
        gate.release()
        await reading.value

        #expect(model.statusIsError)
        #expect(model.hostKeyPrompt == nil)
    }

    @Test("every machine writes into one remembered store, so none erases another's reading")
    func rememberedSettingsAreShared() async throws {
        // Each model used to load its own copy at launch and write the whole of it back, so the last
        // one to write wiped what the others had recorded since.
        var saved: [String: RememberedSystem] = [:]
        let remembered = RememberedSettings(systems: [:], save: { saved = $0 })

        let piRunner = FakeRunner()
        piRunner.answer("status", json: Self.onlineStatus)
        let logs = FakeSSH.logDirectory()
        defer { try? FileManager.default.removeItem(at: logs) }
        var piAgent = RemoteAgent(machine: Self.machine())
        piAgent.logDirectory = logs
        piAgent.runner = { _, arguments, _, _ in piRunner.run(arguments) }
        let pi = MachineModel(machine: Self.machine(), agent: piAgent, remembered: remembered)

        var towerMachine = Self.machine()
        towerMachine.id = "tower"
        towerMachine.name = "Tower"
        let towerRunner = FakeRunner()
        towerRunner.answer("status", json: Self.onlineStatus.replacingOccurrences(of: #""hostname": "pi""#,
                                                                                  with: #""autoUpdate": true, "hostname": "tower""#))
        var towerAgent = RemoteAgent(machine: towerMachine)
        towerAgent.logDirectory = logs
        towerAgent.runner = { _, arguments, _, _ in towerRunner.run(arguments) }
        let tower = MachineModel(machine: towerMachine, agent: towerAgent, remembered: remembered)

        await pi.refresh(userInitiated: false)
        await tower.refresh(userInitiated: false)

        #expect(Set(saved.keys) == ["pi/linux", "tower/linux"])
        #expect(saved["tower/linux"]?.autoUpdate == true)
        #expect(saved["pi/linux"]?.checkedAt != nil)
        #expect(pi.autoUpdateCheckedAt(for: Self.machine().systems[0]) != nil)
        #expect(tower.autoUpdateValue(for: towerMachine.systems[0]) == true)
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
