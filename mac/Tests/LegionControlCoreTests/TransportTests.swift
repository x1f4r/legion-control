import Foundation
import Testing
@testable import LegionControlCore

/// The rules that decide whether a command may be tried again somewhere else.
///
/// These are the ones with teeth. Getting the classification wrong means either a reboot that
/// happens twice or a machine reported asleep when its key was rejected, and neither is visible from
/// a passing build.
struct TransportTests {

    // MARK: - Fixtures

    static func machine(
        id: String = "workstation",
        endpoints: [(String, String, String?)] = [("remote", "100.64.0.10", nil), ("lan", "10.0.0.40", nil)],
        systems: [(String, Platform)] = [("linux", .linux), ("windows", .windows)]
    ) -> Machine {
        Machine(
            id: id,
            name: id,
            endpoints: endpoints.map { kind, host, system in
                Endpoint(id: "\(kind)-\(host)", kind: kind, host: host, port: nil, user: "me",
                         system: system, label: "\(kind) \(host)")
            },
            systems: systems.map { SystemConfig(id: $0.0, platform: $0.1, agent: ["node", "/agent.mjs"]) }
        )
    }

    static func result(exit: Int32? = 0, out: String = "", err: String = "",
                       timedOut: Bool = false, launchFailure: String? = nil) -> CommandResult {
        FakeSSH.result(exit: exit, out: out, err: err, timedOut: timedOut, launchFailure: launchFailure)
    }

    static func attempt(_ machine: Machine) -> RemoteAgent.Attempt {
        RemoteAgent.Attempt(route: machine.routes[0], system: machine.systems[0])
    }

    /// A transport whose ssh is `answer`, writing its logs somewhere of their own.
    static func agent(_ machine: Machine,
                      answer: @escaping @Sendable (_ arguments: [String]) async -> CommandResult) -> (RemoteAgent, URL) {
        let logs = FakeSSH.logDirectory()
        var agent = RemoteAgent(machine: machine)
        agent.logDirectory = logs
        agent.runner = { _, arguments, _, _ in await answer(arguments) }
        return (agent, logs)
    }

    /// What one attempt's evidence comes to, the way the transport turns it into a failure.
    static func failure(_ result: CommandResult, log: String = "") -> AgentFailure {
        let machine = Self.machine()
        let verdict = AttemptClassifier.classify(AttemptEvidence(result, sshLog: log), timeout: 25)
        return RemoteAgent.failure(verdict, attempt: Self.attempt(machine),
                                   command: RemoteCommand(system: machine.systems[0], arguments: ["status"]),
                                   result: result, sshLog: log)
    }

    static func restart() throws -> AgentRequest {
        try AgentRequest.restart(service: "demo", intent: .init(operationId: AgentToken.newOperationId()), dialect: .v3)
    }

    // MARK: - Classification

    @Test("a timeout is never a failure and never a success")
    func timeoutIsUnknown() {
        let failure = Self.failure(Self.result(exit: nil, timedOut: true))
        #expect(failure.kind == .timedOut(seconds: 25))
        // The rule the review's finding 14 is about: the old transport mapped this to "unreachable",
        // and the boot handler then drew a reboot that had never happened.
        #expect(failure.dispatch == .unknown)
        #expect(!failure.meansAsleepOrOff)
    }

    @Test("ssh's own log is told apart rather than folded into unreachable")
    func sshDiagnoses() {
        let cases: [(String, AgentFailure.Kind, Dispatch)] = [
            ("ssh: connect to host x port 22: Connection refused", .connectionRefused, .never),
            ("ssh: connect to host x port 22: No route to host", .hostUnreachable, .never),
            ("me@x: Permission denied (publickey).", .authenticationFailed, .never),
            ("@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @", .hostKeyChanged, .never),
            ("No ED25519 host key is known for x and you have requested strict checking.", .hostKeyUnknown, .never),
            ("client_loop: send disconnect: Broken pipe", .linkLost, .unknown)
        ]
        for (text, kind, dispatch) in cases {
            let failure = Self.failure(Self.result(exit: 255), log: text + "\n")
            #expect(failure.kind == kind, "\(text)")
            #expect(failure.dispatch == dispatch, "\(text)")
            #expect(failure.detail.contains(text), "ssh's words are still shown: \(text)")
        }
    }

