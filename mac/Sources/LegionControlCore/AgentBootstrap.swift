import AppKit
import Foundation

/// Upgrade an authenticated legacy agent using only a locally verified release archive.
enum AgentBootstrap {
    static func isAuthenticatedLegacy(_ status: AgentStatus) -> Bool {
        guard status.ok == true, status.contractVersion < 3, let version = status.version,
              let major = version.split(separator: ".").first.flatMap({ Int($0) }) else { return false }
        return major == 1 || major == 2
    }
    struct Layout: Equatable, Sendable {
        var base: String
        var prefix: [String]
        var separator: String { base.contains("\\") ? "\\" : "/" }
        func path(_ suffix: String) -> String { base + separator + suffix.replacingOccurrences(of: "/", with: separator) }
        var stableArgv: [String] { prefix + [path("bin/launcher.mjs")] }
    }
    struct Failure: Error, LocalizedError {
        var message: String
        var uncertain = false
        var errorDescription: String? { message }
    }
    static func layout(argv: [String], explicitBase: String? = nil) throws -> Layout {
        guard argv.count >= 2 else { throw Failure(message: "Legacy bootstrap needs a configured Node command followed by its agent script.") }
        let script = argv.last!
        let normalized = script.replacingOccurrences(of: "\\", with: "/")
        let suffixes = ["/agent/src/index.mjs", "/bin/launcher.mjs"]
        let inferred = suffixes.first(where: { normalized.hasSuffix($0) }).map { String(script.dropLast($0.count)) }
        guard let base = explicitBase ?? inferred, isAbsolute(base) else {
            throw Failure(message: "Choose the absolute installation base. It cannot be inferred safely from this agent command.")
        }
        return Layout(base: base, prefix: Array(argv.dropLast()))
    }
    static func isAbsolute(_ path: String) -> Bool {
        guard !path.isEmpty, !path.contains("\0"), !path.contains("\n"), !path.contains("\r") else { return false }
        let normalized = path.replacingOccurrences(of: "\\", with: "/")
        guard !normalized.split(separator: "/").contains(".."), !normalized.hasSuffix("/") else { return false }
        return normalized.hasPrefix("/") || normalized.range(of: "^[A-Za-z]:/", options: .regularExpression) != nil
    }

