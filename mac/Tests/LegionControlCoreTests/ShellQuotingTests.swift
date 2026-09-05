import Foundation
import Testing
@testable import LegionControlCore

/// Turning an argv into the one string ssh hands to a remote shell.
///
/// ssh does not run an argv: it joins what it is given with spaces and lets the far side's shell
/// split it again. The old code sent the words unquoted and the config carried a comment saying the
/// agent path "must not need quoting", which meant a path with a space in it was simply broken.
struct ShellQuotingTests {

    static func system(_ platform: Platform, agent: [String], shell: RemoteShell? = nil,
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
        let script = "for word in \(command.commandLine); do printf '%s\\n' \"$word\"; done"
        let run = await Shell.run(executable: "/bin/sh", arguments: ["-c", script], timeout: 15)
        let words = run.standardOutput.split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init).filter { !$0.isEmpty }
        #expect(words == command.argv)
    }

    @Test("quotes, dollars and semicolons inside a POSIX argument stay literal")
    func posixMetacharacters() async throws {
        let nasty = ["/usr/bin/node", "/tmp/a'b\"c$(whoami);rm -rf /", "arg with space"]
        let command = RemoteCommand(system: Self.system(.linux, agent: [nasty[0]]),
                                    arguments: Array(nasty.dropFirst()))
        let script = "for word in \(command.commandLine); do printf '%s\\n' \"$word\"; done"
        let run = await Shell.run(executable: "/bin/sh", arguments: ["-c", script], timeout: 15)
        let words = run.standardOutput.split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init).filter { !$0.isEmpty }
        #expect(words == nasty)
        // Nothing was executed on the way: the command substitution is one of the words.
        #expect(words.contains { $0.contains("$(whoami)") })
    }

    @Test("an ordinary word is left alone, so a command line stays readable in a log")
    func posixLeavesSimpleWordsAlone() {
        #expect(RemoteShell.posixQuoted("update") == "update")
        #expect(RemoteShell.posixQuoted("/usr/bin/node") == "/usr/bin/node")
        #expect(RemoteShell.posixQuoted("") == "''")
        #expect(RemoteShell.posixQuoted("a b") == "'a b'")
        #expect(RemoteShell.posixQuoted("it's") == "'it'\\''s'")
    }

    // MARK: - Windows

    @Test("PowerShell arguments are single quoted with the call operator in front")
    func powerShell() {
        let command = RemoteCommand(
            system: Self.system(.windows, agent: ["node", "C:\\Users\\me\\legion control\\index.mjs"]),
            arguments: ["status"]
        )
        #expect(command.commandLine == "& 'node' 'C:\\Users\\me\\legion control\\index.mjs' 'status'")
    }

    @Test("a quote inside a PowerShell argument is doubled, not escaped")
    func powerShellQuotes() {
        #expect(RemoteShell.powerShellQuoted("it's") == "'it''s'")
        #expect(RemoteShell.powerShellQuoted("C:\\a b\\c") == "'C:\\a b\\c'")
    }

    @Test("cmd arguments are double quoted with the metacharacters escaped")
    func cmd() {
        let command = RemoteCommand(
            system: Self.system(.windows, agent: ["node", "C:\\Users\\me\\index.mjs"], shell: .cmd),
            arguments: ["run", "sunshine"]
        )
        #expect(command.commandLine == "^\"node^\" ^\"C:\\Users\\me\\index.mjs^\" ^\"run^\" ^\"sunshine^\"")
        #expect(RemoteShell.cmdQuoted("a&b").contains("^&"))
    }

    @Test("a Windows system defaults to PowerShell and can be told otherwise")
    func windowsDefault() {
        // The failure is legible when the guess is wrong: PowerShell quoting handed to cmd fails
        // loudly, whereas cmd quoting handed to PowerShell exits zero having printed the command.
        #expect(RemoteShell.default(for: .windows) == .powershell)
        #expect(RemoteShell.default(for: .linux) == .posix)
        #expect(RemoteShell.default(for: .mac) == .posix)
        #expect(Self.system(.windows, agent: ["node"], shell: .cmd).remoteShell == .cmd)
    }

    @Test("a restricted key gets POSIX words whatever platform it is on")
    func restrictedIsAlwaysPosix() {
        // A forced command never reaches a shell: sshd hands the request to the dispatcher, which
        // parses it itself, and the grammar it parses is POSIX.
        let windows = Self.system(.windows, agent: ["node", "C:\\a b\\index.mjs"], shell: .powershell,
                                  restricted: true)
        #expect(windows.remoteShell == .posix)
        let command = RemoteCommand(system: windows, arguments: ["status"])
        #expect(command.commandLine == "node 'C:\\a b\\index.mjs' status")
    }

    // MARK: - The ssh command line

    @Test("the command goes over as exactly one argument")
    func oneArgument() {
        let target = SSHTarget(host: "pi", user: "me", port: 2222, identityFile: "~/.ssh/key")
        let command = RemoteCommand(system: Self.system(.linux, agent: ["node", "/a b/index.mjs"]),
                                    arguments: ["status"])
        let arguments = RemoteCommand.sshArguments(target: target, connectTimeout: 8, command: command)

        // ssh would join several arguments with spaces and hand the join to the remote shell anyway,
        // so building the string here is the only way to control what that shell sees.
        #expect(arguments.last == command.commandLine)
        #expect(arguments.contains("BatchMode=yes"))
        #expect(arguments.contains("ConnectTimeout=8"))
        #expect(arguments.contains("-p"))
        #expect(arguments.contains("2222"))
        #expect(arguments.contains("me@pi"))
        // Nothing accepts a host key on the user's behalf.
        #expect(arguments.contains("StrictHostKeyChecking=yes"))
        #expect(arguments.contains("UpdateHostKeys=no"))
    }

    @Test("an isolated run is told which known_hosts to use, and an ordinary one is not")
    func knownHostsIsolation() {
        let target = SSHTarget(host: "pi", user: nil, port: nil, identityFile: nil)
        let command = RemoteCommand(system: Self.system(.linux, agent: ["node", "/a.mjs"]), arguments: ["status"])

        let ordinary = RemoteCommand.sshArguments(target: target, connectTimeout: 8, command: command)
        #expect(!ordinary.contains { $0.hasPrefix("UserKnownHostsFile") })

        let isolated = RemoteCommand.sshArguments(target: target, connectTimeout: 8, command: command,
                                                  knownHostsFile: "/tmp/run/ssh/known_hosts")
        #expect(isolated.contains("UserKnownHostsFile=/tmp/run/ssh/known_hosts"))
    }
}
