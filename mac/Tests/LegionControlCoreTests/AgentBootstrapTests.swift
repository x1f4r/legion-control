import Foundation
import Testing
@testable import LegionControlCore

struct AgentBootstrapTests {
    @Test("legacy base inference accepts only recognized installed layouts")
    func layoutInference() throws {
        #expect(try AgentBootstrap.layout(argv: ["/opt/node", "/home/My Name/base/agent/src/index.mjs"]).base == "/home/My Name/base")
        let windows = try AgentBootstrap.layout(argv: ["C:\\Node\\node.exe", "C:\\Users\\My Name\\base\\bin\\launcher.mjs"])
        #expect(windows.base == "C:\\Users\\My Name\\base")
        #expect(windows.stableArgv.last == "C:\\Users\\My Name\\base\\bin\\launcher.mjs")
        #expect(throws: AgentBootstrap.Failure.self) { try AgentBootstrap.layout(argv: ["node", "/custom/control.mjs"]) }
        #expect(try AgentBootstrap.layout(argv: ["node", "/custom/control.mjs"], explicitBase: "/home/me/base").base == "/home/me/base")
        #expect(!AgentBootstrap.isAbsolute("~/base"))
        #expect(!AgentBootstrap.isAbsolute("/home/me/../other"))
        #expect(try AgentBootstrap.sftpQuoted("/home/My Name/archive.tgz") == "\"/home/My Name/archive.tgz\"")
    }

