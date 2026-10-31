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

    @Test("an agent older than the one this app carries says so where the machine's state is read")
    func olderAgentIsSaid() async throws {
        // A 3.0 agent speaks contract 3, so nothing said it was a release behind: the install was a
        // button under Diagnostics, and the whole fleet stayed on 3.0 after 3.1 shipped.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        model.core.bundledAgentVersion = "3.1.0"
        #expect(model.newerBundledAgent == nil)

        await model.refresh(userInitiated: false)
        #expect(!model.needsAgentUpgrade)
        #expect(model.newerBundledAgent == "3.1.0")
        #expect(model.newerBundledAgentLine == "Agent 3.0.0 is older than the 3.1.0 this app carries.")
        #expect(model.sidebarSummary == "Linux, agent 3.1.0 available")

        // The same agent, or a build that carries none, has nothing to offer.
        model.core.bundledAgentVersion = "3.0.0"
        #expect(model.newerBundledAgent == nil)
        #expect(model.sidebarSummary == "Linux")
        model.core.bundledAgentVersion = nil
        #expect(model.newerBundledAgentLine == nil)
    }

    @Test("an agent too old for the contract says that first, with the install beside it")
    func legacyAgentIsOfferedTheBundle() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: """
        { "ok": true, "agentVersion": "2.1.0", "os": "linux",
          "t3": { "installed": "1", "nightly": "2", "upToDate": false } }
        """)
        let (model, _, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        model.core.bundledAgentVersion = "3.1.0"

        await model.refresh(userInitiated: false)
        #expect(model.sidebarSummary == "Linux, agent too old")
        #expect(model.newerBundledAgentLine == "Agent 2.1.0 is too old for operations, policy and queue controls. This app carries 3.1.0.")
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

    // MARK: - Queued requests

    /// A sleep this app saw queued on the Pi's Linux, until `expiry`.
    static func queuedSleep(in store: OperationStore, expiry: Date?, system: String = "linux") -> OperationRecord {
        let record = OperationRecord(id: AgentToken.newOperationId(), machineId: "pi", machineName: "Pi", systemId: system,
                                     kind: .sleep, subject: "Pi", summary: "Putting Pi to sleep.", agentTracked: true)
        store.begin(record)
        store.markQueued(record.id, expiresAt: expiry, summary: "The sleep is queued and runs the next time Pi is idle.")
        return record
    }

    static func status(listing operation: String) -> String {
        onlineStatus.replacingOccurrences(of: #""controller": null"#, with: #""operations": \#(operation), "controller": null"#)
    }

    static func waitWhile(_ condition: @MainActor () -> Bool, timeout: TimeInterval = 15) async {
        let deadline = Date().addingTimeInterval(timeout)
        while condition(), Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }
    }

    @Test("a queued request its machine has pruned is asked about by id once the machine answers, and expires")
    func prunedQueuedRequestExpires() async throws {
        // Two sleeps queued on 22 October read "waiting for idle" nine days later. Both agents had
        // expired and pruned them, and nothing ever asked: the records were tracked and owed nothing.
        let runner = FakeRunner()
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let expiry = Date().addingTimeInterval(-9 * 24 * 60 * 60)
        let record = Self.queuedSleep(in: store, expiry: expiry)

        // While the machine cannot be reached it is left exactly as it is.
        await model.refresh(userInitiated: false)
        #expect(!model.isAwake)
        #expect(store.record(id: record.id)?.state == .queued)
        #expect(!runner.sentCommands.contains { $0.contains(record.id) })

        // Agent 3.0.x says it has no record with bad-argument and op null.
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("op", json: #"{"ok":false,"contract":3,"reasonCode":"bad-argument","command":"op","op":null}"#, exit: 1)
        model.allowRedial()
        await model.refresh(userInitiated: false)
        await Self.waitWhile { store.record(id: record.id)?.state == .queued }

        #expect(runner.sentCommands.contains { $0.contains("op") && $0.contains(record.id) })
        let settled = try #require(store.record(id: record.id))
        #expect(settled.state == .expired)
        #expect(!settled.needsReconciliation)
        #expect(settled.finishedAt == expiry)
        #expect(settled.summary.hasSuffix("Pi no longer has a record of it, so whether it ran before then is not known."))
    }

    @Test("a queued request the status lists is settled from it, and never asked of another system")
    func queuedRequestFromTheStatus() async throws {
        let runner = FakeRunner()
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let waiting = Self.queuedSleep(in: store, expiry: Date().addingTimeInterval(3600))
        let ran = Self.queuedSleep(in: store, expiry: Date().addingTimeInterval(-60))
        // Queued on the other system, which is not the one answering: Linux has never heard of it.
        let elsewhere = Self.queuedSleep(in: store, expiry: Date().addingTimeInterval(-60), system: "other")

        runner.answer("status", json: Self.status(listing: """
        { "queued": [ { "id": "\(waiting.id)", "kind": "sleep", "state": "queued", "outcome": "pending" } ],
          "recent": [ { "id": "\(ran.id)", "kind": "sleep", "state": "finished", "phase": "done",
                        "action": "expired", "reasonCode": "expired", "outcome": "deferred" } ] }
        """))
        runner.answer("op", json: #"{"ok":false,"contract":3,"reasonCode":"not-configured","op":null}"#, exit: 1)
        await model.refresh(userInitiated: false)
        // Long enough for a read by id to have gone out, had one been started.
        try? await Task.sleep(for: .milliseconds(300))

        #expect(store.record(id: waiting.id)?.state == .queued)
        #expect(store.record(id: ran.id)?.state == .expired)
        #expect(store.record(id: ran.id)?.summary == "The queued request for Pi expired before the machine was idle.")
        #expect(store.record(id: elsewhere.id)?.state == .queued)
        #expect(!runner.sentCommands.contains { $0.contains("op ") })
    }

    @Test("an unknown outcome is never settled by a system it was not sent to")
    func unknownOutcomeWaitsForItsSystem() async throws {
        // A boot from Linux whose answer was lost, read after the machine came up in the other
        // system: that agent has never heard of the id, which is no sign the boot never started.
        let runner = FakeRunner()
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        let record = OperationRecord(id: AgentToken.newOperationId(), machineId: "pi", machineName: "Pi", systemId: "other",
                                     kind: .boot, subject: "Linux", summary: "Switching to Linux.", agentTracked: true)
        store.begin(record)
        store.finish(record.id, state: .unknown, summary: "The switch to Linux may or may not have happened.")

        runner.answer("status", json: Self.onlineStatus)
        runner.answer("op", json: #"{"ok":false,"contract":3,"reasonCode":"not-configured","op":null}"#, exit: 1)
        await model.refresh(userInitiated: false)
        try? await Task.sleep(for: .milliseconds(300))

        #expect(!runner.sentCommands.contains { $0.contains("op ") })
        #expect(store.record(id: record.id)?.state == .unknown)
        #expect(store.record(id: record.id)?.needsReconciliation == true)
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

    @Test("an update whose polls cannot get through is still running there, never failed here")
    func failedPollIsNotAFailedUpdate() async throws {
        // A poll is a read, and a read that never left this device is "never sent". Thrown back as
        // the update's own failure, that filed an update running perfectly well over there as one
        // that did not happen (C5). The record stays running and owed, and is read again later.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": true, "contract": 3, "action": "accepted", "detached": true, "outcome": "pending",
          "op": { "id": "0123456789abcdef", "kind": "update", "service": "demo",
                  "state": "running", "phase": "installing", "outcome": "pending" } }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        model.core.pollPause = { _ in }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model, timeout: 10)

        let record = try #require(store.newestFirst.first { $0.kind == .update })
        #expect(record.state == .running)
        #expect(record.needsReconciliation)
        #expect(model.statusLine.contains("still running"))
        // Asked about by its id, and never sent again. Every try really went out: the address that
        // did not answer the first poll is otherwise left alone for a minute, and the other two
        // failed without leaving this Mac.
        #expect(runner.sentCommands.filter { $0.contains(" op ") }.count == OperationDriver.pollTries)
        #expect(runner.sentCommands.filter { $0.contains(" update") }.count == 1)
    }

    @Test("an outcome the agent states as unknown is owed and read again, whatever the action says")
    func statedUnknownIsOwed() async throws {
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": false, "contract": 3, "action": "rolled-back", "reasonCode": "rolled-back", "outcome": "unknown",
          "message": "the new build did not come up and the old one may not be back either" }
        """)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        let record = try #require(store.newestFirst.first { $0.kind == .update })
        #expect(record.state == .unknown)
        #expect(record.needsReconciliation)
        #expect(model.statusLine.contains("may or may not have taken effect"))
        #expect(model.statusLine.contains("may not be back"))
        #expect(runner.sentCommands.filter { $0.contains(" update") }.count == 1)
    }

    @Test("an update that failed at its stop step says it would not stop, never that the service is not running")
    func stopFailureIsWordedFromTheReply() async throws {
        // Agent 3.1.0 answers a stop that did not take with not-running, the code for a service in
        // the wrong state either way, and its sentence alone said the opposite of what happened.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("update", json: """
        { "ok": false, "contract": 3, "action": "failed", "reasonCode": "not-running", "outcome": "failed", "from": "1", "to": "2",
          "message": "Demo would not stop: Demo was still running 20 s after the stop request" }
        """, exit: 1)
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let service = try #require(model.services.first)
        model.update(service, force: false, whenIdle: false)
        await Self.settle(model)

        let record = try #require(store.newestFirst.first { $0.kind == .update })
        #expect(record.state == .failed)
        #expect(record.summary == "Demo would not stop: Demo was still running 20 s after the stop request")
        #expect(!record.summary.contains("is not running"))
        #expect(model.lastActionOutcome?.isError == true)
        // With nothing more to go on, the code still says what it means.
        #expect(AgentReason.notRunning.sentence(subject: "Demo", message: nil) == "Demo is not running.")
        #expect(AgentReason.notRunning.sentence(subject: "Demo", message: "") == "Demo is not running.")
    }

    @Test("a cancel that finds the request already running cancels nothing, though ok is true")
    func cancelThatFindsItRunning() async throws {
        // The reply is `conflict` with `ok: true`, and the cancel used to read ok alone: the queued
        // row was filed as cancelled while the update it was meant to stop went on running.
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("cancel", json: String(decoding: try ContractCorpus.data("fixtures/cancel.conflict-running.json"), as: UTF8.self))
        let (model, store, directory) = Self.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }

        await model.refresh(userInitiated: false)
        let id = "6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3"
        store.begin(OperationRecord(id: id, machineId: "pi", machineName: "Pi", kind: .update, subject: "Demo",
                                    summary: "Updating Demo.", agentTracked: true))
        store.markQueued(id, expiresAt: nil, summary: "queued")
        var queued = AgentOperation()
        queued.rawId = id
        queued.kind = "update"
        queued.state = "queued"
        model.cancelQueued(queued)
        await Self.settle(model)

        #expect(store.record(id: id)?.state == .queued)
        #expect(model.statusLine.contains("already running"))
        #expect(store.newestFirst.first { $0.kind == .queue }?.state == .conflict)
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

    // MARK: - Waking

    /// A tower woken directly on 192.0.2.0/24 or through two helpers, through the launch gate with
    /// all of it approved, as the app runs a setup.
    static let wakeDocument = Data("""
    { "version": 1, "machines": [
      { "id": "tower", "name": "Tower",
        "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.0.2.20", "user": "me" } ],
        "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] },
                     { "id": "other", "platform": "linux", "agent": ["node", "/b.mjs"] } ],
        "wake": { "mac": "02:00:00:00:00:01", "lanPrefix": "192.0.2.",
                  "helpers": [ { "machine": "pi", "action": "wake-tower" }, { "machine": "nas", "action": "wake-tower" } ] } },
      { "id": "pi", "name": "Pi",
        "endpoints": [ { "id": "lan", "kind": "lan", "host": "198.51.100.5", "user": "me" } ],
        "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
      { "id": "nas", "name": "NAS",
        "endpoints": [ { "id": "lan", "kind": "lan", "host": "198.51.100.9", "user": "me" } ],
        "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
    """.utf8)

    static func wakeSetup(_ document: Data = wakeDocument) throws -> ControllerConfig {
        LaunchGate.effective(
            try ControllerConfig.accepted(document), document: document,
            record: LaunchValues.approvals(from: try JSONSerialization.jsonObject(with: document), seededFrom: "none")
        ).config
    }

    /// A helper's status, offering the wake action with the kind given.
    static func helperStatus(_ kind: String) -> String {
        """
        { "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux" },
          "agent": { "version": "3.1.0", "contract": 3 }, "services": [],
          "actions": [ { "id": "wake-tower", "name": "Wake Tower", "kind": "\(kind)" } ] }
        """
    }

    static let towerStatus = """
    { "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux" },
      "agent": { "version": "3.1.0", "contract": 3 }, "services": [] }
    """

    /// A clock that moves only when a readiness poll waits, and remembers each wait.
    final class FakeClock {
        var now = Date(timeIntervalSince1970: 1_000_000)
        var waits: [TimeInterval] = []

        @MainActor var clock: WakeClock {
            WakeClock(now: { self.now }, sleep: { seconds in
                self.waits.append(seconds)
                self.now = self.now.addingTimeInterval(seconds)
            })
        }
    }

    /// Three machine models on one setup and one history: the tower and its two helpers.
    struct WakeRig {
        var tower: MachineModel
        var pi: MachineModel
        var nas: MachineModel
        var runners: [String: FakeRunner]
        var clock: FakeClock
    }

    static func wakeRig(store: OperationStore, logs: URL, setup: ControllerConfig,
                        here: SiteAwareness.Placement = .nowhere) throws -> WakeRig {
        var runners: [String: FakeRunner] = [:]
        var models: [String: MachineModel] = [:]
        for id in ["tower", "pi", "nas"] {
            let entry = try #require(setup.machine(id: id))
            let runner = FakeRunner()
            var agent = RemoteAgent(machine: entry)
            agent.logDirectory = logs.appending(path: id)
            agent.runner = { _, arguments, _, _ in runner.run(arguments) }
            let model = MachineModel(machine: entry, agent: agent)
            model.operations = store
            model.core.pollPause = { _ in }
            model.setup = { setup }
            model.placement = { here }
            model.freshPlacement = { here }
            runners[id] = runner
            models[id] = model
        }
        let clock = FakeClock()
        let tower = models["tower"]!
        tower.peer = { models[$0] }
        tower.wakeClock = clock.clock
        tower.sendWakePackets = { _ in "the tests send no packets" }
        return WakeRig(tower: tower, pi: models["pi"]!, nas: models["nas"]!, runners: runners, clock: clock)
    }

    static func runs(_ runner: FakeRunner?) -> Int {
        runner?.sentCommands.filter { $0.contains(" run wake-tower") }.count ?? 0
    }

    @Test("an ambiguous command holds the target across a restart, until its own record says it finished")
    func wakeHoldScenario() async throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appending(path: "operations.json")
        let setup = try Self.wakeSetup()

        // The Pi takes the command and answers with nothing this app can read: it may well be
        // running. The NAS is never asked, because a second helper's command could wake the tower
        // twice over, or do whatever else its command does twice (section 10.2).
        let first = try Self.wakeRig(store: OperationStore(url: url), logs: directory, setup: setup)
        first.runners["pi"]?.answer("status", json: Self.helperStatus("command"))
        first.runners["pi"]?.answer("run", with: [FakeSSH.result(exit: 1, out: "Segmentation fault")])
        first.runners["nas"]?.answer("status", json: Self.helperStatus("command"))
        first.tower.wake()
        await Self.settle(first.tower, timeout: 10)
        #expect(Self.runs(first.runners["pi"]) == 1)
        #expect(Self.runs(first.runners["nas"]) == 0)
        #expect(first.runners["pi"]?.sentCommands.contains { $0.contains(" op ") && $0.contains("--wait 20") } == true, "\(first.runners["pi"]?.sentCommands ?? [])")
        #expect(first.tower.statusDetail?.contains("NAS: a helper may still be running a command") == true)

        // The app restarts and the Pi is unreachable. The hold is still there; the NAS's command
        // is still not run, but a NAS that declares a magic packet is.
        let store = OperationStore(url: url)
        let hold = try #require(store.wakeHolds(target: "tower").first)
        #expect(hold.machineId == "pi")
        let second = try Self.wakeRig(store: store, logs: directory, setup: setup)
        second.runners["nas"]?.answer("status", json: Self.helperStatus("command"))
        await second.pi.refresh(userInitiated: false)
        await second.nas.refresh(userInitiated: false)
        second.tower.wake()
        await Self.settle(second.tower, timeout: 10)
        #expect(Self.runs(second.runners["nas"]) == 0)
        #expect(store.wakeHolds(target: "tower").count == 1)

        second.runners["nas"]?.answer("status", json: Self.helperStatus("wol"))
        second.runners["nas"]?.answer("run", json: #"{"ok":true,"contract":3,"action":"ran","output":"magic packet sent"}"#)
        await second.nas.refresh(userInitiated: false)
        second.tower.wake()
        await Self.settle(second.tower, timeout: 10)
        #expect(Self.runs(second.runners["nas"]) == 1)
        #expect(store.wakeHolds(target: "tower").count == 1)

        // The Pi is back and its record says the command finished: the next press reads that
        // first, the hold ends, and the NAS's command runs again.
        let third = try Self.wakeRig(store: store, logs: directory, setup: setup)
        third.runners["pi"]?.answer("status", json: Self.helperStatus("command"))
        third.runners["pi"]?.answer("op", json: """
        { "ok": true, "contract": 3, "op": { "id": "\(hold.id)", "kind": "run", "actionId": "wake-tower",
          "state": "finished", "result": { "ok": true, "action": "ran" } } }
        """)
        third.runners["pi"]?.answer("run", json: #"{"ok":true,"contract":3,"action":"ran"}"#)
        third.runners["nas"]?.answer("status", json: Self.helperStatus("command"))
        third.runners["nas"]?.answer("run", json: #"{"ok":true,"contract":3,"action":"ran"}"#)
        await third.pi.refresh(userInitiated: false)
        await third.nas.refresh(userInitiated: false)
        third.tower.wake()
        await Self.settle(third.tower, timeout: 10)
        #expect(store.wakeHolds(target: "tower").isEmpty)
        #expect(Self.runs(third.runners["nas"]) == 1)
    }

    @Test("a helper that cannot say whether its command ran holds the target, and the next helper's command is not run")
    func unknownHelperAnswerHolds() async throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let rig = try Self.wakeRig(store: store, logs: directory, setup: try Self.wakeSetup())
        // The Pi answers, and what it says is that it failed while writing the record: its command
        // may be running. That used to read as the Pi's answer, so the walk went on to the NAS and
        // ran its command beside it.
        rig.runners["pi"]?.answer("status", json: Self.helperStatus("command"))
        rig.runners["pi"]?.answer("run", json: String(decoding: try ContractCorpus.data("fixtures/error.internal.json"), as: UTF8.self),
                                  exit: 1)
        rig.runners["nas"]?.answer("status", json: Self.helperStatus("command"))
        rig.runners["nas"]?.answer("run", json: #"{"ok":true,"contract":3,"action":"ran"}"#)
        rig.tower.wake()
        await Self.settle(rig.tower, timeout: 10)
        #expect(Self.runs(rig.runners["pi"]) == 1)
        #expect(Self.runs(rig.runners["nas"]) == 0)
        #expect(store.wakeHolds(target: "tower").map(\.machineId) == ["pi"])
        #expect(rig.runners["pi"]?.sentCommands.contains { $0.contains(" op ") && $0.contains("--wait 20") } == true)
        #expect(rig.tower.statusDetail?.contains("Pi: wake-tower may or may not have run: the agent failed while writing") == true,
                "\(rig.tower.statusDetail ?? "")")
        #expect(rig.tower.statusDetail?.contains("NAS: a helper may still be running a command") == true)
    }

    @Test("a 2.x helper's hold waits for the target to answer, and a person can let it go")
    func legacyHelperHold() async throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let rig = try Self.wakeRig(store: store, logs: directory, setup: try Self.wakeSetup())
        // A 2.x helper keeps no records, so its hold is never asked about.
        store.begin(OperationRecord(machineId: "pi", machineName: "Pi", kind: .action, subject: "wake-tower",
                                    summary: "Asking", agentTracked: false, wakeTarget: "tower"))
        rig.runners["pi"]?.answer("status", json: Self.helperStatus("command"))
        await rig.pi.refresh(userInitiated: false)
        #expect(rig.tower.wakePlan.isEmpty)
        #expect(rig.tower.canTryWaking)
        rig.tower.wake()
        await Self.settle(rig.tower, timeout: 10)
        #expect(Self.runs(rig.runners["pi"]) == 0)
        #expect(!(rig.runners["pi"]?.sentCommands.contains { $0.contains(" op ") } ?? true))
        #expect(store.wakeHolds(target: "tower").count == 1)

        // The tower answering ends it.
        rig.runners["tower"]?.answer("status", json: Self.towerStatus)
        await rig.tower.refresh(userInitiated: false)
        #expect(store.wakeHolds(target: "tower").isEmpty)

        // And so does a person, who has looked and decided to try the others anyway. The NAS is
        // the other one; the Pi's command is not run a second time.
        store.begin(OperationRecord(machineId: "pi", machineName: "Pi", kind: .action, subject: "wake-tower",
                                    summary: "Asking", agentTracked: false, wakeTarget: "tower"))
        #expect(rig.tower.wakeHoldOffer == .otherHelpers)
        rig.runners["pi"]?.answer("run", json: #"{"ok":true,"contract":2,"action":"ran"}"#)
        rig.tower.tryOtherHelpers()
        await Self.settle(rig.tower, timeout: 10)
        #expect(store.wakeHolds(target: "tower").isEmpty)
        #expect(Self.runs(rig.runners["pi"]) == 0)
    }

    static let ran = #"{"ok":true,"contract":3,"action":"ran"}"#

    @Test("trying the other helpers asks only the others, after a question, and does not run the held command again")
    func tryingTheOtherHelpersAsksOnlyTheOthers() async throws {
        // The label said the other helpers, and the Mac used to release every hold and walk the
        // whole plan, so the held helper's command ran again beside the first: with a command that
        // toggles a smart plug, the second run undoes the first.
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let rig = try Self.wakeRig(store: store, logs: directory, setup: try Self.wakeSetup())
        store.begin(OperationRecord(machineId: "pi", machineName: "Pi", kind: .action, subject: "wake-tower",
                                    summary: "Asking", agentTracked: false, wakeTarget: "tower"))
        rig.runners["pi"]?.answer("status", json: Self.helperStatus("command"))
        rig.runners["nas"]?.answer("status", json: Self.helperStatus("command"))
        await rig.pi.refresh(userInitiated: false)
        await rig.nas.refresh(userInitiated: false)
        #expect(rig.tower.wakeHoldOffer == .otherHelpers)

        var asked: PendingDialog?
        rig.tower.ask = { asked = $0 }
        rig.tower.requestTryOtherHelpers()
        let question = try #require(asked)
        #expect(question.title == "Try the other helpers for Tower?")
        #expect(question.message.contains("does not run that one again"))
        #expect(store.wakeHolds(target: "tower").count == 1, "asking releases nothing")

        rig.runners["pi"]?.answer("run", json: Self.ran)
        rig.runners["nas"]?.answer("run", json: Self.ran)
        question.perform()
        await Self.settle(rig.tower, timeout: 10)
        #expect(Self.runs(rig.runners["pi"]) == 0)
        #expect(Self.runs(rig.runners["nas"]) == 1)
        #expect(store.wakeHolds(target: "tower").isEmpty, "the next wake asks every helper")
        #expect(rig.tower.statusDetail?.contains("Pi: its wake-tower was run to wake Tower earlier and may still be going") == true,
                "\(rig.tower.statusDetail ?? "")")
    }

    @Test("with no other helper the hold stands, and only running the held command again after a warning goes past it")
    func runningTheHeldCommandAgain() async throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let onlyPi = Data(String(decoding: Self.wakeDocument, as: UTF8.self)
            .replacingOccurrences(of: #", { "machine": "nas", "action": "wake-tower" }"#, with: "").utf8)
        let rig = try Self.wakeRig(store: store, logs: directory, setup: try Self.wakeSetup(onlyPi))
        store.begin(OperationRecord(machineId: "pi", machineName: "Pi", kind: .action, subject: "wake-tower",
                                    summary: "Asking", agentTracked: false, wakeTarget: "tower"))
        rig.runners["pi"]?.answer("status", json: Self.helperStatus("command"))
        await rig.pi.refresh(userInitiated: false)
        #expect(rig.tower.wakeHoldOffer == .runAgain(label: "Run Pi's command again",
                                                     warning: WakePlan.runAgainWarning(["Pi"], target: "Tower")))

        // The other helpers, asked for anyway: there is nobody new, so nothing is released.
        rig.tower.tryOtherHelpers()
        await Self.settle(rig.tower, timeout: 10)
        #expect(rig.tower.statusLine == WakePlan.noOtherHelper("Tower"))
        #expect(store.wakeHolds(target: "tower").count == 1)
        #expect(Self.runs(rig.runners["pi"]) == 0)

        var asked: PendingDialog?
        rig.tower.ask = { asked = $0 }
        rig.tower.requestRunHeldAgain()
        let question = try #require(asked)
        #expect(question.title == "Run Pi's command again for Tower?")
        #expect(question.message.contains("can run it a second time beside the first"))
        rig.runners["pi"]?.answer("run", json: Self.ran)
        question.perform()
        await Self.settle(rig.tower, timeout: 10)
        #expect(Self.runs(rig.runners["pi"]) == 1)
        #expect(store.wakeHolds(target: "tower").isEmpty)
    }

    @Test("readiness polls start at the contract's times after a send, and a step that sent nothing waits for nothing")
    func readinessOnAFakeClock() async throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let here = SiteAwareness.placement(sites: [], addresses: [LocalAddress("192.0.2.7/24")!], confirmed: nil)
        var rig = try Self.wakeRig(store: store, logs: directory, setup: try Self.wakeSetup(), here: here)
        var sent: [WakeOnLAN.Packets] = []
        rig.tower.sendWakePackets = { sent.append($0); return nil }
        // Asleep for the first two polls, up for the third, and each of them really dialled: an
        // address that did not answer is otherwise left alone for a minute.
        let down = FakeSSH.result(exit: 255)
        rig.runners["tower"]?.answer("status", with: [down, down, FakeSSH.result(out: Self.towerStatus)])
        rig.tower.wake()
        await Self.settle(rig.tower, timeout: 10)
        #expect(sent == [WakeOnLAN.Packets(mac: "02:00:00:00:00:01", broadcasts: ["192.0.2.255"], ports: [9, 7])])
        #expect(rig.clock.waits.map { Int(($0 * 1000).rounded()) } == [5000, 8000, 12800])
        #expect(store.newestFirst.first { $0.kind == .wake }?.state == .succeeded)
        #expect(rig.runners["tower"]?.sentCommands.filter { $0.contains(" status") }.count == 3)
        #expect(Self.runs(rig.runners["pi"]) == 0)

        // No packet left: straight on to the helpers, with no wait in between.
        rig = try Self.wakeRig(store: store, logs: directory, setup: try Self.wakeSetup(), here: here)
        rig.runners["pi"]?.answer("status", json: Self.helperStatus("wol"))
        rig.runners["pi"]?.answer("run", json: #"{"ok":true,"contract":3,"action":"failed","reasonCode":"internal"}"#)
        rig.tower.wake()
        await Self.settle(rig.tower, timeout: 10)
        #expect(rig.clock.waits.isEmpty)
        #expect(Self.runs(rig.runners["pi"]) == 1)
    }

    @Test("a wake into a system that ends early is forgotten, so a later wake boots nothing")
    func wakeIntoIsForgottenOnEveryEarlyExit() async throws {
        // M13: an early return left the request standing, and the next wake that did bring the
        // machine up went on to reboot it into a system nobody had asked for that time.
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-wake-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let lonely = Data(String(decoding: Self.wakeDocument, as: UTF8.self)
            .replacingOccurrences(of: #""helpers": [ { "machine": "pi", "action": "wake-tower" }, { "machine": "nas", "action": "wake-tower" } ]"#,
                                  with: #""broadcast": ["192.0.2.255"]"#).utf8)
        let setup = try Self.wakeSetup(lonely)
        let other = try #require(setup.machine(id: "tower")?.system(id: "other"))

        // Off the network with no helper: nothing to try, said without starting anything.
        let away = try Self.wakeRig(store: store, logs: directory, setup: setup)
        var asked: [String] = []
        away.tower.ask = { asked.append($0.id) }
        away.tower.wake(into: other)
        #expect(!away.tower.isWorking)

        // Sent and never answered.
        let here = SiteAwareness.placement(sites: [], addresses: [LocalAddress("192.0.2.7/24")!], confirmed: nil)
        let silent = try Self.wakeRig(store: store, logs: directory, setup: setup, here: here)
        silent.tower.sendWakePackets = { _ in nil }
        silent.runners["tower"]?.answer("status", with: [FakeSSH.result(exit: 255)])
        silent.tower.ask = { asked.append($0.id) }
        silent.tower.wake(into: other)
        await Self.settle(silent.tower, timeout: 10)
        #expect(silent.tower.statusLine.contains("did not come up"))

        // Each model now wakes again and the tower answers as Linux: neither boots it into the
        // other system.
        for rig in [away, silent] {
            rig.tower.placement = { here }
            rig.tower.freshPlacement = { here }
            rig.tower.sendWakePackets = { _ in nil }
            rig.runners["tower"]?.answer("status", json: Self.towerStatus)
            rig.tower.wake()
            await Self.settle(rig.tower, timeout: 10)
            #expect(rig.tower.isAwake)
        }
        #expect(!asked.contains { $0.hasPrefix("boot-") })

        // And one that is asked for and answers does go on.
        silent.runners["tower"]?.answer("status", with: [FakeSSH.result(exit: 255), FakeSSH.result(out: Self.towerStatus)])
        silent.tower.wake(into: other)
        await Self.settle(silent.tower, timeout: 10)
        #expect(asked.contains("boot-tower-other"))
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
            try ControllerConfig.accepted(document), document: document,
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

    /// Every address one launch dialled, in order.
    final class Dialled: @unchecked Sendable {
        let lock = NSLock()
        var destinations: [String] = []
        func note(_ destination: String?) { lock.withLock { destinations.append(destination ?? "") } }
        var first: String? { lock.withLock { destinations.first } }
    }

    @Test("the route that worked is dialled first at the next launch, and not once the machine's endpoints change")
    func rememberedRouteOutlivesALaunch() async throws {
        // Kept in memory alone, every launch dialled the dead addresses first again, a connect
        // timeout each: eight to sixteen seconds a machine before the one that answers.
        var machine = Self.machine()
        machine.endpoints = [Endpoint(id: "old", kind: "remote", host: "192.0.2.10", user: "me"),
                             Endpoint(id: "new", kind: "remote", host: "192.0.2.20", user: "me")]
        var saved: [String: RememberedSystem] = [:]
        let remembered = RememberedSettings(systems: [:], save: { saved = $0 })
        let logs = FakeSSH.logDirectory()
        defer { try? FileManager.default.removeItem(at: logs) }

        let status = Self.onlineStatus
        func launch(_ machine: Machine) async -> (MachineModel, Dialled) {
            let dialled = Dialled()
            var agent = RemoteAgent(machine: machine)
            agent.logDirectory = logs
            agent.runner = { _, arguments, _, _ in
                let destination = FakeSSH.destination(arguments)
                dialled.note(destination)
                guard destination == "me@192.0.2.20" else {
                    FakeSSH.writeLog("ssh: connect to host 192.0.2.10 port 22: Operation timed out\n", for: arguments)
                    return FakeSSH.result(exit: 255)
                }
                return FakeSSH.result(out: status)
            }
            let model = MachineModel(machine: machine, agent: agent, remembered: remembered)
            await model.refresh(userInitiated: false)
            return (model, dialled)
        }

        let (first, dialledFirst) = await launch(machine)
        #expect(first.isAwake)
        #expect(dialledFirst.first == "me@192.0.2.10")
        #expect(saved["pi/linux"]?.route == "new")

        // The next launch knows nothing yet about which system is up, and starts where the last
        // one finished.
        let (second, dialledSecond) = await launch(machine)
        #expect(second.isAwake)
        #expect(dialledSecond.destinations == ["me@192.0.2.20"])

        // A setup that reaches the machine some other way now walks its list as it stands.
        machine.endpoints.append(Endpoint(id: "spare", kind: "remote", host: "192.0.2.30", user: "me"))
        let (_, dialledThird) = await launch(machine)
        #expect(dialledThird.first == "me@192.0.2.10")

        // What an earlier build remembered, with no route at all, still reads.
        let older = try JSONDecoder().decode([String: RememberedSystem].self, from: Data(#"{"pi/linux":{"autoUpdate":true}}"#.utf8))
        #expect(older["pi/linux"]?.autoUpdate == true)
        #expect(older["pi/linux"]?.route == nil)
    }

    @Test("a LAN route remembered on the machine's network is not dialled first from anywhere else")
    func rememberedRouteKeepsToItsNetwork() async throws {
        // A laptop last opened at home and opened again somewhere else dialled the home address
        // first: a whole connect timeout a machine, on a range that may be somebody else's.
        var machine = Self.machine()
        machine.endpoints = [Endpoint(id: "lan", kind: "lan", host: "192.168.1.10", user: "me"),
                             Endpoint(id: "tailnet", kind: "remote", host: "192.0.2.20", user: "me")]
        machine.wake = WakeConfig(mac: nil, lanPrefix: "192.168.1.")
        let home = SiteAwareness.placement(sites: [], addresses: [LocalAddress("192.168.1.7/24")!], confirmed: nil)
        let remembered = RememberedSettings.inMemory()
        let logs = FakeSSH.logDirectory()
        defer { try? FileManager.default.removeItem(at: logs) }

        let status = Self.onlineStatus
        func launch(at placement: SiteAwareness.Placement) async -> [String] {
            let dialled = Dialled()
            let atHome = placement == home
            var agent = RemoteAgent(machine: machine)
            agent.logDirectory = logs
            agent.runner = { _, arguments, _, _ in
                let destination = FakeSSH.destination(arguments)
                dialled.note(destination)
                // Away from home the LAN address reaches nothing.
                guard atHome || destination != "me@192.168.1.10" else {
                    FakeSSH.writeLog("ssh: connect to host 192.168.1.10 port 22: Operation timed out\n", for: arguments)
                    return FakeSSH.result(exit: 255)
                }
                return FakeSSH.result(out: status)
            }
            let model = MachineModel(machine: machine, agent: agent, remembered: remembered)
            model.placement = { placement }
            await model.refresh(userInitiated: false)
            #expect(model.isAwake)
            return dialled.lock.withLock { dialled.destinations }
        }

        #expect(await launch(at: home) == ["me@192.168.1.10"])
        #expect(await launch(at: .nowhere) == ["me@192.0.2.20"])
        // And the tailnet route that answered away from home does not stand in front of the LAN
        // once the laptop is back.
        #expect(await launch(at: home) == ["me@192.168.1.10"])
        #expect(await launch(at: home) == ["me@192.168.1.10"])
    }

    @Test("a remembered route belongs to one system, and at launch to whichever answered last")
    func rememberedRouteIsPerSystem() {
        let now = Date()
        let remembered = RememberedSettings(systems: [
            "pi/linux": RememberedSystem(checkedAt: now.addingTimeInterval(-60), route: "lan", routes: "these"),
            "pi/other": RememberedSystem(checkedAt: now, route: "tailnet", routes: "these"),
            "pi-2/linux": RememberedSystem(checkedAt: now.addingTimeInterval(60), route: "elsewhere", routes: "these"),
        ], save: { _ in })
        // After a reboot the system it is heading for has a route of its own.
        #expect(remembered.route(machine: "pi", system: "linux", routes: "these") == "lan")
        #expect(remembered.route(machine: "pi", system: nil, routes: "these") == "tailnet")
        #expect(remembered.route(machine: "pi", system: "linux", routes: "others") == nil)
        #expect(remembered.route(machine: "pi", system: "windows", routes: "these") == nil)
    }

    @Test("a refused automatic-update switch is not remembered as saved")
    func refusedAutoUpdateIsNotSaved() async throws {
        var saved: [String: RememberedSystem] = [:]
        let remembered = RememberedSettings(systems: [:], save: { saved = $0 })
        let runner = FakeRunner()
        runner.answer("status", json: Self.onlineStatus)
        runner.answer("auto-update", json: #"{"ok":false,"contract":3,"reasonCode":"config-invalid","message":"the agent config is broken"}"#, exit: 1)
        let logs = FakeSSH.logDirectory()
        defer { try? FileManager.default.removeItem(at: logs) }
        var agent = RemoteAgent(machine: Self.machine())
        agent.logDirectory = logs
        agent.runner = { _, arguments, _, _ in runner.run(arguments) }
        let model = MachineModel(machine: Self.machine(), agent: agent, remembered: remembered)

        await model.refresh(userInitiated: false)
        model.setAutoUpdate(true, on: Self.machine().systems[0])
        await Self.settle(model)

        #expect(saved["pi/linux"]?.autoUpdate != true)
        #expect(model.lastActionOutcome?.isError == true)
        #expect(model.statusLine.contains("broken") || model.statusLine.contains("config"))
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

    // MARK: - Sharing the setup

    /// An agent's replies to the setup requests, told apart by subcommand, and the means to hold one
    /// open while the test does something else.
    final class SetupRunner: @unchecked Sendable {
        private let lock = NSLock()
        private var replies: [String: [String]] = [:]
        private var holding: Set<String> = []
        private var arrived: Set<String> = []
        private(set) var requests: [String] = []
        private(set) var pushed: [Data] = []

        static func key(_ arguments: [String]) -> String {
            let words = (arguments.last ?? "").split(separator: " ").dropFirst(2).map(String.init)
            guard let verb = words.first else { return "" }
            if verb == "config", words.count > 1, ["meta", "set"].contains(words[1]) { return "config \(words[1])" }
            return verb
        }

        func answer(_ key: String, _ json: String...) {
            lock.withLock { replies[key, default: []].append(contentsOf: json) }
        }

        func hold(_ key: String) { lock.withLock { _ = holding.insert(key) } }

        func release(_ key: String) { lock.withLock { _ = holding.remove(key) } }

        func isHolding(_ key: String) -> Bool { lock.withLock { arrived.contains(key) && holding.contains(key) } }

        var sent: [String] { lock.withLock { requests } }

        func run(_ arguments: [String], input: Data?) async -> CommandResult {
            let key = Self.key(arguments)
            lock.withLock {
                requests.append(key)
                arrived.insert(key)
                if key == "config set", let input { pushed.append(input) }
            }
            while lock.withLock({ holding.contains(key) }) { try? await Task.sleep(for: .milliseconds(5)) }
            let reply: String? = lock.withLock {
                guard var queue = replies[key], !queue.isEmpty else { return nil }
                let next = queue.removeFirst()
                replies[key] = queue
                return next
            }
            guard let reply else {
                FakeSSH.writeLog("ssh: connect to host 10.0.0.5 port 22: Operation timed out\n", for: arguments)
                return CommandResult(exitCode: 255, standardOutput: "", standardError: "", timedOut: false, launchFailure: nil)
            }
            return CommandResult(exitCode: 0, standardOutput: reply, standardError: "", timedOut: false, launchFailure: nil)
        }
    }

    static let setupId = "setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846"

    /// The applied setup, in a store of its own, at revision 1 with no ancestors.
    static func setupStore() throws -> (ConfigStore, URL) {
        let home = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-setup-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        let file = home.appending(path: "config.json")
        try Data("""
        { "version": 1,
          "controller": { "id": "\(setupId)", "name": "Home", "revision": 1, "lineage": [] },
          "machines": [
            { "id": "pi", "name": "Pi", "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5", "user": "me" } ],
              "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
            { "id": "nas", "name": "NAS", "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.6", "user": "me" } ],
              "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
        """.utf8).write(to: file)
        return (ConfigStore(url: file), home)
    }

    /// A revision of the store's document as another device would write it, without applying it.
    @MainActor
    static func revision(of store: ConfigStore, renamingPiTo name: String) throws -> Data {
        let raw = try #require(store.rawBytes)
        return try ControllerEditor.apply(to: raw, parentHash: try Canonical.hash(raw),
                                          parentIdentity: try #require(store.config).identity, deviceName: "Pixel") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": name], in: &root)
        }.bytes
    }

    static func setupStatus(holding hash: String, id: String = setupId) -> String {
        """
        { "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux", "name": "Linux" },
          "agent": { "version": "3.1.0", "contract": 3 }, "services": [],
          "controller": { "hash": "\(hash)", "id": "\(id)" } }
        """
    }

    static func setupMeta(_ document: Data) throws -> String {
        let hash = try Canonical.hash(document)
        let identity = ControllerIdentity(block: try SetupJSON.parse(document)["controller"])
        let lineage = identity.ancestors.map { "\"\($0)\"" }.joined(separator: ",")
        return #"{ "ok": true, "contract": 3, "hash": "\#(hash)", "meta": { "hash": "\#(hash)", "id": "\#(setupId)", "revision": \#(identity.revisionNumber), "lineage": [\#(lineage)] } }"#
    }

    static func setupRead(_ document: Data) throws -> String {
        let reply: [String: Any] = ["ok": true, "contract": 3, "raw": String(decoding: document, as: UTF8.self),
                                    "hash": try Canonical.hash(document)]
        return String(decoding: try JSONSerialization.data(withJSONObject: reply), as: UTF8.self)
    }

    /// A machine model wired to the store the way the app wires it. The entry is built in code, so
    /// its system is one this device runs; read from the document it would wait for approval.
    static func setupModel(_ id: String, runner: SetupRunner, store: ConfigStore, logs: URL) throws -> MachineModel {
        let entry = Machine(id: id, name: id, endpoints: [Endpoint(id: "lan", kind: "lan", host: "10.0.0.5", user: "me")],
                            systems: [SystemConfig(id: "linux", name: "Linux", platform: .linux, agent: ["node", "/a.mjs"])])
        var agent = RemoteAgent(machine: entry)
        agent.logDirectory = logs.appending(path: "\(id)-logs")
        agent.runner = { _, arguments, _, input in await runner.run(arguments, input: input) }
        let model = MachineModel(machine: entry, agent: agent)
        model.controllerConfig = { store.document }
        model.adoptSetup = { bytes, description, head in store.adopt(bytes, describedAs: description, replacing: head) }
        return model
    }

    static func eventually(_ condition: @MainActor () -> Bool) async {
        let deadline = Date().addingTimeInterval(15)
        while !condition(), Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
    }

    @Test("an edit saved while a fetch was being read stays applied, and the next status finds the branch")
    func adoptionKeepsAnEditSavedDuringTheRead() async throws {
        let (store, home) = try Self.setupStore()
        defer { try? FileManager.default.removeItem(at: home) }
        let original = try #require(store.document)
        let theirs = try Self.revision(of: store, renamingPiTo: "Kitchen Pi")
        let runner = SetupRunner()
        runner.answer("status", Self.setupStatus(holding: try Canonical.hash(theirs)), Self.setupStatus(holding: try Canonical.hash(theirs)))
        runner.answer("config meta", try Self.setupMeta(theirs))
        runner.answer("config", try Self.setupRead(theirs))
        runner.hold("config")
        let model = try Self.setupModel("pi", runner: runner, store: store, logs: home)

        await model.refresh(userInitiated: false)
        await Self.eventually { runner.isHolding("config") }
        #expect(runner.isHolding("config"))

        // A person saves an edit while the read is out.
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "nas", "name": "Basement NAS"], in: &root)
        } == nil)
        let edit = try #require(store.document)
        runner.release("config")
        await Self.eventually { !model.core.setup.isSharing }

        // M16: the bytes chosen against the document the edit replaced are not written over it.
        #expect(store.document?.hash == edit.hash)
        #expect(store.config?.machine(id: "nas")?.name == "Basement NAS")
        #expect(edit.hash != original.hash)

        // The next status decides from the new head, and both sides now have a change of their own.
        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing }
        #expect(model.setupDecision.decision == .diverged)
        #expect(!runner.sent.contains("config set"))
    }

    @Test("a setup file saved half-way while a fetch was being read is not adopted over, and the line says why")
    func adoptionKeepsAFileSavedBrokenDuringTheRead() async throws {
        let (store, home) = try Self.setupStore()
        defer { try? FileManager.default.removeItem(at: home) }
        let theirs = try Self.revision(of: store, renamingPiTo: "Kitchen Pi")
        let runner = SetupRunner()
        runner.answer("status", Self.setupStatus(holding: try Canonical.hash(theirs)), Self.setupStatus(holding: try Canonical.hash(theirs)))
        runner.answer("config meta", try Self.setupMeta(theirs))
        runner.answer("config", try Self.setupRead(theirs), try Self.setupRead(theirs))
        runner.hold("config")
        let model = try Self.setupModel("pi", runner: runner, store: store, logs: home)

        await model.refresh(userInitiated: false)
        await Self.eventually { runner.isHolding("config") }
        #expect(runner.isHolding("config"))

        // The person's editor saves the file half-way while the read is out. The app goes on running
        // on the document before it, which is the head the fetch was decided against.
        let unfinished = Data(try #require(store.rawBytes).dropLast(3)) + Data(",\n".utf8)
        try unfinished.write(to: store.url)
        runner.release("config")
        await Self.eventually { !model.core.setup.isSharing }

        // M16: nothing is written over it, and the line says why the newer setup was not taken.
        #expect(try Data(contentsOf: store.url) == unfinished)
        #expect(store.config?.machine(id: "pi")?.name == "Pi")
        #expect(model.statusIsError)
        #expect(model.statusLine.contains("not a setup this device can share yet"))

        // A file that is not a document gives nothing to decide from, so the read is not sent again.
        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing }
        #expect(model.setupDecision.decision == .unknown)
        #expect(runner.sent == ["status", "config meta", "config", "status"])
        #expect(try Data(contentsOf: store.url) == unfinished)
    }

    @Test("a kept base that peer intake refuses is skipped like one that is not kept, so every difference is asked about")
    func divergenceSheetSkipsARefusedBase() async throws {
        let home = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-setup-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: home) }
        // The document both sides were made from wakes the Pi through a machine that does not exist,
        // which peer intake refuses: a base an older client could push through a 3.0 agent. Each side
        // has fixed it since, and the other one renamed the Pi as well.
        let file = home.appending(path: "config.json")
        try Data("""
        { "version": 1,
          "controller": { "id": "\(Self.setupId)", "name": "Home", "revision": 1, "lineage": [] },
          "machines": [
            { "id": "pi", "name": "Pi", "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5", "user": "me" } ],
              "wake": { "helper": { "machine": "ghost", "action": "wake-pi" },
                        "helpers": [ { "machine": "ghost", "action": "wake-pi" } ] },
              "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
            { "id": "nas", "name": "NAS", "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.6", "user": "me" } ],
              "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
        """.utf8).write(to: file)
        let store = ConfigStore(url: file)
        let base = try #require(store.document)
        #expect(DocumentAcceptance.check(base.bytes, intake: .peer).errors.first?.code == "helper-unknown")
        let fix = { (root: inout [String: Any]) throws in
            try ControllerEditor.setWakeHelpers([WakeHelper(machine: "nas", action: "wake-pi")], onMachine: "pi", in: &root)
        }
        let theirs = try ControllerEditor.apply(to: base.bytes, parentHash: base.hash, parentIdentity: base.identity,
                                                deviceName: "Pixel") { root in
            try fix(&root)
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Kitchen Pi"], in: &root)
        }.bytes
        #expect(store.applyEdit(describedAs: "fixed the helper", deviceName: "MacBook", change: fix) == nil)

        let runner = SetupRunner()
        runner.answer("status", Self.setupStatus(holding: try Canonical.hash(theirs)))
        runner.answer("config meta", try Self.setupMeta(theirs))
        runner.answer("config", try Self.setupRead(theirs))
        let model = try Self.setupModel("pi", runner: runner, store: store, logs: home)
        model.setupBase = { mineHash, mineLineage, theirsHash, theirsLineage in
            store.commonAncestor(mineHash: mineHash, mineLineage: mineLineage, theirsHash: theirsHash, theirsLineage: theirsLineage)
        }
        var raised: SetupDivergence?
        model.onSetupDiverged = { raised = $0 }

        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing && runner.sent.contains("config meta") }
        #expect(model.setupDecision.decision == .diverged)
        model.openSetupDivergence()
        await Self.eventually { raised != nil }

        // The only revision both sides descend from is one peer intake refuses (section 9.8), so the
        // merge has no base: nothing waits on it, and every entry the two sides differ in is asked
        // about rather than decided from a document this build would not accept.
        let divergence = try #require(raised)
        #expect(divergence.baseBytes == nil)
        #expect(divergence.mergeBlocker == nil)
        #expect(!divergence.differences.isEmpty)
        #expect(divergence.differences.allSatisfy { $0.kind == .noBase })
        #expect(divergence.ancestorNote == "not kept on this device, so every difference needs a decision")
    }

    @Test("a different setup is compared, never merged, and the sheet says so when this device's setup has an error")
    func differentSetupSheetSaysWhatCannotBeCompared() async throws {
        let home = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-setup-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: home) }
        // Two sites with one id: applied intake keeps the document with that error, and it is not one
        // a comparison can be made over.
        let file = home.appending(path: "config.json")
        try Data("""
        { "version": 1,
          "controller": { "id": "\(Self.setupId)", "name": "Home", "revision": 1, "lineage": [] },
          "sites": [ { "id": "home", "name": "Home" }, { "id": "home", "name": "Cabin" } ],
          "machines": [
            { "id": "pi", "name": "Pi", "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5", "user": "me" } ],
              "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
        """.utf8).write(to: file)
        let store = ConfigStore(url: file)
        let local = try #require(store.document)
        #expect(SetupMerge.blocker(mine: local.bytes) != nil)

        let otherId = "setup-5b1e7d02-6c3a-4f19-8e47-2a9d0c6b1f38"
        let theirs = Data("""
        { "version": 1,
          "controller": { "id": "\(otherId)", "name": "Office", "revision": 4, "lineage": [] },
          "machines": [
            { "id": "pi", "name": "Office Pi", "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5", "user": "me" } ],
              "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
        """.utf8)
        #expect(DocumentAcceptance.check(theirs, intake: .peer).valid)
        let runner = SetupRunner()
        runner.answer("status", Self.setupStatus(holding: try Canonical.hash(theirs), id: otherId))
        runner.answer("config", try Self.setupRead(theirs))
        let model = try Self.setupModel("pi", runner: runner, store: store, logs: home)
        var raised: SetupDivergence?
        model.onSetupDiverged = { raised = $0 }

        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing }
        #expect(model.setupDecision.decision == .differentSetup)
        model.openSetupDivergence()
        await Self.eventually { raised != nil }

        let divergence = try #require(raised)
        #expect(divergence.isDifferentSetup)
        #expect(divergence.differences.isEmpty)
        let blocker = try #require(divergence.mergeBlocker)
        #expect(blocker.contains("before the two can be compared"))
        #expect(!blocker.contains("merged"))
        #expect(divergence.ancestorNote == "not kept on this device")
    }

    @Test("two reads out at once cannot both adopt: the second finds the head the first one wrote")
    func twoReadsCannotOverwriteANewHead() async throws {
        let (store, home) = try Self.setupStore()
        defer { try? FileManager.default.removeItem(at: home) }
        // Two machines hold two different children of the applied document.
        let first = try Self.revision(of: store, renamingPiTo: "Kitchen Pi")
        let second = try Self.revision(of: store, renamingPiTo: "Hall Pi")
        let runners = [SetupRunner(), SetupRunner()]
        for (runner, document) in zip(runners, [first, second]) {
            runner.answer("status", Self.setupStatus(holding: try Canonical.hash(document)))
            runner.answer("config meta", try Self.setupMeta(document))
            runner.answer("config", try Self.setupRead(document))
            runner.hold("config")
        }
        let pi = try Self.setupModel("pi", runner: runners[0], store: store, logs: home)
        let nas = try Self.setupModel("nas", runner: runners[1], store: store, logs: home)

        await pi.refresh(userInitiated: false)
        await nas.refresh(userInitiated: false)
        await Self.eventually { runners.allSatisfy { $0.isHolding("config") } }

        runners[0].release("config")
        await Self.eventually { !pi.core.setup.isSharing }
        #expect(store.document?.hash == (try Canonical.hash(first)))

        runners[1].release("config")
        await Self.eventually { !nas.core.setup.isSharing }
        // The second read was decided against the document the first one replaced, so it writes
        // nothing, and deciding again finds the two branches.
        #expect(store.document?.hash == (try Canonical.hash(first)))
        #expect(nas.setupDecision.decision == .diverged)
    }

    @Test("a machine that moves between the meta read and the fetch is decided again, not adopted")
    func remoteMovingBetweenMetaAndFetchReevaluates() async throws {
        let (store, home) = try Self.setupStore()
        defer { try? FileManager.default.removeItem(at: home) }
        let original = try #require(store.document)
        let theirs = try Self.revision(of: store, renamingPiTo: "Kitchen Pi")
        // Written on top of `theirs` by yet another device, after the meta read.
        let scratch = try ConfigStore.scratch(theirs, in: home)
        let later = try Self.revision(of: scratch, renamingPiTo: "Hall Pi")
        let runner = SetupRunner()
        runner.answer("status", Self.setupStatus(holding: try Canonical.hash(theirs)), Self.setupStatus(holding: try Canonical.hash(later)))
        runner.answer("config meta", try Self.setupMeta(theirs), try Self.setupMeta(later))
        runner.answer("config", try Self.setupRead(later), try Self.setupRead(later))
        let model = try Self.setupModel("pi", runner: runner, store: store, logs: home)

        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing && runner.sent.contains("config") }
        #expect(store.document?.hash == original.hash)

        await model.refresh(userInitiated: false)
        await Self.eventually { store.document?.hash == (try? Canonical.hash(later)) }
        #expect(store.document?.hash == (try Canonical.hash(later)))
        #expect(runner.sent == ["status", "config meta", "config", "status", "config meta", "config"])
    }

    @Test("a push refused as restricted is not sent again until the setup changes")
    func restrictedRefusalHoldsUntilHashChanges() async throws {
        let (store, home) = try Self.setupStore()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try #require(store.document)
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Attic Pi"], in: &root)
        } == nil)
        let runner = SetupRunner()
        runner.answer("status", Self.setupStatus(holding: held.hash), Self.setupStatus(holding: held.hash),
                      Self.setupStatus(holding: held.hash))
        runner.answer("config set", #"{ "ok": false, "contract": 3, "reasonCode": "restricted", "current": { "hash": "\#(held.hash)", "id": "\#(Self.setupId)" } }"#)
        let model = try Self.setupModel("pi", runner: runner, store: store, logs: home)

        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing && runner.sent.contains("config set") }
        guard case .held = model.setupSharing else {
            Issue.record("the refusal is not held: \(model.setupSharing)")
            return
        }
        await model.refresh(userInitiated: false)
        await Self.eventually { !model.core.setup.isSharing }
        #expect(runner.sent.filter { $0 == "config set" }.count == 1)

        // A new revision is a new document, and the machine is offered it.
        #expect(store.applyEdit(describedAs: "renamed again", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Loft Pi"], in: &root)
        } == nil)
        let newer = try #require(store.document)
        runner.answer("config set", #"{ "ok": true, "contract": 3, "action": "stored", "hash": "\#(newer.hash)" }"#)
        await model.refresh(userInitiated: false)
        await Self.eventually { runner.sent.filter { $0 == "config set" }.count == 2 && !model.core.setup.isSharing }
        #expect(runner.pushed.last == newer.bytes)
        #expect(model.core.setup.lastShare?.failure == nil)
    }
}

extension ConfigStore {
    /// A store of its own holding one document, for building revisions on top of it.
    @MainActor
    static func scratch(_ document: Data, in home: URL) throws -> ConfigStore {
        let directory = home.appending(path: "scratch-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let file = directory.appending(path: "config.json")
        try document.write(to: file)
        return ConfigStore(url: file)
    }
}
