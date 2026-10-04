import Foundation

/// Whether a command may be tried again somewhere else.
///
/// The transport walks a list of routes and a list of systems, because a machine can be reachable at
/// more than one address and can be running more than one operating system. Walking that list is
/// free for a question and dangerous for an instruction: "reboot" tried on the second route after
/// the first one timed out is a reboot that may happen twice, or happen at all when the user was
/// told it had failed.
enum CommandSafety: Sendable, Equatable {
    /// Reads nothing but state. Safe to try on every route and every system.
    case readOnly
    /// Changes something. Only ever tried again after a failure that proves nothing ran.
    case mutation
}

/// One attempt's worth of context, kept so the UI can say which address and which system answered.
struct AgentReply<Value: Sendable>: Sendable {
    var value: Value
    /// Which of the configured systems the command shape actually worked for. Worth remembering for
    /// the next call.
    var system: SystemConfig
    /// Which address it went over.
    var route: MachineRoute
    /// How many attempts it took, for the diagnostics page.
    var attempts: Int
}

/// Talks to the agent on one machine over ssh. No daemon, no ports, no credentials: every call is
/// one `ssh <destination> <command line>` round trip, with both halves taken from the controller
/// config.
struct RemoteAgent: Sendable {
    static let sshPath = "/usr/bin/ssh"

    /// The machine this instance drives. Replaced wholesale when the config changes.
    var machine: Machine

    /// This device's private settings: the key to offer, the alias to dial. Never published.
    var bindings: Bindings = .empty

    /// True when this device believes it is on the machine's own network, which makes the `lan`
    /// endpoints the ones worth dialling first. A hint, and never a claim about who answers: the
    /// pinned host key settles that on every connection.
    var preferLAN: Bool = false

    var connectTimeout: Int = 8
    var backoff = RouteBackoff()
    /// What the last status over each route said about the key's session. Shared by copies, like
    /// the backoff, because both are about the far side rather than about this value.
    var sessions = RouteSessions()

    /// Swapped out in tests. Everything above this line is pure decision making; this is the only
    /// thing that touches a process.
    var runner: @Sendable (_ executable: String, _ arguments: [String], _ timeout: TimeInterval, _ input: Data?) async -> CommandResult = {
        await Shell.run(executable: $0, arguments: $1, timeout: $2, input: $3)
    }

    // MARK: - Commands

    /// Every command goes through one door.
    ///
    /// The argv, the timeout and whether a failure may be retried elsewhere all come from the
    /// request, which is built in AgentRequest and is the same object the local Mac transport uses.
    /// There is no second idea anywhere in this app of what a command looks like.
    func send<Value: Decodable & Sendable>(
        _ request: AgentRequest,
        decoding: Value.Type,
        preferring system: SystemConfig?,
        route: MachineRoute? = nil
    ) async throws -> AgentReply<Value> {
        try await call(request, decoding: Value.self, preferring: system, route: route)
    }

    func status(dialect: AgentDialect = .legacy, preferring system: SystemConfig?,
                route: MachineRoute? = nil) async throws -> AgentReply<AgentStatus> {
        try await send(.status(dialect: dialect), decoding: AgentStatus.self, preferring: system, route: route)
    }

    // MARK: - Transport

    /// One attempt: an address and a command shape to try it with.
    struct Attempt: Sendable, Equatable {
        var route: MachineRoute
        var system: SystemConfig
    }

