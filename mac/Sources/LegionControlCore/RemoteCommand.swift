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

/// The grammar a system's command lines are written in.
///
/// ssh does not run an argv: it joins whatever it was given with single spaces and hands the result
/// to whatever runs commands on the far side, which then does its own word splitting. That is why the
/// first version of this file was a comment in the config saying the agent argv "must not need
/// quoting": a path with a space in it was simply broken, and a service id with a semicolon in it
/// was worse than broken.
///
/// So every argv is serialised here, once, for the grammar that will actually parse it, exactly as
/// section 15 of the contract says and `contract/tools/transport.mjs` does. The shared transport
/// vectors hold this file to that reference case by case. Before them the Mac quoted a Windows
/// machine for PowerShell, the desktop for cmd with carets, and the phone left a backslash bare, so
/// one document ran three different commands on the same machine.
enum CommandGrammar: String, Sendable, Equatable, CaseIterable {
    /// sh, dash, bash, zsh, ksh and fish. Not csh or tcsh.
    case posix
    /// The agent's forced-command dispatcher on a restricted key. It is not a shell: it parses the
    /// line itself, and refuses anything outside its own small grammar.
    case dispatch
    /// cmd.exe, which is what a stock Windows OpenSSH runs.
    case cmd
    /// Windows PowerShell or pwsh, on an account whose OpenSSH `DefaultShell` points at it.
    case powershell

    /// The grammar for a system (section 15.1), or why it has none.
    ///
    /// A system restricted on the route always gets the dispatcher's grammar, whatever its `shell`
    /// says, because the forced command never reaches a shell. Otherwise the configured shell wins,
    /// and with none the platform's stock OpenSSH decides: cmd on Windows, a POSIX shell everywhere
    /// else. PowerShell is never guessed.
    ///
    /// `shell` is the document's raw value. Only the exact spellings `posix`, `cmd` and `powershell`
    /// name a grammar; anything else is refused rather than read as the nearest one.
    static func resolve(platform: Platform?, shell: String?, restricted: Bool?, restrictedSession: Bool,
                        agentLength: Int) -> Result<CommandGrammar, CommandLineRefusal> {
        if restricted == true || restrictedSession {
            // The dispatcher takes exactly an interpreter and an entry in front of the command.
            return agentLength == 2 ? .success(.dispatch) : .failure(CommandLineRefusal("restricted-agent-shape"))
        }
        guard let shell else { return .success(platform == .windows ? .cmd : .posix) }
        switch shell {
        case "posix": return .success(.posix)
        case "cmd": return .success(.cmd)
        case "powershell": return .success(.powershell)
        default: return .failure(CommandLineRefusal("unknown-shell"))
        }
    }

    /// The one line an argv becomes in this grammar (section 15.2).
    ///
    /// Elements are checked in order and the first that fails refuses the whole line, naming its
    /// index: nothing is ever sent with an element dropped or "fixed". posix refuses the backslash
    /// sequences fish reads as escapes even inside single quotes; dispatch instead holds the line to
    /// the dispatcher's own limits, so a line that leaves here is always one the dispatcher accepts.
    /// cmd quotes every element and refuses what cmd expands even inside quotes (`%`, `!`) or cannot
    /// carry at all (`"`); the `@` in front makes the line survive whether sshd wrapped it in quotes
    /// or not. powershell refuses what its native argument passing mangles: an embedded `"`, a
    /// trailing backslash, two spaces in a row.
    func serialize(_ argv: [String]) throws(CommandLineRefusal) -> String {
        guard !argv.isEmpty else { throw CommandLineRefusal("no-command") }
        if self == .dispatch, argv.count > 64 { throw CommandLineRefusal("too-many-arguments") }
        var tokens: [String] = []
        for (index, value) in argv.enumerated() {
            let scalars = value.unicodeScalars
            func refusal(_ code: String) -> CommandLineRefusal { CommandLineRefusal(code, index: index) }
            if scalars.isEmpty { throw refusal("empty-argument") }
            if scalars.contains(where: { $0.value <= 0x1F || $0.value == 0x7F }) { throw refusal("control-character") }
            switch self {
            case .posix, .dispatch:
                if self == .posix, Self.hasFishEscape(scalars) { throw refusal("posix-backslash") }
                if self == .dispatch, value.utf8.count > 4096 { throw refusal("argument-too-long") }
                tokens.append(Self.posixToken(value, first: index == 0))
            case .cmd:
                if scalars.contains("\"") { throw refusal("double-quote") }
                if scalars.contains("%") { throw refusal("percent") }
                if scalars.contains("!") { throw refusal("exclamation") }
                // The C runtime reads a run of backslashes in front of a quote as escapes, so the
                // run at the end is doubled and the closing quote stays a quote. A backslash
                // anywhere else stands for itself.
                let trailing = scalars.reversed().prefix { $0 == "\\" }.count
                tokens.append("\"" + value + String(repeating: "\\", count: trailing) + "\"")
            case .powershell:
                if scalars.contains("\"") { throw refusal("double-quote") }
                if scalars.last == "\\" { throw refusal("trailing-backslash") }
                if zip(scalars, scalars.dropFirst()).contains(where: { $0 == " " && $1 == " " }) {
                    throw refusal("double-space")
                }
                // PowerShell's tokenizer reads U+2018 to U+201B as single quotes too. Doubling only
                // U+0027 left a typographic quote free to end the string early.
                var token = String.UnicodeScalarView()
                token.append("'")
                for scalar in scalars {
                    if Self.powerShellQuotes.contains(scalar) { token.append(scalar) }
                    token.append(scalar)
                }
                token.append("'")
                tokens.append(String(token))
            }
        }
        let body = tokens.joined(separator: " ")
        if self == .dispatch, body.utf8.count > 16384 { throw CommandLineRefusal("line-too-long") }
        switch self {
        case .cmd: return "@" + body
        case .powershell: return "& " + body
        case .posix, .dispatch: return body
        }
    }