    @MainActor static func chooseLayout(argv: [String]) -> Layout? {
        if let inferred = try? layout(argv: argv) { return inferred }
        let alert = NSAlert()
        alert.messageText = "Choose the agent installation base"
        alert.informativeText = "The configured agent command has no recognized installation layout. Enter the absolute directory that contains the installed agent and its configuration. The current command will be retained until you approve a separate migration."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 420, height: 24))
        field.placeholderString = "/home/me/.legion-control"
        alert.accessoryView = field
        alert.addButton(withTitle: "Use this base")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return nil }
        return try? layout(argv: argv, explicitBase: field.stringValue.trimmingCharacters(in: .whitespaces))
    }

    static func stagedArchive(_ payload: Data, operation: String) async throws -> URL {
        guard payload.count <= 32 * 1024 * 1024, AgentToken.isValidOperationId(operation) else {
            throw Failure(message: "The agent archive or operation id is invalid.")
        }
        let file = AppPaths.file("bootstrap/\(operation).tgz")
        try AtomicFile.write(payload, to: file)
        let listing = await Shell.run(executable: "/usr/bin/tar", arguments: ["-tzf", file.path], timeout: 30)
        let verbose = await Shell.run(executable: "/usr/bin/tar", arguments: ["-tvzf", file.path], timeout: 30)
        guard listing.succeeded, verbose.succeeded else { throw Failure(message: "The signed archive could not be inspected.") }
        var seen = Set<String>()
        for raw in listing.standardOutput.split(separator: "\n") {
            let name = String(raw)
            let normalized = name.hasSuffix("/") ? String(name.dropLast()) : name
            guard normalized == "agent" || normalized.hasPrefix("agent/"),
                  !normalized.contains("\\"), !normalized.contains("//"),
                  !normalized.split(separator: "/").contains(".."),
                  !normalized.split(separator: "/").contains("."),
                  seen.insert(normalized).inserted else { throw Failure(message: "The signed archive contains an unsafe or duplicate path.") }
        }
        guard seen.count <= 10_000 else { throw Failure(message: "The agent archive contains too many entries.") }
        var total: Int64 = 0
        for line in verbose.standardOutput.split(separator: "\n") {
            let fields = line.split(whereSeparator: \.isWhitespace)
            guard fields.count >= 6, let size = Int64(fields[4]), size >= 0,
                  size <= 32 * 1024 * 1024 else { throw Failure(message: "An archive entry has an invalid or excessive size.") }
            total += size
            guard total <= 128 * 1024 * 1024 else { throw Failure(message: "The expanded agent archive is too large.") }
        }
        guard seen.contains("agent/src/index.mjs"), seen.contains("agent/MANIFEST.json"), seen.contains("agent/MANIFEST.json.sig"),
              verbose.standardOutput.split(separator: "\n").allSatisfy({ $0.first == "-" || $0.first == "d" }) else {
            throw Failure(message: "The signed archive contains links, special files or an incomplete agent.")
        }
        return file
    }

    static let prepareScript = """
    const fs=require('node:fs'),path=require('node:path');
    if(Number(process.versions.node.split('.')[0])<24) throw Error('Node 24 or newer is required before this upgrade');
    const base=process.argv[1], incoming=path.join(base,'incoming');
    if(!fs.existsSync(path.join(base,'agent','src','index.mjs'))) throw Error('No installed agent exists in this base; bootstrap refused');
    fs.mkdirSync(incoming,{recursive:true});
    if(fs.existsSync(path.join(incoming,'agent'))) throw Error('An incoming agent tree already exists. Inspect and recover it before trying another installation');
    console.log(JSON.stringify({ok:true}));
    """
    static let installScript = """
    const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),cp=require('node:child_process');
    const [base,archive,hash,size,op]=process.argv.slice(1), incoming=path.join(base,'incoming');
    const stat=fs.statSync(archive); if(stat.size!==Number(size)) throw Error('Uploaded archive size does not match');
    const bytes=fs.readFileSync(archive); if(crypto.createHash('sha256').update(bytes).digest('hex')!==hash) throw Error('Uploaded archive hash does not match');
    if(fs.existsSync(path.join(incoming,'agent'))) throw Error('An incoming agent tree already exists; nothing was replaced');
    const unpack=cp.spawnSync('tar',['-xzf',archive,'-C',incoming],{encoding:'utf8',timeout:30000});
    if(unpack.status!==0) throw Error(unpack.stderr||'Archive extraction failed');
    const installed=cp.spawnSync(process.execPath,[path.join(incoming,'agent','src','index.mjs'),'self-update','--install','--op',op],{env:{...process.env,LEGIONCTL_HOME:base},stdio:'inherit',timeout:240000});
    process.exit(installed.status===null?1:installed.status);
    """

    static func sshArguments(target: SSHTarget, system: SystemConfig, argv: [String]) -> [String] {
        var commandSystem = system
        commandSystem.agent = argv
        return RemoteCommand.sshArguments(target: target, connectTimeout: 8,
                                           command: RemoteCommand(system: commandSystem, arguments: []),
                                           knownHostsFile: AppPaths.overridesKnownHosts ? AppPaths.knownHostsFile.path : nil)
    }
    static func sftpArguments(target: SSHTarget) -> [String] {
        var arguments = ["-b", "-", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no", "-o", "ConnectTimeout=8"]
        if AppPaths.overridesKnownHosts {
            arguments += ["-F", "/dev/null", "-o", "UserKnownHostsFile=\(AppPaths.knownHostsFile.path)", "-o", "GlobalKnownHostsFile=/dev/null"]
        }
        if let port = target.port { arguments += ["-P", String(port)] }
        if let identity = target.identityFile { arguments += ["-i", (identity as NSString).expandingTildeInPath] }
        return arguments + ["--", target.destination]
    }
    static func sftpQuoted(_ path: String) throws -> String {
        guard !path.contains("\n"), !path.contains("\r"), !path.contains("\0") else { throw Failure(message: "A file path contains an invalid control character.") }
        return "\"" + path.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") + "\""
    }

    static func installRemote(payload: Data, operation: String, layout: Layout, system: SystemConfig, target: SSHTarget) async throws -> AgentActionResult {
        guard !system.isRestricted else { throw Failure(message: "A restricted legacy key cannot upload a bootstrap. Use an administrator connection.") }
        let archive = try await stagedArchive(payload, operation: operation)
        defer { try? FileManager.default.removeItem(at: archive) }
        let remoteArchive = layout.path("incoming/legion-bootstrap-\(operation).tgz").replacingOccurrences(of: "\\", with: "/")
        let prepare = await Shell.run(executable: "/usr/bin/ssh", arguments: sshArguments(target: target, system: system,
                                      argv: layout.prefix + ["-e", prepareScript, "--", layout.base]), timeout: 30)
        guard prepare.succeeded else { throw Failure(message: "Bootstrap preflight failed: \(prepare.failureText)") }
        let batch = "put \(try sftpQuoted(archive.path)) \(try sftpQuoted(remoteArchive))\n"
        let upload = await Shell.run(executable: "/usr/bin/sftp", arguments: sftpArguments(target: target), timeout: 120, input: Data(batch.utf8))
        guard upload.succeeded else { throw Failure(message: "The signed archive could not be uploaded: \(upload.failureText)") }
        let command = layout.prefix + ["-e", installScript, "--", layout.base, remoteArchive,
                                       ReleaseTrust.sha256(of: payload), String(payload.count), operation]
        let result = await Shell.run(executable: "/usr/bin/ssh", arguments: sshArguments(target: target, system: system, argv: command), timeout: 300)
        return try decode(result)
    }

    static func installLocal(payload: Data, operation: String, layout: Layout) async throws -> AgentActionResult {
        let archive = try await stagedArchive(payload, operation: operation)
        defer { try? FileManager.default.removeItem(at: archive) }
        let prepare = await Shell.run(executable: layout.prefix[0], arguments: Array(layout.prefix.dropFirst()) + ["-e", prepareScript, "--", layout.base], timeout: 30)
        guard prepare.succeeded else { throw Failure(message: "Bootstrap preflight failed: \(prepare.failureText)") }
        let args = Array(layout.prefix.dropFirst()) + ["-e", installScript, "--", layout.base, archive.path,
                                                    ReleaseTrust.sha256(of: payload), String(payload.count), operation]
        return try decode(await Shell.run(executable: layout.prefix[0], arguments: args, timeout: 300))
    }

    static func decode(_ result: CommandResult) throws -> AgentActionResult {
        if let json = RemoteAgent.extractJSONObject(from: result.standardOutput),
           let reply = try? JSONDecoder().decode(AgentActionResult.self, from: json) { return reply }
        throw Failure(message: "The bootstrap returned no acknowledged result: \(result.failureText)", uncertain: true)
    }
}