    /// The order to try things in.
    ///
    /// Routes outer, systems inner. A system hint orders both routes and command shapes, while the
    /// authenticated response establishes what is actually running. A system with no agent command
    /// is never driven, so it is not tried at all.
    static func attempts(
        machine: Machine,
        preferredSystem: SystemConfig?,
        preferredRoute: MachineRoute?,
        bindings: Bindings = .empty,
        preferLAN: Bool = false
    ) -> [Attempt] {
        let allRoutes = machine.routes(bindings: bindings)
        let drivable = machine.systems.filter { !$0.agent.isEmpty }
        guard !allRoutes.isEmpty, !drivable.isEmpty else { return [] }

        var routes = allRoutes

        // A private alias is an explicit device-local override. Keep it ahead of remembered and
        // site-aware routes; those order the configured endpoints only.
        let privateAliases = routes.filter(\.isPrivateAlias)
        routes.removeAll(where: \.isPrivateAlias)

        // On the machine's own network the LAN addresses are the fast ones and the remote ones go
        // through a tunnel; off it, only the remote ones can work at all. A stable sort, so
        // everything else keeps the order the setup lists it in.
        if preferLAN {
            routes = routes.filter(\.isLAN) + routes.filter { !$0.isLAN }
        } else {
            routes = routes.filter { !$0.isLAN } + routes.filter(\.isLAN)
        }

        // The system that is expected next goes first among the endpoints pinned to a system. After
        // a reboot into another system this is what stops the app dialling the address that only
        // answers while the previous one was up.
        if let preferredSystem {
            let hinted = routes.filter { $0.systemId == preferredSystem.id }
            if !hinted.isEmpty {
                routes = hinted + routes.filter { $0.systemId != preferredSystem.id }
            }
        }

        if let preferredRoute, let index = routes.firstIndex(where: { $0.id == preferredRoute.id }) {
            routes.insert(routes.remove(at: index), at: 0)
        }

        routes = privateAliases + routes

        var systems = drivable
        if let preferredSystem, let index = systems.firstIndex(where: { $0.id == preferredSystem.id }) {
            systems.insert(systems.remove(at: index), at: 0)
        }

        var attempts: [Attempt] = []
        for route in routes {
            let usable = route.systemId.map { hint in
                systems.filter { $0.id == hint } + systems.filter { $0.id != hint }
            } ?? systems
            for system in usable {
                attempts.append(Attempt(route: route, system: system))
            }
        }
        return attempts
    }

