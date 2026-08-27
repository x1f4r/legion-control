import Foundation

enum MacAgentError: Error, Sendable {
    /// No node interpreter anywhere we know to look.
    case nodeMissing(String)
    /// node is here, the agent is not.
    case agentMissing(String)
    /// The agent ran and printed something we could not read.
    case unreadableOutput(String)
    /// The agent replied cleanly, and what it said was that it had failed.
    case agentFailed(String)

    var message: String {
        switch self {
        case .nodeMissing:
            "No node interpreter was found on this Mac, so the local agent cannot run."
        case .agentMissing:
            "The control agent is not installed on this Mac. Run the installer here."
        case .unreadableOutput(let detail):
            "The local agent replied with something unreadable. \(detail)"
        case .agentFailed(let detail):
            "The local agent reported a problem. \(detail)"
        }
    }

    var detail: String? {
        switch self {
        case .nodeMissing(let detail): detail
        case .agentMissing(let detail): detail
        case .unreadableOutput(let detail): detail
        case .agentFailed(let detail): detail
        }
    }
}

/// The same agent the other machines run, invoked locally on this Mac. No ssh, no network: one
/// `node <agent> <command>` per call, which is why the local side costs a few tens of milliseconds
/// instead of a round trip. Where the script lives is the config's business, not this file's.
actor MacAgent {
    private let scriptPath: String

    init(scriptPath: String) {
        self.scriptPath = scriptPath
    }

    /// Resolved once per app run. Finding node costs a process launch at worst, and the answer does
    /// not change while the app is open.
    private var cachedNode: String?

    // MARK: - Commands

    func status() async throws -> AgentStatus {
        try await call(["status"], decoding: AgentStatus.self, timeout: 30)
    }

    /// Deliberately never passes --force.
    ///
    /// On this Mac an update can mean quitting an app and starting it again, so --force would mean
    /// "close the editor out from under whatever it is doing". The agent's refusal while work is in
    /// flight is the entire safety property of this button, and the app must not have a way to
    /// switch it off.
    func update(service: String?) async throws -> AgentActionResult {
        var arguments = ["update"]
        if let service { arguments += ["--service", service] }
        return try await call(arguments, decoding: AgentActionResult.self, timeout: 420)
    }

    func setAutoUpdate(_ enabled: Bool) async throws -> AgentActionResult {
        try await call(["auto-update", enabled ? "on" : "off"], decoding: AgentActionResult.self, timeout: 30)
    }

    /// Hands the agent on this Mac the controller config the app is running on. The same command the
    /// other machines get, minus the ssh in front of it.
    func pushControllerConfig(_ bytes: Data) async throws -> AgentConfigResult {
        try await call(["config", "set"], decoding: AgentConfigResult.self, timeout: 40, input: bytes)
    }

    // MARK: - Transport

    private func call<Value: Decodable & Sendable>(
        _ arguments: [String],
        decoding: Value.Type,
        timeout: TimeInterval,
        input: Data? = nil
    ) async throws -> Value {
        let node = try await resolveNode()

        guard FileManager.default.isReadableFile(atPath: scriptPath) else {
            throw MacAgentError.agentMissing("Nothing to run at \(scriptPath).")
        }

        let result = await Shell.run(executable: node, arguments: [scriptPath] + arguments,
                                     timeout: timeout, input: input)

        if let launchFailure = result.launchFailure {
            throw MacAgentError.nodeMissing(launchFailure)
        }
        if result.timedOut {
            throw MacAgentError.agentFailed("The command did not finish within \(Int(timeout)) seconds.")
        }

        // Decode first: the agent reports its own refusals as a clean JSON object with a non-zero exit
        // code, and those refusals are the interesting answers, not failures.
        if let json = RemoteAgent.extractJSONObject(from: result.standardOutput) {
            do {
                return try JSONDecoder().decode(Value.self, from: json)
            } catch {
                throw MacAgentError.unreadableOutput("\(error.localizedDescription) Output: \(condense(result.standardOutput))")
            }
        }

        let text = (result.standardError + " " + result.standardOutput).lowercased()
        if text.contains("cannot find module") || text.contains("err_module_not_found") {
            throw MacAgentError.agentMissing(condense(result.failureText))
        }
        throw MacAgentError.unreadableOutput(condense(result.failureText))
    }

    // MARK: - Finding node

    private func resolveNode() async throws -> String {
        if let cachedNode { return cachedNode }

        for candidate in Self.candidateNodePaths() where isRunnable(candidate) {
            cachedNode = candidate
            return candidate
        }

        // Nothing at the usual addresses. A login shell knows about version managers and anything else
        // the user has set up, and it is worth one process launch before giving up. This is the only
        // place the app reads the user's shell configuration, and only when the obvious paths failed.
        let probe = await Shell.run(
            executable: "/bin/zsh",
            arguments: ["-lc", "command -v node"],
            timeout: 10
        )
        let found = probe.standardOutput
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { isRunnable($0) }
        if let found {
            cachedNode = found
            return found
        }

        throw MacAgentError.nodeMissing(
            "Looked at \(Self.candidateNodePaths().joined(separator: ", ")) and asked a login shell."
        )
    }

    /// The app is launched from Finder, so its PATH is the bare system one: no Homebrew, no version
    /// manager. Every plausible location has to be named outright.
    private static func candidateNodePaths() -> [String] {
        let home = FileManager.default.homeDirectoryForCurrentUser
        var paths: [String] = []
        // An explicit override, for the case where node lives somewhere none of this guesses.
        if let override = UserDefaults.standard.string(forKey: "nodePath"), !override.isEmpty {
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

    private nonisolated func condense(_ text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.count <= 400 { return trimmed }
        return String(trimmed.prefix(400)) + "..."
    }
}
