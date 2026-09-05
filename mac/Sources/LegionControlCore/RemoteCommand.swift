import Foundation

/// Which shell the far side hands an ssh command to.
///
/// ssh does not run an argv: it joins whatever it was given with single spaces and hands the result
/// to the login shell on the far side, which then does its own word splitting. That is why the
/// previous version of this file was a comment in the config saying the agent argv "must not need
/// quoting": a path with a space in it was simply broken, and a service id with a semicolon in it
/// was worse than broken.
///
/// So the command is serialised here, once, for the shell that will actually parse it.
enum RemoteShell: String, Codable, Sendable, Equatable, CaseIterable {
    /// sh, bash, zsh, fish and everything else on Linux and macOS.
    case posix
    /// Windows OpenSSH with `DefaultShell` left alone, which is `cmd.exe`.
    case cmd
    /// Windows OpenSSH with `DefaultShell` set to PowerShell, which is what the Windows installer
    /// recommends and what a machine set up for remote administration usually has.
    case powershell

    /// What a system gets when its config does not name one.
    ///
    /// Windows defaults to PowerShell because that is what a machine set up for remote
    /// administration has, and because the failure is legible when the guess is wrong: PowerShell
    /// quoting handed to cmd.exe fails loudly with "& was unexpected at this time", whereas cmd
    /// quoting handed to PowerShell exits zero having printed the command back as a string, which
    /// this app would report as unreadable output and nobody could diagnose. A machine still on the
    /// stock `DefaultShell` says `"shell": "cmd"`; this app never changes a machine's setting for it.
    static func `default`(for platform: Platform) -> RemoteShell {
        switch platform {
        case .linux, .mac: .posix
        case .windows: .powershell
        }
    }

    /// One argv, quoted so the far side splits it back into exactly the same words.
    func serialize(_ argv: [String]) -> String {
        switch self {
        case .posix:
            return argv.map(Self.posixQuoted).joined(separator: " ")
        case .cmd:
            return argv.map(Self.cmdQuoted).joined(separator: " ")
        case .powershell:
            // The call operator is needed the moment the command itself is quoted: PowerShell reads
            // a bare quoted string as a string to print, not as a program to run.
            let parts = argv.map(Self.powerShellQuoted)
            guard let first = parts.first else { return "" }
            return (["&", first] + parts.dropFirst()).joined(separator: " ")
        }
    }

    // MARK: - Per shell quoting

    /// Single quotes, with the one escape a POSIX shell has: end the quoting, emit a literal quote,
    /// start it again. Everything else inside single quotes is already literal, including newlines.
    static func posixQuoted(_ value: String) -> String {
        if value.isEmpty { return "''" }
        // Nothing to do for the overwhelmingly common case, and leaving it alone keeps the command
        // line readable in a log.
        if value.allSatisfy(isPosixSafe) { return value }
        return "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// PowerShell single quotes: literal throughout, and a quote of its own is doubled.
    static func powerShellQuoted(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "''") + "'"
    }

    /// cmd.exe, which has two layers: the C runtime's argv parsing inside the double quotes, and
    /// cmd's own metacharacter handling outside them.
    ///
    /// The value is wrapped in double quotes so spaces survive the runtime's split, backslashes
    /// before the closing quote are doubled so the quote is not escaped by accident, and cmd's own
    /// metacharacters are caret-escaped so they are not acted on before the runtime ever sees them.
    static func cmdQuoted(_ value: String) -> String {
        var quoted = "\""
        var backslashes = 0
        for character in value {
            switch character {
            case "\\":
                backslashes += 1
                quoted.append(character)
            case "\"":
                // Every backslash run immediately before a quote is doubled, then the quote itself
                // is escaped. This is the documented rule the Windows C runtime parses by.
                quoted.append(String(repeating: "\\", count: backslashes + 1))
                quoted.append("\"")
                backslashes = 0
            default:
                backslashes = 0
                quoted.append(character)
            }
        }
        quoted.append(String(repeating: "\\", count: backslashes))
        quoted.append("\"")

        // cmd looks at the whole line before the runtime does, and the two disagree about what a
        // quote means. The recipe that survives both is to caret-escape every character cmd acts on
        // INCLUDING the double quotes: cmd then never enters quoting mode, passes each escaped
        // character through literally, and the runtime re-parses the result with the quotes back in
        // place. Leaving the quotes unescaped is the subtle version of this that gets a `&` inside a
        // quoted path executed.
        var escaped = ""
        for character in quoted {
            if "^&|<>()!%\"".contains(character) { escaped.append("^") }
            escaped.append(character)
        }
        return escaped
    }

    private static func isPosixSafe(_ character: Character) -> Bool {
        character.isLetter && character.isASCII
            || character.isNumber && character.isASCII
            || "._-/:=@+,".contains(character)
    }
}

/// One command to run on one system: the agent argv from the config, then the verb and its flags.
///
/// Kept apart from the transport so the exact bytes that go over ssh can be asserted in a test
/// rather than inferred from a machine that may or may not be awake.
struct RemoteCommand: Sendable, Equatable {
    var system: SystemConfig
    var arguments: [String]

    /// The whole argv, agent first.
    var argv: [String] { system.agent + arguments }

    /// The single string ssh is given, quoted for the shell the system says it has.
    var commandLine: String { system.remoteShell.serialize(argv) }

    /// Everything after `ssh`: the options, the destination, and the one quoted command.
    ///
    /// One argument, not many. ssh would join several with spaces and hand the join to the remote
    /// shell anyway, so building the string here is the only way to control what that shell sees.
    static func sshArguments(
        target: SSHTarget,
        connectTimeout: Int,
        command: RemoteCommand,
        /// Set only when this run has been relocated by `LEGION_CONTROL_HOME`. An ordinary run says
        /// nothing about known_hosts, so the user's own file and their own `~/.ssh/config` keep
        /// working exactly as before; an isolated run gets a file of its own and never touches
        /// theirs.
        knownHostsFile: String? = nil
    ) -> [String] {
        // BatchMode refuses every prompt, which is what turns an unknown or changed host key into a
        // failure this app can name rather than a process sitting on a question nobody can see.
        // Accepting a key on the user's behalf is deliberately not done here: the first connection
        // to a machine is a trust decision, and it belongs in a terminal where a fingerprint can be
        // read.
        ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no",
         "-o", "ConnectTimeout=\(connectTimeout)"]
            + (knownHostsFile.map { ["-F", "/dev/null", "-o", "UserKnownHostsFile=\($0)", "-o", "GlobalKnownHostsFile=/dev/null"] } ?? [])
            + target.sshOptions
            + ["--", target.destination, command.commandLine]
    }
}