    private func call<Value: Decodable & Sendable>(
        _ request: AgentRequest,
        decoding: Value.Type,
        preferring preferred: SystemConfig?,
        route preferredRoute: MachineRoute?
    ) async throws -> AgentReply<Value> {
        let arguments = request.arguments
        let timeout = request.timeout
        let safety = request.safety
        let input = request.input
        let attempts = Self.attempts(machine: machine, preferredSystem: preferred,
                                     preferredRoute: preferredRoute, bindings: bindings,
                                     preferLAN: preferLAN)
        guard !attempts.isEmpty else {
            if machine.routes(bindings: bindings).isEmpty {
                throw AgentFailure(.noRoute, detail: "\(machine.name) has no ssh host and no endpoints.", dispatch: .never)
            }
            throw AgentFailure(.badRequest, detail: CommandLineRefusal("no-command").message, dispatch: .never)
        }

        // Every attempt shares the budget the caller gave the whole command. Without this a machine
        // with three addresses and two systems could spend six times the status timeout finding out
        // it is asleep, which is exactly the "reachable machine looks unavailable" failure.
        let deadline = Date().addingTimeInterval(timeout)
        var best: AgentFailure?
        var tried = 0

        var index = 0
        while index < attempts.count {
            let attempt = attempts[index]
            index += 1
            if var failure = backoff.failure(for: attempt.route.target) {
                failure.dispatch = .never
                best = Self.preferred(best, failure)
                while index < attempts.count, attempts[index].route.id == attempt.route.id { index += 1 }
                continue
            }
            let remaining = deadline.timeIntervalSinceNow
            // Below this there is not enough time left for a connection, let alone a command, and
            // spending it produces a timeout that says nothing.
            if tried > 0, remaining < 4 { break }

            // Neither of these is sent anywhere, so neither can have run. An address this app does
            // not dial is the same refusal for every system behind it.
            if let problem = Addresses.problem(with: attempt.route.target) {
                best = Self.preferred(best, AgentFailure(.badRequest, system: attempt.system, route: attempt.route.label,
                                                         detail: problem, dispatch: .never))
                while index < attempts.count, attempts[index].route.id == attempt.route.id { index += 1 }
                continue
            }
            let command = RemoteCommand(system: attempt.system, arguments: arguments,
                                        restrictedSession: sessions.isRestricted(attempt.route.target))
            let line: String
            do throws(CommandLineRefusal) {
                line = try command.commandLine()
            } catch {
                best = Self.preferred(best, AgentFailure(.badRequest, system: attempt.system, route: attempt.route.label,
                                                         detail: error.message, dispatch: .never))
                continue
            }

            tried += 1
            let result = await runner(
                Self.sshPath,
                RemoteCommand.sshArguments(
                    target: attempt.route.target,
                    connectTimeout: min(connectTimeout, max(2, Int(remaining))),
                    command: line,
                    knownHostsFile: AppPaths.overridesKnownHosts
                        ? AppPaths.knownHostsFile.path(percentEncoded: false)
                        : nil
                ),
                max(4, remaining),
                input
            )

            // Decode before classifying failures: `boot` prints its JSON and then pulls the machine
            // out from under ssh, so a non-zero exit with a good reply still means the agent spoke.
            if let json = Self.extractJSONObject(from: result.standardOutput) {
                do {
                    let value = try JSONDecoder().decode(Value.self, from: json)
                    backoff.clear(attempt.route.target)
                    if request.isStatusRead { sessions.record(json, target: attempt.route.target) }
                    return AgentReply(value: value, system: attempt.system, route: attempt.route, attempts: tried)
                } catch {
                    throw AgentFailure(
                        .unreadableOutput,
                        system: attempt.system,
                        route: attempt.route.label,
                        detail: "\(error.localizedDescription) Output: \(Self.condense(result.standardOutput))",
                        // The agent answered; we simply could not read it. Nothing about that says
                        // the command did not run.
                        dispatch: .acknowledged
                    )
                }
            }

            var failure = Self.classify(result, attempt: attempt, timeout: Int(timeout))
            failure.target = attempt.route.target
            if Self.isRouteLevel(failure.kind) { backoff.record(failure, target: attempt.route.target) }

            // The rule the whole file exists for. A mutation whose fate is not known is never sent
            // anywhere else, on any route, under any system: it is reported as it is.
            if safety == .mutation, !failure.dispatch.isSafeToRetryMutation {
                throw failure
            }

            best = Self.preferred(best, failure)

            // A failure at the connection level says nothing about the command shape, so trying the
            // other systems on this same address would be three more attempts at a machine that is
            // not answering. Skip to the next route instead.
            if Self.isRouteLevel(failure.kind) {
                while index < attempts.count, attempts[index].route.id == attempt.route.id { index += 1 }
            }
        }

        throw best ?? AgentFailure(.unreadableOutput, detail: "No output.", dispatch: .unknown)
    }

    // MARK: - Reading what came back

    static func classify(_ result: CommandResult, attempt: Attempt, timeout: Int) -> AgentFailure {
        if let launchFailure = result.launchFailure {
            return AgentFailure(.launchFailed, system: attempt.system, route: attempt.route.label,
                                detail: launchFailure, dispatch: .never)
        }
        if result.timedOut {
            // Our own watchdog. The far side may be halfway through an install, so this is the one
            // outcome that is genuinely unknown and must never be read as either success or failure.
            return AgentFailure(.timedOut(seconds: timeout), system: attempt.system,
                                route: attempt.route.label,
                                detail: condense(result.failureText), dispatch: .unknown)
        }

        let text = result.standardError + "\n" + result.standardOutput

        // ssh reserves exit status 255 for its own failures. Anything else came from the far side,
        // which means a session was established and the command ran.
        if result.exitCode == 255 {
            let diagnosis = SSHDiagnosis.classify(text)
                ?? (.hostUnreachable, Dispatch.never)
            return AgentFailure(diagnosis.0, system: attempt.system, route: attempt.route.label,
                                detail: condense(result.failureText), dispatch: diagnosis.1)
        }

        // Which failure we report matters, because every attempt but one is aimed at a system that
        // is not running and is guaranteed to fail. An interpreter that was not found means we used
        // the wrong shape; an interpreter that ran and could not find the script means we used the
        // right shape and the agent really is not installed.
        if SSHDiagnosis.agentIsMissing(text) {
            return AgentFailure(.agentMissing, system: attempt.system, route: attempt.route.label,
                                detail: condense(result.failureText), dispatch: .never)
        }
        if SSHDiagnosis.interpreterIsMissing(text) {
            return AgentFailure(.interpreterMissing, system: attempt.system, route: attempt.route.label,
                                detail: condense(result.failureText), dispatch: .never)
        }

        // Something ran on the far side and said something we do not understand. It got as far as a
        // shell, so we cannot claim it did nothing.
        return AgentFailure(.unreadableOutput, system: attempt.system, route: attempt.route.label,
                            detail: condense(result.failureText), dispatch: .unknown)
    }

