import Foundation

enum AgentError: Error, Sendable {
    /// ssh could not reach the box at all: it is asleep, off, or off the network.
    case unreachable(String)
    /// We got a shell on the far side but the agent is not there.
    case agentMissing(SystemConfig, String)
    /// The agent ran but printed something we could not read.
    case unreadableOutput(String)
    /// The agent replied cleanly, and what it said was that it had failed.
    case agentFailed(String)

    func message(machine: String) -> String {
        switch self {
        case .unreachable(let detail):
            "\(machine) did not answer. \(detail)"
        case .agentMissing(let system, _):
            "The control agent is not installed on \(system.name). Run the installer on that system."
        case .unreadableOutput(let detail):
            "The agent replied with something unreadable. \(detail)"
        case .agentFailed(let detail):
            "The control agent reported a problem. \(detail)"
        }
    }

    /// Detail worth showing under the status line (raw ssh text, exit codes, and so on).
    var detail: String? {
        switch self {
        case .unreachable(let detail): detail
        case .agentMissing(_, let detail): detail
        case .unreadableOutput(let detail): detail
        case .agentFailed(let detail): detail
        }
    }
}

/// Talks to the agent on one machine over ssh. No daemon, no ports, no credentials: every call is
/// one `ssh <destination> <agent argv> <command>` round trip, with both halves taken from the
/// controller config.
struct RemoteAgent: Sendable {
    static let sshPath = "/usr/bin/ssh"

    /// The machine this instance drives. Replaced wholesale when the config changes.
    var machine: Machine

    var connectTimeout: Int = 8

    struct Reply<Value: Sendable>: Sendable {
        var value: Value
        /// Which of the configured systems the command shape actually worked for. Worth remembering
        /// for the next call.
        var system: SystemConfig
    }

    // MARK: - Commands

    func status(preferring system: SystemConfig?) async throws -> Reply<AgentStatus> {
        try await call(["status"], decoding: AgentStatus.self, timeout: 25, preferring: system)
    }

    /// `--service` is only sent to an agent that reported a services array. The first version of the
    /// agent looks after exactly one thing and rejects flags it has never heard of, and its one
    /// service is the default anyway, so leaving the flag off is the same command.
    func update(service: String?, force: Bool, preferring system: SystemConfig?) async throws -> Reply<AgentActionResult> {
        var arguments = ["update"]
        if let service { arguments += ["--service", service] }
        if force { arguments.append("--force") }
        return try await call(arguments, decoding: AgentActionResult.self, timeout: 420, preferring: system)
    }

    func restart(service: String?, force: Bool, preferring system: SystemConfig?) async throws -> Reply<AgentActionResult> {
        var arguments = ["restart"]
        if let service { arguments += ["--service", service] }
        if force { arguments.append("--force") }
        return try await call(arguments, decoding: AgentActionResult.self, timeout: 240, preferring: system)
    }

    func setAutoUpdate(_ enabled: Bool, preferring system: SystemConfig?) async throws -> Reply<AgentActionResult> {
        try await call(["auto-update", enabled ? "on" : "off"],
                       decoding: AgentActionResult.self, timeout: 40, preferring: system)
    }

    func boot(into target: String, force: Bool, preferring system: SystemConfig?) async throws -> Reply<AgentActionResult> {
        var arguments = ["boot", target]
        if force { arguments.append("--force") }
        // The reboot cuts the connection while ssh is still waiting, so keep this timeout short and
        // treat a dropped link after a successful hand-off as normal.
        return try await call(arguments, decoding: AgentActionResult.self, timeout: 60, preferring: system)
    }

    func sleep(force: Bool, preferring system: SystemConfig?) async throws -> Reply<AgentActionResult> {
        var arguments = ["sleep"]
        if force { arguments.append("--force") }
        // Suspending pulls the machine out from under ssh while it is still waiting, the same as a
        // reboot does, so this timeout is short and a link dropped after the hand-off reads as normal.
        return try await call(arguments, decoding: AgentActionResult.self, timeout: 60, preferring: system)
    }

    func run(action: String, force: Bool, preferring system: SystemConfig?) async throws -> Reply<AgentActionResult> {
        var arguments = ["run", action]
        if force { arguments.append("--force") }
        return try await call(arguments, decoding: AgentActionResult.self, timeout: 300, preferring: system)
    }

    // MARK: - Transport

