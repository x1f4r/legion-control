import Foundation

/// The same agent the other machines run, invoked locally on this Mac. No ssh, no network: one
/// `node <agent> <command>` per call, which is why the local side costs a few tens of milliseconds
/// instead of a round trip. Where the script lives is the config's business, not this file's.
///
/// It reports the same failures the remote transport does, in the same shape, because the local Mac
/// is a machine like the others and the app must not have two ideas of what "the agent is not
/// installed" means.
actor MacAgent {
    /// The script to run, when the interpreter has to be found first.
    private let scriptPath: String?
    /// A complete argv from this device's private bindings: the binary and everything that goes in
    /// front of the command. Spawned directly, so there is no shell and nothing to quote — quoting
    /// rules exist because ssh joins words and hands them to a remote shell, and a local spawn has
    /// no such step.
    private let boundArgv: [String]?

    /// Swapped out in tests, and the only thing here that touches a process.
    private let runner: @Sendable (_ executable: String, _ arguments: [String], _ timeout: TimeInterval, _ input: Data?) async -> CommandResult

    /// Set in tests to skip looking for a real node.
    private let interpreterOverride: String?

    init(
        scriptPath: String,
        interpreter: String? = nil,
        runner: @escaping @Sendable (_ executable: String, _ arguments: [String], _ timeout: TimeInterval, _ input: Data?) async -> CommandResult = {
            await Shell.run(executable: $0, arguments: $1, timeout: $2, input: $3)
        }
    ) {
        self.scriptPath = scriptPath
        self.boundArgv = nil
        self.interpreterOverride = interpreter
        self.runner = runner
    }

    /// The form the private bindings use: run exactly this, with the command appended.
    init(
        argv: [String],
        runner: @escaping @Sendable (_ executable: String, _ arguments: [String], _ timeout: TimeInterval, _ input: Data?) async -> CommandResult = {
            await Shell.run(executable: $0, arguments: $1, timeout: $2, input: $3)
        }
    ) {
        self.scriptPath = nil
        self.boundArgv = argv
        self.interpreterOverride = nil
        self.runner = runner
    }

    /// Resolved once per app run. Finding node costs a process launch at worst, and the answer does
    /// not change while the app is open.
    private var cachedNode: String?

    // MARK: - Commands

    /// The same requests the machines over ssh get, minus the ssh in front of them. Built from the
    /// same AgentRequest, so the local Mac and a remote machine can never drift apart about what a
    /// command is — which is exactly the parity the local side used to be missing.
    func send<Value: Decodable & Sendable>(_ request: AgentRequest, decoding: Value.Type) async throws -> Value {
        try await call(request, decoding: Value.self)
    }

    func status(dialect: AgentDialect = .legacy) async throws -> AgentStatus {
        try await send(.status(dialect: dialect), decoding: AgentStatus.self)
    }

    func bootstrapCommand() async throws -> [String] {
        if let boundArgv { return boundArgv }
        guard let scriptPath else { throw AgentBootstrap.Failure(message: "No local agent command is configured.") }
        return [try await resolveNode(), scriptPath]
    }

    // MARK: - Transport

    private func call<Value: Decodable & Sendable>(
        _ request: AgentRequest,
        decoding: Value.Type
    ) async throws -> Value {
        let arguments = request.arguments
        let timeout = request.timeout
        let input = request.input

        let executable: String
        let leading: [String]
        if let boundArgv, !boundArgv.isEmpty {
            executable = boundArgv[0]
            leading = Array(boundArgv.dropFirst())
            guard FileManager.default.isExecutableFile(atPath: executable) else {
                throw AgentFailure(.interpreterMissing,
                                   detail: "This device's bindings name \(executable), which is not an executable file.",
                                   dispatch: .never)
            }
        } else {
            guard let scriptPath else {
                throw AgentFailure(.agentMissing, detail: "No local agent is configured on this device.", dispatch: .never)
            }
            executable = try await resolveNode()
            leading = [scriptPath]
            guard FileManager.default.isReadableFile(atPath: scriptPath) else {
                throw AgentFailure(.agentMissing, detail: "Nothing to run at \(scriptPath).", dispatch: .never)
            }
        }

        let result = await runner(executable, leading + arguments, timeout, input)

        if let launchFailure = result.launchFailure {
            throw AgentFailure(.launchFailed, detail: launchFailure, dispatch: .never)
        }
        if result.timedOut {
            // The same honesty the remote side keeps: the agent may be halfway through an install,
            // so this is unknown rather than failed.
            throw AgentFailure(.timedOut(seconds: Int(timeout)),
                               detail: "The local agent did not finish within \(Int(timeout)) seconds.",
                               dispatch: .unknown)
        }

        // Decode first: the agent reports its own refusals as a clean JSON object with a non-zero
        // exit code, and those refusals are the interesting answers, not failures.
        if let json = RemoteAgent.extractJSONObject(from: result.standardOutput) {
            do {
                return try JSONDecoder().decode(Value.self, from: json)
            } catch {
                throw AgentFailure(.unreadableOutput,
                                   detail: "\(error.localizedDescription) Output: \(RemoteAgent.condense(result.standardOutput))",
                                   dispatch: .acknowledged)
            }
        }

        let text = result.standardError + "\n" + result.standardOutput
        if SSHDiagnosis.agentIsMissing(text) {
            throw AgentFailure(.agentMissing, detail: RemoteAgent.condense(result.failureText), dispatch: .never)
        }
        throw AgentFailure(.unreadableOutput, detail: RemoteAgent.condense(result.failureText), dispatch: .unknown)
    }

    // MARK: - Finding node

    private func resolveNode() async throws -> String {
        if let interpreterOverride { return interpreterOverride }
        if let cachedNode { return cachedNode }

        for candidate in Self.candidateNodePaths() where isRunnable(candidate) {
            cachedNode = candidate
            return candidate
        }

        // Nothing at the usual addresses. A login shell knows about version managers and anything else
        // the user has set up, and it is worth one process launch before giving up. This is the only
        // place the app reads the user's shell configuration, and only when the obvious paths failed.
        let probe = await runner("/bin/zsh", ["-lc", "command -v node"], 10, nil)
        let found = probe.standardOutput
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { isRunnable($0) }
        if let found {
            cachedNode = found
            return found
        }

        throw AgentFailure(
            .interpreterMissing,
            detail: "Looked at \(Self.candidateNodePaths().joined(separator: ", ")) and asked a login shell.",
            dispatch: .never
        )
    }

    /// The app is launched from Finder, so its PATH is the bare system one: no Homebrew, no version
    /// manager. Every plausible location has to be named outright.
    private static func candidateNodePaths() -> [String] {
        let home = FileManager.default.homeDirectoryForCurrentUser
        var paths: [String] = []
        // An explicit override, for the case where node lives somewhere none of this guesses.
        if let override = AppPreferences.string(forKey: "nodePath"), !override.isEmpty {
            paths.append(override)
        }
        paths += [
            "/opt/homebrew/bin/node",
            "/usr/local/bin/node",
            "/usr/bin/node",
        ]
        paths += [".volta/bin/node", ".local/bin/node", ".bun/bin/node"].map {
            home.appending(path: $0).path(percentEncoded: false)
        }
        // nvm keeps one directory per installed version; take the newest by name.
        let nvm = home.appending(path: ".nvm/versions/node").path(percentEncoded: false)
        if let versions = try? FileManager.default.contentsOfDirectory(atPath: nvm) {
            let newest = versions.sorted { $0.compare($1, options: .numeric) == .orderedDescending }.first
            if let newest { paths.append("\(nvm)/\(newest)/bin/node") }
        }
        return paths
    }

    private func isRunnable(_ path: String) -> Bool {
        !path.isEmpty && FileManager.default.isExecutableFile(atPath: path)
    }
}

extension AgentFailure {
    /// The local wording. The remote sentences name the machine and talk about ssh; on this Mac
    /// there is no machine to name and no connection to blame.
    var localMessage: String {
        switch kind {
        case .interpreterMissing:
            "No node interpreter was found on this Mac, so the local agent cannot run."
        case .agentMissing:
            "The control agent is not installed on this Mac. Run the installer here."
        case .launchFailed:
            "The local agent could not be started."
        case .timedOut(let seconds):
            "The local agent did not finish within \(seconds) seconds, so what it did is not known."
        case .unreadableOutput:
            "The local agent replied with something unreadable."
        case .agentFailed:
            "The local agent reported a problem. \(detail)"
        default:
            message(machine: "this Mac")
        }
    }
}