    /// Whether the failure is about the address rather than about the command.
    static func isRouteLevel(_ kind: AgentFailure.Kind) -> Bool {
        switch kind {
        case .hostUnreachable, .connectionRefused, .authenticationFailed,
             .hostKeyChanged, .hostKeyUnknown, .launchFailed, .noRoute, .timedOut, .linkLost:
            true
        case .interpreterMissing, .agentMissing, .unreadableOutput, .agentFailed, .badRequest:
            false
        }
    }

    /// Which of two failures is the one worth telling the user about.
    ///
    /// A machine with two configured systems produces one failure per system on every call, and
    /// exactly one of those failures is about the system that is actually running. "The agent is not
    /// installed" is the most specific thing that can be said and it is nearly always the true one,
    /// so it beats "the interpreter is missing", which is what the other, sleeping system always
    /// says. Anything about the connection beats both: it explains all of them at once.
    static func preferred(_ existing: AgentFailure?, _ candidate: AgentFailure) -> AgentFailure {
        guard let existing else { return candidate }
        func rank(_ failure: AgentFailure) -> Int {
            switch failure.kind {
            case .hostKeyChanged, .hostKeyUnknown, .authenticationFailed: 5
            case .connectionRefused, .hostUnreachable, .launchFailed, .noRoute: 4
            case .timedOut, .linkLost: 4
            case .agentMissing: 3
            case .unreadableOutput, .agentFailed, .badRequest: 2
            case .interpreterMissing: 1
            }
        }
        return rank(candidate) > rank(existing) ? candidate : existing
    }

    static func condense(_ text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.count <= 400 { return trimmed }
        return String(trimmed.prefix(400)) + "..."
    }

    /// PowerShell prepends a CLIXML banner and can interleave progress records, so take the slice
    /// from the first brace to the last brace instead of trusting the whole stream to be JSON.
    static func extractJSONObject(from text: String) -> Data? {
        guard let start = text.firstIndex(of: "{"), let end = text.lastIndex(of: "}"), start < end else {
            return nil
        }
        return String(text[start...end]).data(using: .utf8)
    }
}

/// What the last status over each route said about the key's session.
///
/// A key bound to the restricted dispatcher answers status with `agent.restrictedSession: true`, and
/// from then on every line over that route is written in the dispatcher's grammar, whatever the
/// document says about the system (section 15.1 of the contract). Keyed by the account, address and
/// key, because restriction is a property of one key on one account.
final class RouteSessions: @unchecked Sendable {
    private let lock = NSLock()
    private var restricted: [String: Bool] = [:]

    private func key(_ target: SSHTarget) -> String {
        "\(target.destination):\(target.port ?? 22):\(target.identityFile ?? "")"
    }

    func isRestricted(_ target: SSHTarget) -> Bool {
        lock.withLock { restricted[key(target)] ?? false }
    }

    /// Takes what a decoded status reply says. A reply that does not say it is not restricted.
    func record(_ json: Data, target: SSHTarget) {
        struct Report: Decodable {
            struct Agent: Decodable { var restrictedSession: Bool? }
            var agent: Agent?
        }
        let report = try? JSONDecoder().decode(Report.self, from: json)
        lock.withLock { restricted[key(target)] = report?.agent?.restrictedSession == true }
    }
}