    private func call<Value: Decodable & Sendable>(
        _ arguments: [String],
        decoding: Value.Type,
        timeout: TimeInterval,
        preferring preferred: SystemConfig?
    ) async throws -> Reply<Value> {
        guard let target = machine.sshTarget else {
            throw AgentError.unreachable("There is no ssh host configured for \(machine.name).")
        }

        // We do not know which system is up until the agent tells us, so try the remembered one
        // first and fall back to the others in the order the config lists them. A wrong guess costs
        // one extra round trip, never a wrong answer, because only the matching system has the
        // interpreter and the script the config names for it.
        let order: [SystemConfig] = {
            guard let preferred, machine.system(id: preferred.id) != nil else { return machine.systems }
            return [preferred] + machine.systems.filter { $0.id != preferred.id }
        }()

        var scriptMissing: AgentError?
        var unreadable: AgentError?
        var wrongShape: AgentError?

        for system in order {
            let result = await Shell.run(
                executable: Self.sshPath,
                arguments: ["-o", "BatchMode=yes", "-o", "ConnectTimeout=\(connectTimeout)"]
                    + target.sshOptions + [target.destination] + system.agent + arguments,
                timeout: timeout
            )

            if let launchFailure = result.launchFailure {
                throw AgentError.unreachable(launchFailure)
            }
            if result.timedOut {
                throw AgentError.unreachable("The command timed out after \(Int(timeout)) seconds.")
            }

            // Decode before classifying failures: `boot` prints its JSON and then pulls the machine out
            // from under ssh, so a non-zero exit with a good reply still means the command worked.
            if let json = Self.extractJSONObject(from: result.standardOutput) {
                do {
                    let value = try JSONDecoder().decode(Value.self, from: json)
                    return Reply(value: value, system: system)
                } catch {
                    throw AgentError.unreadableOutput("\(error.localizedDescription) Output: \(condense(result.standardOutput))")
                }
            }

            if looksUnreachable(result) {
                throw AgentError.unreachable(condense(result.failureText))
            }

            // Which failure we report matters, because every attempt but one is aimed at a system
            // that is not running and is guaranteed to fail. An interpreter that was not found means
            // we used the wrong shape; an interpreter that ran and could not find the script means we
            // used the right shape and the agent really is not installed.
            switch classify(result) {
            case .interpreterMissing:
                if wrongShape == nil { wrongShape = .agentMissing(system, condense(result.failureText)) }
            case .scriptMissing:
                if scriptMissing == nil { scriptMissing = .agentMissing(system, condense(result.failureText)) }
            case .other:
                if unreadable == nil {
                    unreadable = .unreadableOutput(
                        condense(result.standardOutput.isEmpty ? result.standardError : result.standardOutput)
                    )
                }
            }
        }

        throw scriptMissing ?? unreadable ?? wrongShape ?? AgentError.unreadableOutput("No output.")
    }

    /// ssh reserves exit code 255 for its own failures: no route, refused, dropped mid-session, or the
    /// routing helper giving up. The remote command's own exit codes come through unchanged, so 255
    /// without a JSON reply always means we never got a working session, and retrying the other command
    /// shape would be pointless.
    private func looksUnreachable(_ result: CommandResult) -> Bool {
        result.exitCode == 255
    }

    private enum Failure {
        case interpreterMissing
        case scriptMissing
        case other
    }

    private func classify(_ result: CommandResult) -> Failure {
        let text = (result.standardError + " " + result.standardOutput).lowercased()
        let scriptMarkers = [
            "cannot find module", "module_not_found", "err_module_not_found"
        ]
        if scriptMarkers.contains(where: { text.contains($0) }) { return .scriptMissing }

        let interpreterMarkers = [
            "command not found", "unknown command", "not recognized as the name",
            "is not recognized as an internal", "commandnotfoundexception",
            "no such file or directory"
        ]
        if interpreterMarkers.contains(where: { text.contains($0) }) { return .interpreterMissing }

        return .other
    }

    private func condense(_ text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.count <= 400 { return trimmed }
        return String(trimmed.prefix(400)) + "..."
    }

    /// PowerShell prepends a CLIXML banner and can interleave progress records, so take the slice from
    /// the first brace to the last brace instead of trusting the whole stream to be JSON.
    static func extractJSONObject(from text: String) -> Data? {
        guard let start = text.firstIndex(of: "{"), let end = text.lastIndex(of: "}"), start < end else {
            return nil
        }
        return String(text[start...end]).data(using: .utf8)
    }
}
