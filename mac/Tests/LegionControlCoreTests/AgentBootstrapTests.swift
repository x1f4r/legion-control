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