    @Test("only an authenticated actual legacy status permits bootstrap")
    func noMissingAgentFallback() throws {
        func status(_ json: String) throws -> AgentStatus { try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8)) }
        #expect(AgentBootstrap.isAuthenticatedLegacy(try status(#"{"ok":true,"agentVersion":"2.1.0"}"#)))
        #expect(!AgentBootstrap.isAuthenticatedLegacy(try status(#"{"ok":false,"agentVersion":"2.1.0"}"#)))
        #expect(!AgentBootstrap.isAuthenticatedLegacy(try status(#"{"ok":true,"agentVersion":"3.0.0","contract":3}"#)))
        #expect(!AgentBootstrap.isAuthenticatedLegacy(try status("{}")))
    }

    @Test("the upgrade over ssh is offered only where its scripts can be written, and says why not")
    func bootstrapGrammarGate() throws {
        // Both scripts contain !, which cmd can expand even inside quotes, so a cmd login, now the
        // Windows default, cannot be sent them. Saying so up front beats a refusal after the user
        // has confirmed and the archive has been staged.
        func system(_ platform: Platform, shell: String?, restricted: Bool? = nil) -> SystemConfig {
            SystemConfig(id: "s", name: "Tower", platform: platform,
                         agent: platform == .windows ? ["C:\\node.exe", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"]
                                                     : ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"],
                         shell: shell, restricted: restricted)
        }
        for allowed in [system(.linux, shell: nil), system(.mac, shell: "posix"), system(.windows, shell: "powershell")] {
            #expect(AgentBootstrap.remoteRefusal(system: allowed, restrictedSession: false) == nil, "\(allowed.grammar)")
            // And what the gate lets through really can be written in that grammar.
            let layout = try AgentBootstrap.layout(argv: allowed.agent)
            for script in [AgentBootstrap.prepareScript, AgentBootstrap.installScript] {
                var carrier = allowed
                carrier.agent = layout.prefix + ["-e", AgentBootstrap.oneLine(script), "--", layout.base]
                #expect(throws: Never.self) { try RemoteCommand(system: carrier, arguments: []).commandLine() }
            }
        }
        let cmd = try #require(AgentBootstrap.remoteRefusal(system: system(.windows, shell: nil), restrictedSession: false))
        #expect(cmd.contains("cmd"))
        #expect(AgentBootstrap.remoteRefusal(system: system(.windows, shell: "cmd"), restrictedSession: false) == cmd)
        #expect(AgentBootstrap.remoteRefusal(system: system(.windows, shell: "pwsh"), restrictedSession: false) != nil)
        #expect(AgentBootstrap.remoteRefusal(system: system(.linux, shell: nil, restricted: true), restrictedSession: false) != nil)
        #expect(AgentBootstrap.remoteRefusal(system: system(.linux, shell: nil), restrictedSession: true) != nil)
    }

    func fixture() async throws -> (URL, URL, Data) {
        let dir = FileManager.default.temporaryDirectory.appending(path: "bootstrap-\(UUID().uuidString)")
        let source = dir.appending(path: "source/agent/src")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: true)
        let entry = """
        import fs from 'node:fs'; import path from 'node:path';
        fs.writeFileSync(path.join(process.env.LEGIONCTL_HOME,'proof.json'),JSON.stringify({base:process.env.LEGIONCTL_HOME,args:process.argv.slice(2)}));
        console.log(JSON.stringify({ok:true,contract:3,agentVersion:'3.0.0',action:'installed'}));
        """
        try Data(entry.utf8).write(to: source.appending(path: "index.mjs"))
        for file in ["MANIFEST.json", "MANIFEST.json.sig"] { try Data("fixture".utf8).write(to: source.deletingLastPathComponent().appending(path: file)) }
        let archive = dir.appending(path: "agent.tgz")
        let result = await Shell.run(executable: "/usr/bin/tar", arguments: ["-czf", archive.path, "-C", dir.appending(path: "source").path, "agent"], timeout: 10)
        #expect(result.succeeded)
        return (dir, source, try Data(contentsOf: archive))
    }

    @Test("bootstrap runs the staged entry with the chosen base and immutable operation id")
    func stagedExecution() async throws {
        let (dir, _, archive) = try await fixture()
        defer { try? FileManager.default.removeItem(at: dir) }
        let base = dir.appending(path: "installation with spaces")
        let old = base.appending(path: "agent/src")
        try FileManager.default.createDirectory(at: old, withIntermediateDirectories: true)
        try Data("// legacy fixture".utf8).write(to: old.appending(path: "index.mjs"))
        let node = try #require(["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"].first { FileManager.default.isExecutableFile(atPath: $0) })
        let layout = try AgentBootstrap.layout(argv: [node, old.appending(path: "index.mjs").path])
        let operation = AgentToken.newOperationId()
        let reply = try await AgentBootstrap.installLocal(payload: archive, operation: operation, layout: layout)
        #expect(reply.action == "installed")
        let proof = try #require(try JSONSerialization.jsonObject(with: Data(contentsOf: base.appending(path: "proof.json"))) as? [String: Any])
        #expect(proof["base"] as? String == base.path)
        #expect(proof["args"] as? [String] == ["self-update", "--install", "--op", operation])
        #expect(try String(contentsOf: old.appending(path: "index.mjs"), encoding: .utf8) == "// legacy fixture")
    }

    @Test("the bootstrap scripts go over ssh on one line and still run")
    func scriptsSurviveTheCommandLine() async throws {
        // A command line may not carry a line feed in any grammar, so the scripts are joined onto one
        // line on the way out. This runs that line through sh with a real node, as a POSIX login
        // would, and checks node still reads both scripts.
        let base = FileManager.default.temporaryDirectory.appending(path: "bootstrap-line-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: base) }
        let entry = base.appending(path: "agent/src")
        try FileManager.default.createDirectory(at: entry, withIntermediateDirectories: true)
        try Data("// legacy fixture".utf8).write(to: entry.appending(path: "index.mjs"))
        let node = try #require(["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"].first { FileManager.default.isExecutableFile(atPath: $0) })
        let layout = try AgentBootstrap.layout(argv: [node, entry.appending(path: "index.mjs").path])

        func line(_ argv: [String]) throws -> String {
            try RemoteCommand(system: SystemConfig(id: "linux", agent: argv), arguments: []).commandLine()
        }
        let prepare = try line(layout.prefix + ["-e", AgentBootstrap.oneLine(AgentBootstrap.prepareScript), "--", layout.base])
        #expect(!prepare.contains("\n"))
        let prepared = await Shell.run(executable: "/bin/sh", arguments: ["-c", prepare], timeout: 30)
        #expect(prepared.standardOutput == "{\"ok\":true}\n", "\(prepared.standardError)")

        // The install script refuses an archive that is not there, which it can only do once node
        // has parsed all of it.
        let install = try line(layout.prefix + ["-e", AgentBootstrap.oneLine(AgentBootstrap.installScript), "--", layout.base,
                                                base.appending(path: "missing.tgz").path, String(repeating: "0", count: 64), "1",
                                                AgentToken.newOperationId()])
        let installed = await Shell.run(executable: "/bin/sh", arguments: ["-c", install], timeout: 30)
        #expect(installed.exitCode != 0)
        #expect(installed.standardError.contains("ENOENT"))
        #expect(!installed.standardError.contains("SyntaxError"))
    }

    @Test("the bootstrap reads ssh's own log, and is certain only when nothing can have run")
    func bootstrapUsesTheClassifier() async throws {
        let logs = FakeSSH.logDirectory()
        defer { try? FileManager.default.removeItem(at: logs) }
        let target = SSHTarget(host: "192.0.2.10", user: "me", port: nil, identityFile: nil)
        let system = SystemConfig(id: "linux", agent: ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"])
        let argv = ["/usr/bin/node", "-e", AgentBootstrap.oneLine(AgentBootstrap.prepareScript), "--", "/home/me/.legion-control"]

        let refused = try await AgentBootstrap.runOverSSH(target: target, system: system, argv: argv, timeout: 5,
                                                          logDirectory: logs) { _, arguments, _, _ in
            #expect(Array(arguments.suffix(3).prefix(2)) == ["--", "me@192.0.2.10"])
            FakeSSH.writeLog("ssh: connect to host 192.0.2.10 port 22: Connection refused\n", for: arguments)
            return FakeSSH.result(exit: 255)
        }
        #expect(refused.verdict.dispatch == .never)
        do {
            _ = try AgentBootstrap.decode(refused.result, verdict: refused.verdict)
            Issue.record("a refused connection decoded as a reply")
        } catch let failure as AgentBootstrap.Failure {
            #expect(!failure.uncertain)
        }

        // The same exit with nothing in ssh's log may have run, so the outcome is left open.
        let dropped = try await AgentBootstrap.runOverSSH(target: target, system: system, argv: argv, timeout: 5,
                                                          logDirectory: logs) { _, _, _, _ in FakeSSH.result(exit: 255) }
        #expect(dropped.verdict.dispatch == .unknown)
        do {
            _ = try AgentBootstrap.decode(dropped.result, verdict: dropped.verdict)
            Issue.record("a dropped link decoded as a reply")
        } catch let failure as AgentBootstrap.Failure {
            #expect(failure.uncertain)
        }

        // A system whose shell cannot carry the script is refused before anything is dialled.
        let cmd = SystemConfig(id: "windows", platform: .windows, agent: ["node.exe", "C:\\lc\\agent\\src\\index.mjs"])
        await #expect(throws: AgentBootstrap.Failure.self) {
            _ = try await AgentBootstrap.runOverSSH(target: target, system: cmd, argv: ["node.exe", "-e", AgentBootstrap.oneLine(AgentBootstrap.prepareScript)],
                                                    timeout: 5, logDirectory: logs) { _, _, _, _ in
                Issue.record("ssh was run for a line cmd cannot carry")
                return FakeSSH.result()
            }
        }
    }

    @Test("bootstrap refuses archive symlinks before running any staged code")
    func unsafeArchive() async throws {
        let (dir, source, _) = try await fixture()
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createSymbolicLink(at: source.appending(path: "link"), withDestinationURL: dir.appending(path: "outside"))
        let archive = dir.appending(path: "unsafe.tgz")
        let result = await Shell.run(executable: "/usr/bin/tar", arguments: ["-czf", archive.path, "-C", dir.appending(path: "source").path, "agent"], timeout: 10)
        #expect(result.succeeded)
        await #expect(throws: AgentBootstrap.Failure.self) {
            try await AgentBootstrap.stagedArchive(Data(contentsOf: archive), operation: AgentToken.newOperationId())
        }
    }
}
