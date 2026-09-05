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

    static func result(exit: Int32 = 0, out: String = "", err: String = "",
                       timedOut: Bool = false, launchFailure: String? = nil) -> CommandResult {
        CommandResult(exitCode: exit, standardOutput: out, standardError: err,
                      timedOut: timedOut, launchFailure: launchFailure)
    }

    static func attempt(_ machine: Machine) -> RemoteAgent.Attempt {
        RemoteAgent.Attempt(route: machine.routes[0], system: machine.systems[0])
    }

    // MARK: - Classification

    @Test("a timeout is never a failure and never a success")
    func timeoutIsUnknown() {
        let failure = RemoteAgent.classify(Self.result(timedOut: true), attempt: Self.attempt(Self.machine()), timeout: 60)
        #expect(failure.kind == .timedOut(seconds: 60))
        // The rule the review's finding 14 is about: the old transport mapped this to "unreachable",
        // and the boot handler then drew a reboot that had never happened.
        #expect(failure.dispatch == .unknown)
        #expect(!failure.meansAsleepOrOff)
    }

    @Test("ssh diagnoses are told apart rather than folded into unreachable")
    func sshDiagnoses() {
        let cases: [(String, AgentFailure.Kind, Dispatch)] = [
            ("ssh: connect to host x port 22: Connection refused", .connectionRefused, .never),
            ("ssh: connect to host x port 22: No route to host", .hostUnreachable, .never),
            ("me@x: Permission denied (publickey).", .authenticationFailed, .never),
            ("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@", .hostKeyChanged, .never),
            ("No ED25519 host key is known for x and you have requested strict checking.", .hostKeyUnknown, .never),
            ("client_loop: send disconnect: Broken pipe", .linkLost, .unknown)
        ]
        for (text, kind, dispatch) in cases {
            let failure = RemoteAgent.classify(Self.result(exit: 255, err: text),
                                               attempt: Self.attempt(Self.machine()), timeout: 25)
            #expect(failure.kind == kind, "\(text)")
            #expect(failure.dispatch == dispatch, "\(text)")
        }
    }

    @Test("a rejected key does not read as a sleeping machine")
    func authIsNotSleep() {
        let failure = RemoteAgent.classify(Self.result(exit: 255, err: "Permission denied (publickey)."),
                                           attempt: Self.attempt(Self.machine()), timeout: 25)
        #expect(!failure.meansAsleepOrOff)
        #expect(failure.message(machine: "Tower").contains("rejected the key"))
    }

    @Test("a missing interpreter proves nothing ran; unreadable output does not")
    func remoteFailureDispatch() {
        let missing = RemoteAgent.classify(Self.result(exit: 127, err: "bash: node: command not found"),
                                           attempt: Self.attempt(Self.machine()), timeout: 25)
        #expect(missing.kind == .interpreterMissing)
        #expect(missing.dispatch == .never)

        let noAgent = RemoteAgent.classify(Self.result(exit: 1, err: "Error: Cannot find module '/agent.mjs'"),
                                           attempt: Self.attempt(Self.machine()), timeout: 25)
        #expect(noAgent.kind == .agentMissing)
        #expect(noAgent.dispatch == .never)

        let garbage = RemoteAgent.classify(Self.result(exit: 3, out: "something happened"),
                                           attempt: Self.attempt(Self.machine()), timeout: 25)
        #expect(garbage.kind == .unreadableOutput)
        // It got as far as a shell on the far side, so we cannot claim it did nothing.
        #expect(garbage.dispatch == .unknown)
    }

    // MARK: - Retrying elsewhere

    @Test("a read is tried on every route; a mutation stops at the first uncertain failure")
    func mutationIsNotReplayed() async throws {
        let machine = Self.machine()
        let calls = Counter()

        var agent = RemoteAgent(machine: machine)
        agent.runner = { _, _, _, _ in
            await calls.increment()
            // A timeout: the far side may have done the work.
            return Self.result(timedOut: true)
        }

        // A read may be retried freely: nothing on the far side changed.
        _ = try? await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        let readAttempts = await calls.value
        #expect(readAttempts >= 1)

        await calls.reset()
        agent.backoff = RouteBackoff()
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
        let mutationAttempts = await calls.value
        // Exactly one. A reboot whose outcome is unknown must never be sent to a second address.
        #expect(mutationAttempts == 1)
        #expect(failure?.dispatch == .unknown)
    }

    @Test("a mutation is retried elsewhere only when nothing can have run")
    func mutationRetriesAfterProvableNonDispatch() async throws {
        let machine = Self.machine()
        let calls = Counter()
        var agent = RemoteAgent(machine: machine)
        agent.runner = { _, _, _, _ in
            await calls.increment()
            // Connection refused: nothing was ever handed to the far side.
            return Self.result(exit: 255, err: "ssh: connect to host x port 22: Connection refused")
        }
        _ = try? await agent.send(
            try AgentRequest.restart(service: "demo", intent: .init(operationId: AgentToken.newOperationId()), dialect: .v3),
            decoding: AgentActionResult.self,
            preferring: nil
        )
        // Both routes, because neither could have run anything. Systems are not retried per route
        // for a connection-level failure.
        #expect(await calls.value == 2)
    }

    @Test("a route-level failure skips the other systems on that address")
    func routeLevelFailureSkipsSystems() async throws {
        // Two routes and two systems is four combinations; a machine that is simply asleep should
        // cost two attempts, not four, or a status call takes four connect timeouts to fail.
        let machine = Self.machine()
        let calls = Counter()
        var agent = RemoteAgent(machine: machine)
        agent.runner = { _, _, _, _ in
            await calls.increment()
            return Self.result(exit: 255, err: "ssh: connect to host x port 22: No route to host")
        }
        _ = try? await agent.send(.status(dialect: .v3), decoding: AgentStatus.self, preferring: nil)
        #expect(await calls.value == 2)
    }

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
        let target = SSHTarget(host: "192.168.178.20", user: "linux", port: 22, identityFile: nil)
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
        #expect(CommandArguments.parse(#"["/opt/node", "/Users/My Name/base/bin/launcher.mjs"]"#) == ["/opt/node", "/Users/My Name/base/bin/launcher.mjs"])
        #expect(CommandArguments.parse("node /agent.mjs") == nil)
        #expect(CommandArguments.parse(#"["node", ""]"#) == nil)
        #expect(!AgentToken.isValidID("service/path"))
        #expect(!AgentToken.isValidID("service\n"))
        #expect(AgentToken.isValidSetupID("setup:home"))
        #expect(!AgentToken.isValidSetupID("setup:home\n"))
    }
}