    // MARK: - Pieces

    private static let powerShellQuotes: Set<Unicode.Scalar> = ["\u{27}", "\u{2018}", "\u{2019}", "\u{201A}", "\u{201B}"]

    /// A word a POSIX shell and the dispatcher both read as itself, or the same word in single
    /// quotes with the one escape a POSIX shell has: end the quoting, emit a literal quote, start it
    /// again.
    ///
    /// `~` is never bare: argv items are literal, and a bare tilde would be expanded by every shell
    /// but the dispatcher, so the same document would run a different file depending on which of the
    /// two received it. A word may not start with `=`, which zsh expands, and the first word may not
    /// hold one at all, which sh reads as a variable assignment.
    static func posixToken(_ value: String, first: Bool = false) -> String {
        let scalars = value.unicodeScalars
        if let head = scalars.first, isBareStart(head),
           scalars.dropFirst().allSatisfy({ isBareStart($0) || $0 == "=" }),
           !(first && scalars.contains("=")) {
            return value
        }
        var quoted = String.UnicodeScalarView()
        quoted.append("'")
        for scalar in scalars {
            if scalar == "'" { quoted.append(contentsOf: "'\\''".unicodeScalars) } else { quoted.append(scalar) }
        }
        quoted.append("'")
        return String(quoted)
    }

    /// One word for a script this app runs itself with `/bin/sh`. The same token as a command
    /// line's, with nothing refused, since no remote shell or fish is involved.
    static func posixQuoted(_ value: String) -> String {
        value.isEmpty ? "''" : posixToken(value)
    }

    private static func isBareStart(_ scalar: Unicode.Scalar) -> Bool {
        switch scalar {
        case "A"..."Z", "a"..."z", "0"..."9", ".", "_", "/", ":", "@", "+", ",", "-": true
        default: false
        }
    }

    /// A backslash followed by another backslash, by `'`, or by the end of the word: what fish
    /// treats as an escape inside single quotes, where every other shell reads it literally.
    private static func hasFishEscape(_ scalars: String.UnicodeScalarView) -> Bool {
        var index = scalars.startIndex
        while index != scalars.endIndex {
            let next = scalars.index(after: index)
            if scalars[index] == "\\", next == scalars.endIndex || scalars[next] == "\\" || scalars[next] == "'" {
                return true
            }
            index = next
        }
        return false
    }
}

/// Why an argv has no command line (section 15.2), or a system no grammar (15.1).
///
/// The code is the contract's, so the vectors can compare it. `index` is the element that failed,
/// counted from zero, or nil when the refusal is about the whole argv or the whole line. A refused
/// line is never sent anywhere, which makes it the one failure that is certain nothing ran.
struct CommandLineRefusal: Error, Sendable, Equatable {
    var code: String
    var index: Int?

    init(_ code: String, index: Int? = nil) {
        self.code = code
        self.index = index
    }

    /// What it means for the person whose command it was.
    var message: String {
        let element = index.map { "Argument \($0 + 1) of the command" } ?? "The command"
        switch code {
        case "no-command":
            return "There is no command to run: the system's agent command is empty."
        case "empty-argument":
            return "\(element) is empty, and an empty argument does not survive every shell."
        case "control-character":
            return "\(element) contains a control character such as a line break."
        case "posix-backslash":
            return "\(element) has a backslash that fish would read as an escape."
        case "double-quote":
            return "\(element) contains a double quote, which this system's shell cannot be handed safely."
        case "percent":
            return "\(element) contains %, which cmd expands even inside quotes."
        case "exclamation":
            return "\(element) contains !, which cmd can expand even inside quotes."
        case "trailing-backslash":
            return "\(element) ends in a backslash, which PowerShell passes on mangled."
        case "double-space":
            return "\(element) contains two spaces in a row, which PowerShell passes on as one."
        case "too-many-arguments":
            return "The command has more than 64 arguments, more than the restricted dispatcher takes."
        case "argument-too-long":
            return "\(element) is longer than the 4096 bytes the restricted dispatcher takes."
        case "line-too-long":
            return "The command line is longer than the 16384 bytes the restricted dispatcher takes."
        case "restricted-agent-shape":
            return "A restricted key runs through the dispatcher, which needs the agent command to be exactly the interpreter and the agent's entry."
        case "unknown-shell":
            return "The system names a shell this app does not know. Use posix, cmd or powershell."
        default:
            return "The command line was refused (\(code))."
        }
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