    @Test("an ssh-looking line on remote stderr proves nothing")
    func remoteStderrIsNotEvidence() {
        // The agent writes raw text to stderr and the launcher passes it through, so a program on the
        // far side can print any of these after doing its work. Only ssh's own log may say "never".
        for text in ["ssh: connect to host x port 22: Connection refused", "me@x: Permission denied (publickey)."] {
            let failure = Self.failure(Self.result(exit: 255, err: text))
            #expect(failure.kind == .linkLost, "\(text)")
            #expect(failure.dispatch == .unknown, "\(text)")
        }
    }

    @Test("a rejected key does not read as a sleeping machine")
    func authIsNotSleep() {
        let failure = Self.failure(Self.result(exit: 255), log: "me@x: Permission denied (publickey).\n")
        #expect(!failure.meansAsleepOrOff)
        #expect(failure.message(machine: "Tower").contains("rejected the key"))
    }

    @Test("a shell that could not start the interpreter proves nothing ran; the rest does not")
    func remoteFailureDispatch() {
        let missing = Self.failure(Self.result(exit: 127, err: "bash: node: command not found"))
        #expect(missing.kind == .interpreterMissing)
        #expect(missing.dispatch == .never)

        // Node ran and printed this, possibly after doing half of its work.
        let noAgent = Self.failure(Self.result(exit: 1, err: "Error: Cannot find module '/agent.mjs'"))
        #expect(noAgent.kind == .agentMissing)
        #expect(noAgent.dispatch == .unknown)

        let garbage = Self.failure(Self.result(exit: 3, out: "something happened"))
        #expect(garbage.kind == .unreadableOutput)
        // It got as far as a shell on the far side, so we cannot claim it did nothing.
        #expect(garbage.dispatch == .unknown)
    }

    @Test("a PowerShell login refusing a cmd line is named, with the one-word fix")
    func powerShellLoginHint() {
        let windows = SystemConfig(id: "windows", platform: .windows, agent: ["node", "C:\\a.mjs"])
        let machine = Self.machine(systems: [("windows", .windows)])
        let attempt = RemoteAgent.Attempt(route: machine.routes[0], system: windows)
        let result = Self.result(exit: 1, err: "No characters are allowed after a here-string header but before the end of the line.\n    + CategoryInfo          : ParserError: (:) [], ParentContainsErrorRecordException")
        let verdict = AttemptClassifier.classify(AttemptEvidence(result, sshLog: ""))
        let failure = RemoteAgent.failure(verdict, attempt: attempt, command: RemoteCommand(system: windows, arguments: ["status"]),
                                          result: result, sshLog: "")
        #expect(failure.message(machine: "Tower") == "This account logs in to PowerShell; set shell to powershell.")

        func hint(shell: String?, restrictedSession: Bool = false, stderr: String = result.standardError) -> String? {
            let system = SystemConfig(id: "windows", platform: .windows, agent: ["node", "C:\\a.mjs"], shell: shell)
            let failed = Self.result(exit: 1, err: stderr)
            return RemoteAgent.failure(AttemptClassifier.classify(AttemptEvidence(failed, sshLog: "")),
                                       attempt: .init(route: machine.routes[0], system: system),
                                       command: RemoteCommand(system: system, arguments: ["status"], restrictedSession: restrictedSession),
                                       result: failed, sshLog: "").hint
        }
        // A setup that says cmd out loud gets the same hint. A 1.3 Mac wrote these lines for
        // PowerShell, and an edit since may have written cmd into a system on a PowerShell login.
        #expect(hint(shell: "cmd") == "This account logs in to PowerShell; set shell to powershell.")
        // A line that went out as PowerShell or through the dispatcher was not refused for being cmd.
        #expect(hint(shell: "powershell") == nil)
        #expect(hint(shell: nil, restrictedSession: true) == nil)
        // Nor is every failure of a cmd line a PowerShell login.
        #expect(hint(shell: "cmd", stderr: "'node' is not recognized as an internal or external command") == nil)
    }

