import Foundation
import Testing
@testable import LegionControlCore

/// Turning an argv into the one string ssh hands to whatever runs commands on the far side.
///
/// ssh does not run an argv: it joins what it is given with spaces and lets the far side split it
/// again. The old code sent the words unquoted and the config carried a comment saying the agent
/// path "must not need quoting", which meant a path with a space in it was simply broken. The shared
/// transport vectors pin every grammar case by case; these are the round trips and the decisions
/// that sit around them.
struct ShellQuotingTests {

    static func system(_ platform: Platform, agent: [String], shell: String? = nil,
                       restricted: Bool? = nil) -> SystemConfig {
        SystemConfig(id: "sys", platform: platform, agent: agent, shell: shell, restricted: restricted)
    }

    // MARK: - POSIX

    @Test("a POSIX path with spaces survives the round trip through sh")
    func posixSpaces() async throws {
        let command = RemoteCommand(
            system: Self.system(.linux, agent: ["/usr/bin/node", "/home/me/legion control/agent/src/index.mjs"]),
            arguments: ["update", "--service", "t3"]
        )
        // Not a claim about the string: sh itself is asked to split it, and the words that come out
        // are compared with the words that went in.
        let script = "for word in \(try command.commandLine()); do printf '%s\\n' \"$word\"; done"
        let run = await Shell.run(executable: "/bin/sh", arguments: ["-c", script], timeout: 15)
        let words = run.standardOutput.split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init).filter { !$0.isEmpty }
        #expect(words == command.argv)
    }

    @Test("quotes, dollars, tildes and semicolons inside a POSIX argument stay literal")
    func posixMetacharacters() async throws {
        let nasty = ["/usr/bin/node", "/tmp/a'b\"c$(whoami);rm -rf /", "arg with space", "~/agent.mjs"]
        let command = RemoteCommand(system: Self.system(.linux, agent: [nasty[0]]),
                                    arguments: Array(nasty.dropFirst()))
        let script = "for word in \(try command.commandLine()); do printf '%s\\n' \"$word\"; done"
        let run = await Shell.run(executable: "/bin/sh", arguments: ["-c", script], timeout: 15)
        let words = run.standardOutput.split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init).filter { !$0.isEmpty }
        #expect(words == nasty)
        // Nothing was executed or expanded on the way: the command substitution is one of the words,
        // and the tilde is still a tilde.
        #expect(words.contains { $0.contains("$(whoami)") })
        #expect(words.last == "~/agent.mjs")
    }

    @Test("an ordinary word is left alone, so a command line stays readable in a log")
    func posixLeavesSimpleWordsAlone() {
        #expect(CommandGrammar.posixQuoted("update") == "update")
        #expect(CommandGrammar.posixQuoted("/usr/bin/node") == "/usr/bin/node")
        #expect(CommandGrammar.posixQuoted("") == "''")
        #expect(CommandGrammar.posixQuoted("a b") == "'a b'")
        #expect(CommandGrammar.posixQuoted("it's") == "'it'\\''s'")
        #expect(CommandGrammar.posixQuoted("~/x") == "'~/x'")
    }

    // MARK: - Windows

    @Test("PowerShell arguments are single quoted with the call operator in front")
    func powerShell() throws {
        let command = RemoteCommand(
            system: Self.system(.windows, agent: ["node", "C:\\Users\\me\\legion control\\index.mjs"], shell: "powershell"),
            arguments: ["status"]
        )
        #expect(try command.commandLine() == "& 'node' 'C:\\Users\\me\\legion control\\index.mjs' 'status'")
    }

    @Test("every quote PowerShell honours is doubled, typographic ones included")
    func powerShellQuotes() throws {
        #expect(try CommandGrammar.powershell.serialize(["node", "it's"]) == "& 'node' 'it''s'")
        // U+2018 to U+201B end a PowerShell string as surely as U+0027 does.
        #expect(try CommandGrammar.powershell.serialize(["node", "\u{2018}a\u{2019}\u{201A}b\u{201B}"])
                == "& 'node' '\u{2018}\u{2018}a\u{2019}\u{2019}\u{201A}\u{201A}b\u{201B}\u{201B}'")
    }

    @Test("cmd arguments are double quoted behind an @, with no carets")
    func cmd() throws {
        let command = RemoteCommand(
            system: Self.system(.windows, agent: ["node", "C:\\Users\\me\\index.mjs"], shell: "cmd"),
            arguments: ["run", "sunshine"]
        )
        #expect(try command.commandLine() == "@\"node\" \"C:\\Users\\me\\index.mjs\" \"run\" \"sunshine\"")
        // Inside the quotes cmd's operators are literal, and a caret there would reach the program.
        #expect(try CommandGrammar.cmd.serialize(["node", "a&b"]) == "@\"node\" \"a&b\"")
        #expect(throws: CommandLineRefusal("percent", index: 1)) { try CommandGrammar.cmd.serialize(["node", "%PATH%"]) }
    }

    @Test("a Windows system with no shell is written for cmd, never guessed as PowerShell")
    func windowsDefault() {
        // The stock Windows OpenSSH runs cmd.exe, and the other clients have always assumed so. A
        // PowerShell login says so in the document.
        #expect(Self.system(.windows, agent: ["node", "x"]).grammar == .success(.cmd))
        #expect(Self.system(.linux, agent: ["node", "x"]).grammar == .success(.posix))
        #expect(Self.system(.mac, agent: ["node", "x"]).grammar == .success(.posix))
        #expect(Self.system(.windows, agent: ["node", "x"], shell: "powershell").grammar == .success(.powershell))
        #expect(Self.system(.windows, agent: ["node", "x"], shell: "pwsh").grammar == .failure(CommandLineRefusal("unknown-shell")))
    }

    @Test("a document with a shell nobody knows is refused, and a Windows system without one decodes")
    func shellDecoding() throws {
        func document(_ shell: String) -> Data {
            Data("""
            { "version": 1, "machines": [ { "id": "tower", "endpoints": [ { "host": "192.0.2.10" } ],
              "systems": [ { "id": "windows", "platform": "windows", \(shell) "agent": ["node", "C:\\\\a.mjs"] } ] } ] }
            """.utf8)
        }
        let defaulted = try ControllerConfig.accepted(document(""))
        #expect(defaulted.machines[0].systems[0].grammar == .success(.cmd))
        // Decoding no longer fails on it, so the setup can be read and repaired; using it still fails.
        let unknown = try JSONDecoder().decode(ControllerConfig.self, from: document(#""shell": "pwsh","#))
        #expect(unknown.machines[0].systems[0].shell == "pwsh")
        #expect(DocumentAcceptance.check(document(#""shell": "pwsh","#), intake: .peer).errors.map(\.code) == ["schema"])
    }

    @Test("a restricted key gets the dispatcher's grammar whatever platform it is on")
    func restrictedIsDispatch() throws {
        // A forced command never reaches a shell: sshd hands the request to the dispatcher, which
        // parses it itself. A Windows path in it is quoted, because the dispatcher refuses a bare
        // backslash.
        let windows = Self.system(.windows, agent: ["node", "C:\\a b\\index.mjs"], shell: "powershell",
                                  restricted: true)
        #expect(windows.grammar == .success(.dispatch))
        let command = RemoteCommand(system: windows, arguments: ["status"])
        #expect(try command.commandLine() == "node 'C:\\a b\\index.mjs' status")

        // The dispatcher takes exactly an interpreter and an entry in front of the command.
        let wrongShape = Self.system(.linux, agent: ["/bin/sh", "-c", "legionctl"], restricted: true)
        #expect(throws: CommandLineRefusal("restricted-agent-shape")) {
            try RemoteCommand(system: wrongShape, arguments: ["status"]).commandLine()
        }
    }

    @Test("a route that reported a restricted session moves the line to the dispatcher, and only there")
    func restrictedSessionMovesToDispatch() throws {
        let windows = Self.system(.windows, agent: ["C:\\node.exe", "C:\\Users\\me\\launcher.mjs"], shell: "cmd")
        #expect(try RemoteCommand(system: windows, arguments: ["status"]).commandLine()
                == "@\"C:\\node.exe\" \"C:\\Users\\me\\launcher.mjs\" \"status\"")
        #expect(try RemoteCommand(system: windows, arguments: ["status"], restrictedSession: true).commandLine()
                == "'C:\\node.exe' 'C:\\Users\\me\\launcher.mjs' status")
    }

    // MARK: - The ssh command line

    @Test("the command goes over as exactly one argument, after -- and with ssh's own log")
    func oneArgument() throws {
        let target = SSHTarget(host: "pi", user: "me", port: 2222, identityFile: "~/.ssh/key")
        let command = RemoteCommand(system: Self.system(.linux, agent: ["node", "/a b/index.mjs"]),
                                    arguments: ["status"])
        let line = try command.commandLine()
        let arguments = RemoteCommand.sshArguments(target: target, connectTimeout: 8, command: line,
                                                   logFile: "/tmp/run/ssh-logs/x.log")

        // ssh would join several arguments with spaces and hand the join to the remote shell anyway,
        // so building the string here is the only way to control what that shell sees.
        #expect(arguments.last == line)
        #expect(Array(arguments.suffix(3)) == ["--", "me@pi", line])
        #expect(arguments.first == "-T")
        #expect(arguments.contains("BatchMode=yes"))
        #expect(arguments.contains("ConnectTimeout=8"))
        #expect(arguments.contains("ClearAllForwardings=yes"))
        #expect(arguments.contains("RemoteCommand=none"))
        #expect(arguments.contains("LogLevel=ERROR"))
        let log = try #require(arguments.firstIndex(of: "-E"))
        #expect(arguments[log + 1] == "/tmp/run/ssh-logs/x.log")
        #expect(arguments.contains("-p"))
        #expect(arguments.contains("2222"))
        // Nothing accepts a host key on the user's behalf.
        #expect(arguments.contains("StrictHostKeyChecking=yes"))
        #expect(arguments.contains("UpdateHostKeys=no"))
    }

    @Test("an isolated run is told which known_hosts to use, and an ordinary one is not")
    func knownHostsIsolation() {
        let target = SSHTarget(host: "pi", user: nil, port: nil, identityFile: nil)

        let ordinary = RemoteCommand.sshArguments(target: target, connectTimeout: 8, command: "node /a.mjs status")
        #expect(!ordinary.contains { $0.hasPrefix("UserKnownHostsFile") })

        let isolated = RemoteCommand.sshArguments(target: target, connectTimeout: 8, command: "node /a.mjs status",
                                                  knownHostsFile: "/tmp/run/ssh/known_hosts")
        #expect(isolated.contains("UserKnownHostsFile=/tmp/run/ssh/known_hosts"))
    }

    @Test("ssh's log is a private file, read back once and deleted")
    func sshLogFile() throws {
        let directory = FileManager.default.temporaryDirectory.appending(path: "ssh-logs-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let file = try #require(SSHLog.create(in: directory))
        let mode = try FileManager.default.attributesOfItem(atPath: file.path)[.posixPermissions] as? Int
        let folderMode = try FileManager.default.attributesOfItem(atPath: directory.path)[.posixPermissions] as? Int
        #expect(mode == 0o600)
        #expect(folderMode == 0o700)
        try Data(repeating: UInt8(ascii: "a"), count: SSHLog.readLimit + 10).write(to: file)
        #expect(SSHLog.consume(file).utf8.count == SSHLog.readLimit)
        #expect(!FileManager.default.fileExists(atPath: file.path))
    }

    @Test("logs a crashed run left behind are swept when the next run makes its first one")
    func staleSSHLogsAreSwept() throws {
        let manager = FileManager.default
        let directory = manager.temporaryDirectory.appending(path: "ssh-logs-\(UUID().uuidString)")
        defer { try? manager.removeItem(at: directory) }
        try manager.createDirectory(at: directory, withIntermediateDirectories: true)
        func file(_ name: String, age: TimeInterval) throws -> URL {
            let url = directory.appending(path: name)
            try Data("ssh: connect to host x port 22: Connection refused\n".utf8).write(to: url)
            try manager.setAttributes([.modificationDate: Date().addingTimeInterval(-age)], ofItemAtPath: url.path)
            return url
        }
        let left = try file("left-behind.log", age: SSHLog.staleAfter + 60)
        let writing = try file("still-writing.log", age: 60)
        let other = try file("notes.txt", age: SSHLog.staleAfter + 60)

        let first = try #require(SSHLog.create(in: directory))
        #expect(!manager.fileExists(atPath: left.path))
        #expect(manager.fileExists(atPath: writing.path))
        #expect(manager.fileExists(atPath: other.path))
        #expect(manager.fileExists(atPath: first.path))

        // Once a run: the attempts after the first do not list the folder again.
        let later = try file("later.log", age: SSHLog.staleAfter + 60)
        _ = try #require(SSHLog.create(in: directory))
        #expect(manager.fileExists(atPath: later.path))
    }
}
