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
    /// Where ssh writes its own log for each attempt. Moved in tests, so they never write into the
    /// real application support folder.
    var logDirectory: URL = SSHLog.directory

    /// The clock the one deadline is read from. Moved in tests, so a walk over dead routes can be
    /// run without waiting for them.
    var now: @Sendable () -> Date = { Date() }

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

    /// The walk of section 15.5 of the contract.
    ///
    /// One deadline covers the whole command, so a machine with three addresses and two systems
    /// cannot spend six times the timeout finding out it is asleep. A read goes on after any failure;
    /// a mutation goes on only after a failure that proves nothing ran, and AttemptWalk is where that
    /// rule is written. Each attempt gets a fresh ssh log, because ssh's own words are the only
    /// evidence of where a command stopped.
    private func call<Value: Decodable & Sendable>(
        _ request: AgentRequest,
        decoding: Value.Type,
        preferring preferred: SystemConfig?,
        route preferredRoute: MachineRoute?
    ) async throws -> AgentReply<Value> {
        let timeout = request.timeout
        let attempts = Self.attempts(machine: machine, preferredSystem: preferred,
                                     preferredRoute: preferredRoute, bindings: bindings,
                                     preferLAN: preferLAN)
        guard !attempts.isEmpty else {
            if machine.routes(bindings: bindings).isEmpty {
                throw AgentFailure(.noRoute, detail: "\(machine.name) has no ssh host and no endpoints.", dispatch: .never)
            }
            throw AgentFailure(.badRequest, detail: CommandLineRefusal("no-command").message, dispatch: .never)
        }

        let deadline = now().addingTimeInterval(timeout)
        var best: AgentFailure?
        var made = 0
        var dead = Set<String>()

        for attempt in attempts where !dead.contains(attempt.route.id) {
            if var failure = backoff.failure(for: attempt.route.target) {
                failure.dispatch = .never
                best = Self.preferred(best, failure)
                dead.insert(attempt.route.id)
                continue
            }
            let remaining = deadline.timeIntervalSince(now())
            // Below this there is not enough time left for a connection, let alone a command, and
            // spending it produces a timeout that says nothing.
            if made > 0, remaining < 4 { break }
            guard let arguments = request.arguments(remainingMilliseconds: Int((remaining * 1000).rounded(.down)),
                                                    attemptsMade: made) else { break }

            // Neither of these is sent anywhere, so neither can have run. An address this app does
            // not dial is the same refusal for every system behind it.
            if let problem = Addresses.problem(with: attempt.route.target) {
                best = Self.preferred(best, AgentFailure(.badRequest, system: attempt.system, route: attempt.route.label,
                                                         detail: problem, dispatch: .never))
                dead.insert(attempt.route.id)
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

            made += 1
            let log = SSHLog.create(in: logDirectory)
            let result = await runner(
                Self.sshPath,
                RemoteCommand.sshArguments(
                    target: attempt.route.target,
                    connectTimeout: min(connectTimeout, max(2, Int(remaining))),
                    command: line,
                    knownHostsFile: AppPaths.overridesKnownHosts
                        ? AppPaths.knownHostsFile.path(percentEncoded: false)
                        : nil,
                    logFile: log?.path(percentEncoded: false)
                ),
                remaining,
                request.input
            )
            let sshLog = log.map(SSHLog.consume) ?? ""
            let verdict = AttemptClassifier.classify(AttemptEvidence(result, sshLog: sshLog), timeout: Int(timeout))

            let step = AttemptWalk.step(after: verdict, safety: request.safety)
            if step == .reply {
                // `boot` prints its JSON and then pulls the machine out from under ssh, so a non-zero
                // exit with a good reply still means the agent spoke.
                let json = AttemptClassifier.jsonObjectSlice(result.standardOutput) ?? Data()
                do {
                    let value = try JSONDecoder().decode(Value.self, from: json)
                    backoff.clear(attempt.route.target)
                    if request.isStatusRead { sessions.record(json, target: attempt.route.target) }
                    return AgentReply(value: value, system: attempt.system, route: attempt.route, attempts: made)
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

            var failure = Self.failure(verdict, attempt: attempt, command: command, result: result, sshLog: sshLog)
            failure.target = attempt.route.target
            if verdict.routeLevel { backoff.record(failure, target: attempt.route.target) }

            // The rule the whole walk exists for. A mutation whose fate is not known is never sent
            // anywhere else, on any route, under any system: it is reported as it is.
            if step == .stop { throw failure }

            best = Self.preferred(best, failure)
            // A failure at the connection level says nothing about the command shape, so trying the
            // other systems on this same address would be more attempts at a machine that is not
            // answering.
            if step == .next(routeIsDead: true) { dead.insert(attempt.route.id) }
        }

        throw best ?? AgentFailure(.unreadableOutput, detail: "No output.", dispatch: .unknown)
    }

    // MARK: - Reading what came back

    /// The failure a verdict stands for, in words a person can act on.
    static func failure(_ verdict: AttemptClassifier.Verdict, attempt: Attempt, command: RemoteCommand,
                        result: CommandResult, sshLog: String) -> AgentFailure {
        var failure = AgentFailure(verdict.kind ?? .unreadableOutput, system: attempt.system, route: attempt.route.label,
                                   detail: detail(result, sshLog: sshLog), dispatch: verdict.dispatch)
        // A cmd line handed to a PowerShell login fails loudly, as a here-string PowerShell cannot
        // parse. On a Windows system that left its shell to the default that is almost certainly
        // what happened, and the fix is one word in the setup.
        if attempt.system.platform == .windows, attempt.system.hasDefaultedShell, !command.restrictedSession,
           ["parsererror", "here-string"].contains(where: { result.standardError.lowercased().contains($0) }) {
            failure.hint = "This account logs in to PowerShell; set shell to powershell."
        }
        return failure
    }

    /// What to show under the sentence: ssh's own words first, because with `-E` they are no longer
    /// in stderr, then whatever the far side said.
    static func detail(_ result: CommandResult, sshLog: String) -> String {
        let logged = CommandResult.informativeLines(sshLog)
        let text = result.failureText
        return condense(logged.isEmpty ? text : logged + "\n" + text)
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