    // MARK: - Retrying elsewhere

    @Test("a read is tried on every route; a mutation stops at the first uncertain failure")
    func mutationIsNotReplayed() async throws {
        let calls = Counter()
        // A timeout: the far side may have done the work.
        let (agent, logs) = Self.agent(Self.machine()) { _ in
            await calls.increment()
            return Self.result(exit: nil, timedOut: true)
        }
        defer { try? FileManager.default.removeItem(at: logs) }

        // A read may be retried freely: nothing on the far side changed.
        _ = try? await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        #expect(await calls.value >= 1)

        await calls.reset()
        agent.backoff.clear(Self.machine().routes[0].target)
        agent.backoff.clear(Self.machine().routes[1].target)
        var failure: AgentFailure?
        do {
            _ = try await agent.send(
                try AgentRequest.boot(target: "windows", intent: .init(operationId: AgentToken.newOperationId()), dialect: .v3),
                decoding: AgentActionResult.self,
                preferring: nil
            )
        } catch let error as AgentFailure {
            failure = error
        }
        // Exactly one. A reboot whose outcome is unknown must never be sent to a second address.
        #expect(await calls.value == 1)
        #expect(failure?.dispatch == .unknown)
    }

    @Test("an unexplained exit 255 stops a mutation after one attempt, whatever the agent speaks",
          arguments: [AgentDialect.legacy, AgentDialect.v3])
    func unexplainedExit255IsNotRetried(dialect: AgentDialect) async throws {
        // M15. The Mac read a bare 255 as "never sent" and walked on to the next route, so a restart
        // whose link dropped after it had started could run a second time. Two routes and two systems
        // are four places it could have gone; it goes to one.
        for log in ["", "Connection to 10.0.0.40 closed by remote host.\n", "something nobody has seen before\n"] {
            let calls = Counter()
            let (agent, logs) = Self.agent(Self.machine()) { arguments in
                await calls.increment()
                FakeSSH.writeLog(log, for: arguments)
                return Self.result(exit: 255)
            }
            defer { try? FileManager.default.removeItem(at: logs) }
            var failure: AgentFailure?
            do {
                _ = try await agent.send(
                    try AgentRequest.restart(service: "demo", intent: .init(operationId: AgentToken.newOperationId()), dialect: dialect),
                    decoding: AgentActionResult.self, preferring: nil)
            } catch let error as AgentFailure {
                failure = error
            }
            #expect(await calls.value == 1, "log: \(log)")
            #expect(failure?.kind == .linkLost)
            #expect(failure?.dispatch == .unknown)
        }
    }

    @Test("a cancelled request ends the walk and holds no route back", arguments: [CommandSafety.readOnly, .mutation])
    func cancellationIsNotARouteFailure(safety: CommandSafety) async throws {
        // Cancelling a task stops its ssh, which reads as a timeout, and a timeout is about the
        // route. Recorded, it held every address of the machine back for a minute, and a read
        // went on to the next route only to find the task cancelled and record a launch failure
        // there too. Closing the Service setup sheet while it loaded did exactly that.
        let machine = Self.machine()
        let calls = Counter()
        let (agent, logs) = Self.agent(machine) { _ in
            await calls.increment()
            // ssh as Shell runs it: a process that is still going when the task is cancelled.
            return await Shell.run(executable: "/bin/sleep", arguments: ["30"], timeout: 60)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        let request = safety == .readOnly ? AgentRequest.status(dialect: .v3) : try Self.restart()
        let task = Task { try await agent.send(request, decoding: AgentActionResult.self, preferring: nil) }
        while await calls.value == 0 { try await Task.sleep(for: .milliseconds(10)) }
        task.cancel()
        let outcome = await task.result

        #expect(await calls.value == 1)
        for route in machine.routes { #expect(agent.backoff.failure(for: route.target) == nil, "\(route.label)") }
        switch (safety, outcome) {
        case (.readOnly, .failure(let error)):
            #expect(error is CancellationError)
        case (.mutation, .failure(let error as AgentFailure)):
            // It may have started on the far side, and cancelling it here does not stop it there.
            #expect(error.dispatch == .unknown)
        default:
            Issue.record("unexpected outcome \(outcome)")
        }

        // A task cancelled before anything was sent sends nothing.
        let early = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await agent.send(request, decoding: AgentActionResult.self, preferring: nil)
        }
        if case .failure(let error) = await early.result { #expect(error is CancellationError) }
        else { Issue.record("a cancelled request was answered") }
        #expect(await calls.value == 1)
    }

    @Test("a reply that cannot be read is an acknowledgement, and is not sent again")
    func unreadableReplyIsNotRetried() async throws {
        let calls = Counter()
        let (agent, logs) = Self.agent(Self.machine()) { _ in
            await calls.increment()
            return Self.result(exit: 0, out: #"{"ok":"yes"}"#)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        await #expect(throws: AgentFailure.self) {
            _ = try await agent.send(try Self.restart(), decoding: AgentActionResult.self, preferring: nil)
        }
        #expect(await calls.value == 1)
    }

    @Test("a mutation is retried elsewhere only when ssh's own log proves nothing ran")
    func mutationRetriesAfterProvableNonDispatch() async throws {
        let calls = Counter()
        let (agent, logs) = Self.agent(Self.machine()) { arguments in
            await calls.increment()
            // Connection refused: nothing was ever handed to the far side.
            FakeSSH.writeLog("ssh: connect to host x port 22: Connection refused\n", for: arguments)
            return Self.result(exit: 255)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        _ = try? await agent.send(try Self.restart(), decoding: AgentActionResult.self, preferring: nil)
        // Both routes, because neither could have run anything. Systems are not retried per route
        // for a connection-level failure.
        #expect(await calls.value == 2)
        // And every log was read back and deleted.
        #expect(((try? FileManager.default.contentsOfDirectory(atPath: logs.path)) ?? []).isEmpty)
    }

    @Test("a read goes on to the next route after an unexplained exit 255")
    func readsFailOver() async throws {
        let destinations = Recorder()
        let (agent, logs) = Self.agent(Self.machine()) { arguments in
            await destinations.append(FakeSSH.destination(arguments) ?? "")
            if await destinations.count == 1 { return Self.result(exit: 255) }
            return Self.result(out: TransportTests.statusJSON)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        let reply = try await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        #expect(reply.value.ok == true)
        // The second attempt is on the other route: linkLost says nothing good about the first one.
        #expect(await destinations.values == ["me@100.64.0.10", "me@10.0.0.40"])
    }

    @Test("a route-level failure skips the other systems on that address")
    func routeLevelFailureSkipsSystems() async throws {
        // Two routes and two systems is four combinations; a machine that is simply asleep should
        // cost two attempts, not four, or a status call takes four connect timeouts to fail.
        let calls = Counter()
        let (agent, logs) = Self.agent(Self.machine()) { arguments in
            await calls.increment()
            FakeSSH.writeLog("ssh: connect to host x port 22: No route to host\n", for: arguments)
            return Self.result(exit: 255)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        _ = try? await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        #expect(await calls.value == 2)
    }

    @Test("a status after two dead routes asks for only the time that is left")
    func statusBudgetShrinks() async throws {
        // C9. Every attempt used to send 20000 whatever was left of the 30 s, so after two dead
        // routes the working one was cut off by this app's own deadline. Each dead route here costs
        // the eight seconds a connect timeout would.
        let machine = Self.machine(endpoints: [("remote", "192.0.2.10", nil), ("remote", "192.0.2.11", nil),
                                               ("remote", "192.0.2.12", nil)],
                                   systems: [("linux", .linux)])
        let clock = TestClock()
        let lines = Recorder()
        let (base, logs) = Self.agent(machine) { arguments in
            await lines.append(FakeSSH.line(arguments))
            if await lines.count < 3 {
                clock.advance(8)
                FakeSSH.writeLog("ssh: connect to host x port 22: Operation timed out\n", for: arguments)
                return Self.result(exit: 255)
            }
            return Self.result(out: TransportTests.statusJSON)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        var agent = base
        agent.now = { clock.now() }
        _ = try await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        #expect(await lines.values == ["node /agent.mjs status --budget-ms 20000",
                                       "node /agent.mjs status --budget-ms 17000",
                                       "node /agent.mjs status --budget-ms 9000"])

        // An agent not known to speak contract 3 gets no budget at all.
        let legacy = AgentRequest.status(dialect: .legacy)
        #expect(legacy.arguments(remainingMilliseconds: 14_000, attemptsMade: 2) == ["status"])
    }

    @Test("an address outside the dial grammar is never dialled, and the next one is")
    func undialableAddressIsSkipped() async throws {
        // T5. The host is passed after `--`, but a known_hosts pattern or an option-shaped host is
        // still nothing this app should ever hand to ssh.
        let machine = Self.machine(endpoints: [("remote", "victim.example,*", nil), ("remote", "192.0.2.10", nil)],
                                   systems: [("linux", .linux)])
        let destinations = Recorder()
        let (agent, logs) = Self.agent(machine) { arguments in
            await destinations.append(FakeSSH.destination(arguments) ?? "")
            return Self.result(out: TransportTests.statusJSON)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        _ = try await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        #expect(await destinations.values == ["me@192.0.2.10"])

        let only = Self.machine(endpoints: [("lan", "-oProxyCommand=x", nil)], systems: [("linux", .linux)])
        let (refusing, refusingLogs) = Self.agent(only) { arguments in
            await destinations.append(FakeSSH.destination(arguments) ?? "")
            return Self.result(out: TransportTests.statusJSON)
        }
        defer { try? FileManager.default.removeItem(at: refusingLogs) }
        await #expect(throws: AgentFailure(.badRequest, system: only.systems[0], route: only.routes[0].label,
                                           detail: Addresses.problem(with: only.routes[0].target)!, dispatch: .never)) {
            _ = try await refusing.send(try Self.restart(), decoding: AgentActionResult.self, preferring: nil)
        }
        #expect(await destinations.count == 1)
    }

    @Test("a line a system's grammar refuses is not sent, and a mutation may go on to the next system")
    func refusedLineIsBadRequest() async throws {
        // The walk vectors pin this: nothing left this Mac, so the next system is as safe as the first.
        let machine = Machine(
            id: "tower",
            endpoints: [Endpoint(id: "lan", kind: "lan", host: "192.0.2.10", user: "me")],
            systems: [SystemConfig(id: "windows", platform: .windows, agent: ["node", "C:\\100%\\a.mjs"], shell: "cmd"),
                      SystemConfig(id: "linux", platform: .linux, agent: ["node", "/a.mjs"])]
        )
        let lines = Recorder()
        let (agent, logs) = Self.agent(machine) { arguments in
            await lines.append(FakeSSH.line(arguments))
            return Self.result(out: #"{"ok":true,"contract":3,"action":"restarted"}"#)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        let reply = try await agent.send(try Self.restart(), decoding: AgentActionResult.self, preferring: nil)
        #expect(reply.system.id == "linux")
        #expect(await lines.count == 1)
    }

    @Test("a route that reported a restricted session gets the dispatcher's grammar from then on")
    func restrictedSessionIsRemembered() async throws {
        let machine = Machine(
            id: "tower",
            endpoints: [Endpoint(id: "lan", kind: "lan", host: "192.0.2.10", user: "me")],
            systems: [SystemConfig(id: "windows", platform: .windows,
                                   agent: ["C:\\node.exe", "C:\\Users\\me\\launcher.mjs"], shell: "cmd")]
        )
        let lines = Recorder()
        let (agent, logs) = Self.agent(machine) { arguments in
            await lines.append(FakeSSH.line(arguments))
            return Self.result(out: #"{"ok":true,"contract":3,"agent":{"restrictedSession":true},"services":[]}"#)
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        _ = try await agent.send(.status(dialect: .legacy), decoding: AgentStatus.self, preferring: nil)
        _ = try await agent.send(.status(dialect: .legacy), decoding: AgentStatus.self, preferring: nil)
        #expect(await lines.values == ["@\"C:\\node.exe\" \"C:\\Users\\me\\launcher.mjs\" \"status\"",
                                       "'C:\\node.exe' 'C:\\Users\\me\\launcher.mjs' status"])
    }

    @Test("output cut off at the cap is never read as a reply")
    func truncatedOutputIsNoReply() async throws {
        let calls = Counter()
        let (agent, logs) = Self.agent(Self.machine(systems: [("linux", .linux)])) { _ in
            await calls.increment()
            var result = Self.result(out: #"{"ok":true,"contract":3,"action":"restarted"}"#)
            result.standardOutputTruncated = true
            return result
        }
        defer { try? FileManager.default.removeItem(at: logs) }
        var failure: AgentFailure?
        do {
            _ = try await agent.send(try Self.restart(), decoding: AgentActionResult.self, preferring: nil)
        } catch let error as AgentFailure {
            failure = error
        }
        #expect(failure?.kind == .unreadableOutput)
        #expect(failure?.dispatch == .unknown)
        #expect(await calls.value == 1)
    }

    @Test("a system with no agent command is not driven")
    func emptyAgentIsNotTried() {
        let machine = Machine(id: "tower", endpoints: [Endpoint(host: "192.0.2.10")],
                              systems: [SystemConfig(id: "linux", agent: []), SystemConfig(id: "other", agent: ["node", "/a.mjs"])])
        let attempts = RemoteAgent.attempts(machine: machine, preferredSystem: nil, preferredRoute: nil)
        #expect(attempts.map(\.system.id) == ["other"])
    }

    static let statusJSON = #"{"ok":true,"contract":3,"agentVersion":"3.1.0","system":{"id":"linux","name":"Linux"},"services":[]}"#

    @Test("the whole command shares one budget across every attempt")
    func attemptsShareTheBudget() {
        let machine = Self.machine(endpoints: [("remote", "a", nil), ("remote", "b", nil), ("remote", "c", nil)])
        let attempts = RemoteAgent.attempts(machine: machine, preferredSystem: nil, preferredRoute: nil)
        #expect(attempts.count == 6)
    }

    // MARK: - Ordering

    @Test("on the machine's own network the LAN address is dialled first")
    func lanFirstOnSite() {
        let machine = Self.machine()
        let onSite = RemoteAgent.attempts(machine: machine, preferredSystem: nil, preferredRoute: nil, preferLAN: true)
        #expect(onSite.first?.route.isLAN == true)

        let offSite = RemoteAgent.attempts(machine: machine, preferredSystem: nil, preferredRoute: nil, preferLAN: false)
        #expect(offSite.first?.route.isRemote == true)
    }

    @Test("after a boot the endpoints hinted at the new system go first")
    func systemHintedRoutesFirst() {
        let machine = Self.machine(endpoints: [
            ("remote", "linux.example", "linux"),
            ("remote", "windows.example", "windows")
        ])
        let windows = machine.systems.first { $0.id == "windows" }
        let attempts = RemoteAgent.attempts(machine: machine, preferredSystem: windows, preferredRoute: nil)
        #expect(attempts.first?.route.target.host == "windows.example")
        #expect(attempts.first?.system.id == "windows")
        // System labels are routing hints; the other command shape remains available if the
        // authenticated machine is actually running another configured system.
        #expect(attempts.contains { $0.route.target.host == "windows.example" && $0.system.id == "linux" })
    }

    @Test("every route inherits the machine's key rather than dropping it on endpoints")
    func endpointsInheritTheIdentity() {
        // The exact defect: the key lived on the `ssh` block and the endpoints were built without
        // one, so a machine reachable only at an endpoint was dialled with no key at all.
        let machine = Machine(
            id: "workstation",
            ssh: SSHTarget(host: "workstation", user: nil, port: nil, identityFile: "~/.ssh/legacy"),
            endpoints: [Endpoint(id: "lan", kind: "lan", host: "10.0.0.40", port: nil, user: "me",
                                 system: nil, label: nil)],
            systems: [SystemConfig(id: "linux", agent: ["node", "/agent.mjs"])]
        )
        var bindings = Bindings()
        bindings.identityFile = "~/.ssh/device"
        let routes = machine.routes(bindings: bindings)
        #expect(routes.count == 1)
        #expect(routes.allSatisfy { $0.target.identityFile == "~/.ssh/device" })

        // With no device-wide key, the deprecated shared one is still honoured rather than dropped.
        let fallback = machine.routes(bindings: .empty)
        #expect(fallback.allSatisfy { $0.target.identityFile == "~/.ssh/legacy" })

        let legacyOnly = Machine(
            id: "old-workstation",
            ssh: SSHTarget(host: "old-workstation", user: nil, port: nil, identityFile: "~/.ssh/legacy"),
            systems: [SystemConfig(id: "linux", agent: ["node", "/agent.mjs"])]
        )
        #expect(legacyOnly.routes(bindings: .empty).map(\.target.host) == ["old-workstation"])
    }

    @Test("a private ssh alias is dialled before the addresses in the shared document")
    func aliasFirst() {
        let machine = Self.machine()
        var bindings = Bindings()
        bindings.machines = ["workstation": Bindings.MachineBinding(identityFile: nil, sshAlias: "work-lan",
                                                                    port: nil, user: nil)]
        let routes = machine.routes(bindings: bindings)
        #expect(routes.first?.target.host == "work-lan")

        let onSite = RemoteAgent.attempts(machine: machine, preferredSystem: nil,
                                          preferredRoute: nil, bindings: bindings, preferLAN: true)
        #expect(onSite.first?.route.target.host == "work-lan")
    }

    @Test("a deprecated shared alias is ignored when explicit endpoints exist")
    func sharedAliasDoesNotWakeAfterEndpointsFail() {
        let machine = Machine(
            id: "workstation",
            ssh: SSHTarget(host: "legacy-auto-wake", user: nil, port: nil, identityFile: nil),
            endpoints: [Endpoint(id: "remote", kind: "remote", host: "work.example")],
            systems: [SystemConfig(id: "linux", agent: ["node", "/agent.mjs"])]
        )
        #expect(machine.routes(bindings: .empty).map(\.target.host) == ["work.example"])
    }

    // MARK: - Argument safety

    @Test("an id outside the agent's grammar never reaches a shell")
    func idsAreValidated() {
        #expect(throws: AgentRequest.Invalid.self) {
            _ = try AgentRequest.update(service: "demo; rm -rf /", intent: .manual, dialect: .v3)
        }
        #expect(throws: AgentRequest.Invalid.self) {
            _ = try AgentRequest.run(action: "$(whoami)", intent: .manual, dialect: .v3)
        }
        #expect(throws: AgentRequest.Invalid.self) {
            _ = try AgentRequest.operation(id: "not a uuid", wait: nil)
        }
        #expect(AgentToken.isValid("t3-code"))
        #expect(AgentToken.isValid("C:\\Users\\me\\agent.mjs"))
        #expect(!AgentToken.isValid("has space"))
        #expect(!AgentToken.isValid("-leading-dash"))
    }

    @Test("v3-only flags are never sent to an agent that would refuse them")
    func flagsAreGatedOnTheContract() throws {
        let legacy = try AgentRequest.update(
            service: "demo",
            intent: .init(force: true, whenIdle: true, detach: true, operationId: AgentToken.newOperationId()),
            dialect: .legacy
        )
        #expect(legacy.arguments == ["update", "--service", "demo", "--force"])

        let v3 = try AgentRequest.update(
            service: "demo",
            intent: .init(force: false, whenIdle: false, detach: true, operationId: "0123456789abcdef"),
            dialect: .v3
        )
        #expect(v3.arguments == ["update", "--service", "demo", "--op", "0123456789abcdef", "--detach"])
    }

    @Test("a queued request carries an expiry in the form the agent accepts")
    func queuedExpiry() throws {
        let request = try AgentRequest.restart(
            service: "demo",
            intent: .init(whenIdle: true, expires: 4 * 60 * 60, operationId: "0123456789abcdef"),
            dialect: .v3
        )
        #expect(request.arguments.contains("--when-idle"))
        #expect(request.arguments.contains("4h"))
        // A queued request answers at once, so it does not need the long timeout an inline one does.
        #expect(request.timeout <= 60)
    }
}

/// A counter that can be touched from the transport's `@Sendable` runner.
actor Counter {
    private(set) var value = 0
    func increment() { value += 1 }
    func reset() { value = 0 }
}

/// What a fake transport was given, in order.
actor Recorder {
    private(set) var values: [String] = []
    var count: Int { values.count }
    func append(_ value: String) { values.append(value) }
}

/// A clock a test moves by hand.
final class TestClock: @unchecked Sendable {
    private let lock = NSLock()
    private var date = Date(timeIntervalSince1970: 1_000)
    func now() -> Date { lock.withLock { date } }
    func advance(_ seconds: TimeInterval) { lock.withLock { date.addTimeInterval(seconds) } }
}

struct RouteBackoffTests {
    final class Clock: @unchecked Sendable {
        let lock = NSLock()
        var date = Date(timeIntervalSince1970: 100)
        func now() -> Date { lock.withLock { date } }
        func advance(_ seconds: TimeInterval) { lock.withLock { date.addTimeInterval(seconds) } }
    }

    @Test("the same failed address stays backed off for sixty seconds across OS hints")
    func addressCooldown() {
        let clock = Clock()
        let backoff = RouteBackoff(now: { clock.now() })
        let target = SSHTarget(host: "192.168.50.20", user: "linux", port: 22, identityFile: nil)
        let otherSystem = SSHTarget(host: target.host, user: "windows", port: nil, identityFile: nil)
        backoff.record(AgentFailure(.hostUnreachable, dispatch: .never), target: target)
        clock.advance(59)
        #expect(backoff.failure(for: otherSystem) != nil)
        #expect(backoff.failure(for: .init(host: target.host, user: nil, port: 2222, identityFile: nil)) == nil)
        clock.advance(1)
        #expect(backoff.failure(for: otherSystem) == nil)
    }

    @Test("agent argument editing preserves spaces and rejects shell strings")
    func argumentArray() {
        #expect(CommandArguments.parse(#"["/opt/node", "/Users/me/My Name/base/bin/launcher.mjs"]"#) == ["/opt/node", "/Users/me/My Name/base/bin/launcher.mjs"])
        #expect(CommandArguments.parse("node /agent.mjs") == nil)
        #expect(CommandArguments.parse(#"["node", ""]"#) == nil)
        #expect(!AgentToken.isValidID("service/path"))
        #expect(!AgentToken.isValidID("service\n"))
        #expect(AgentToken.isValidSetupID("setup:home"))
        #expect(!AgentToken.isValidSetupID("setup:home\n"))
    }
}
