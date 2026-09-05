import CryptoKit
import Darwin
import Foundation

struct HostPublicKey: Codable, Equatable, Hashable, Sendable, Identifiable {
    var algorithm: String
    var blob: String
    var id: String { "\(algorithm) \(blob)" }
    var fingerprint: String {
        let digest = SHA256.hash(data: Data(base64Encoded: blob) ?? Data())
        return "\(algorithm) SHA256:\(Data(digest).base64EncodedString().replacingOccurrences(of: "=", with: ""))"
    }

    static func parse(_ lines: String, uniqueAlgorithms: Bool) throws -> [Self] {
        var keys: [Self] = []
        for line in lines.split(separator: "\n") where !line.hasPrefix("#") {
            let words = line.split(whereSeparator: \.isWhitespace)
            guard words.count >= 3, !words[0].hasPrefix("@"),
                  let data = Data(base64Encoded: String(words[2])), data.count >= 4 else {
                throw HostIdentityFailure("The host keys could not be inspected reliably. App enrollment is unavailable.")
            }
            let length = data.prefix(4).reduce(0) { ($0 << 8) | Int($1) }
            let algorithm = String(words[1])
            guard length > 0, length < data.count - 4,
                  String(data: data.subdata(in: 4..<(4 + length)), encoding: .utf8) == algorithm,
                  ["ssh-ed25519", "ssh-rsa", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"].contains(algorithm) else {
                throw HostIdentityFailure("This host key format is not supported for app enrollment. Native SSH verification remains in use.")
            }
            let key = Self(algorithm: algorithm, blob: String(words[2]))
            if !keys.contains(key) { keys.append(key) }
        }
        guard !uniqueAlgorithms || Set(keys.map(\.algorithm)).count == keys.count else {
            throw HostIdentityFailure("The scan contains more than one key for an algorithm. Nothing was approved.")
        }
        return keys
    }
}

struct HostIdentityFailure: Error, LocalizedError {
    var message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}

struct HostIdentityDocument: Codable, Sendable {
    var version = 1
    var revision = 0
    var endpoints: [Endpoint] = []
    struct Endpoint: Codable, Sendable {
        var address: String
        var systems: [System]
    }
    struct System: Codable, Equatable, Sendable, Identifiable {
        var id: String
        var name: String
        var keys: [HostPublicKey] = []
    }
}

/// The effective OpenSSH lookup identity and the exact pin-file snapshots used for this decision.
struct HostIdentityContext: Sendable {
    var address: String
    var lookup: String
    var scanHost: String
    var port: Int
    var knownHosts: URL
    var pinFiles: [URL: Data]
    var existing: [HostPublicKey]

    static func resolve(_ target: SSHTarget,
                        runner: @Sendable (String, [String], TimeInterval) async -> CommandResult = {
                            await Shell.run(executable: $0, arguments: $1, timeout: $2)
                        }) async throws -> Self {
        let isolated = AppPaths.overridesKnownHosts
        let options = isolated ? ["-F", "/dev/null", "-o", "UserKnownHostsFile=\(AppPaths.knownHostsFile.path)", "-o", "GlobalKnownHostsFile=/dev/null"] : []
        let config = await runner("/usr/bin/ssh", ["-G"] + options + target.sshOptions + ["--", target.destination], 10)
        guard config.succeeded else { throw HostIdentityFailure("OpenSSH's effective settings could not be read.") }
        let values = Dictionary(config.standardOutput.split(separator: "\n").compactMap { line -> (String, String)? in
            guard let split = line.firstIndex(of: " ") else { return nil }
            return (String(line[..<split]), String(line[line.index(after: split)...]))
        }, uniquingKeysWith: { first, _ in first })
        guard values["proxycommand"] == nil || values["proxycommand"] == "none",
              values["proxyjump"] == nil || values["proxyjump"] == "none",
              values["knownhostscommand"] == nil || values["knownhostscommand"] == "none" else {
            throw HostIdentityFailure("This route uses an SSH proxy or external host-key provider. Approve its keys through your SSH setup; app enrollment cannot inspect it reliably.")
        }
        let host = values["hostname"] ?? target.host
        let port = Int(values["port"] ?? "") ?? target.port ?? 22
        let alias = values["hostkeyalias"].flatMap { $0 == "none" ? nil : $0 }
        let lookup = alias ?? (port == 22 ? host : "[\(host)]:\(port)")
        func paths(_ key: String) throws -> [URL] {
            let raw = values[key] ?? ""
            // OpenSSH emits escaped/quoted file names when needed. Unsupported complex settings
            // remain native SSH settings; enrollment must never redirect around them.
            guard !raw.contains("\""), !raw.contains("\\"), !raw.contains("%") else {
                throw HostIdentityFailure("The configured known_hosts paths need manual SSH enrollment.")
            }
            return raw.split(separator: " ").filter { $0 != "none" && $0 != "/dev/null" }.map {
                URL(fileURLWithPath: (String($0) as NSString).expandingTildeInPath)
            }
        }
        let userFiles = try paths("userknownhostsfile")
        guard let destination = userFiles.first else { throw HostIdentityFailure("No writable user known_hosts file is configured.") }
        let allFiles = userFiles + (try paths("globalknownhostsfile"))
        var snapshots: [URL: Data] = [:]
        var existing: [HostPublicKey] = []
        for file in allFiles {
            let data = try? Data(contentsOf: file)
            if data == nil, FileManager.default.fileExists(atPath: file.path) {
                throw HostIdentityFailure("A configured host-key file cannot be read. App enrollment is unavailable.")
            }
            snapshots[file] = data ?? Data()
            guard data != nil else { continue }
            let found = await runner("/usr/bin/ssh-keygen", ["-F", lookup, "-f", file.path], 10)
            guard found.exitCode == 0 || found.exitCode == 1 else { throw HostIdentityFailure("Existing host pins could not be inspected.") }
            for key in try HostPublicKey.parse(found.standardOutput, uniqueAlgorithms: false) where !existing.contains(key) { existing.append(key) }
        }
        return Self(address: "\(lookup.lowercased()):\(port)", lookup: lookup, scanHost: host, port: port,
                    knownHosts: destination, pinFiles: snapshots, existing: existing)
    }
}

struct HostIdentityStore {
    let url: URL
    var write: (Data, URL) throws -> Void
    init(url: URL = AppPaths.file("host-identities.json"), write: @escaping (Data, URL) throws -> Void = { try AtomicFile.write($0, to: $1) }) {
        self.url = url
        self.write = write
    }
    private static let processLock = NSLock()

    func load() throws -> HostIdentityDocument {
        guard FileManager.default.fileExists(atPath: url.path) else { return HostIdentityDocument() }
        let document = try JSONDecoder().decode(HostIdentityDocument.self, from: Data(contentsOf: url))
        guard document.version == 1 else { throw HostIdentityFailure("Unsupported host identity settings. No key was added.") }
        return document
    }

    func approve(context: HostIdentityContext, expectedRevision: Int, systems: [HostIdentityDocument.System],
                 selected: String, offered: [HostPublicKey], legacyAssignments: [String: String]) throws {
        try Self.processLock.withLock {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let descriptor = open(url.appendingPathExtension("lock").path, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR)
            guard descriptor >= 0 else { throw HostIdentityFailure("The host identity lock could not be opened.") }
            defer { close(descriptor) }
            guard flock(descriptor, LOCK_EX | LOCK_NB) == 0 else { throw HostIdentityFailure("Another host approval is being saved. Read the keys again.") }
            defer { flock(descriptor, LOCK_UN) }
            var document = try load()
            if document.endpoints.first(where: { $0.address == context.address })?.systems.first(where: { $0.id == selected })?.keys == offered,
               !offered.isEmpty,
               let current = try? String(contentsOf: context.knownHosts, encoding: .utf8) {
                let lines = Set(current.split(separator: "\n").map(String.init))
                let unchangedPins = context.pinFiles.allSatisfy { ((try? Data(contentsOf: $0.key)) ?? Data()) == $0.value }
                if offered.allSatisfy({ (unchangedPins && context.existing.contains($0)) || lines.contains("\(context.lookup) \($0.algorithm) \($0.blob)") }) {
                    return
                }
            }
            for (file, snapshot) in context.pinFiles {
                guard ((try? Data(contentsOf: file)) ?? Data()) == snapshot else {
                    throw HostIdentityFailure("The SSH pins changed while this question was open. Read the keys again.")
                }
            }
            var endpoint = document.endpoints.first { $0.address == context.address }
                ?? .init(address: context.address, systems: [])
            let identical = endpoint.systems.first { $0.id == selected }?.keys == offered
            guard document.revision == expectedRevision || identical else {
                throw HostIdentityFailure("Another approval changed the local host identities. Read the keys again.")
            }
            guard !offered.isEmpty, Set(offered.map(\.algorithm)).count == offered.count else {
                throw HostIdentityFailure("The displayed key scan is empty or ambiguous.")
            }
            let tracked = Set(endpoint.systems.flatMap(\.keys))
            let unassigned = context.existing.filter { !tracked.contains($0) }
            if endpoint.systems.isEmpty || !unassigned.isEmpty {
                guard !systems.isEmpty, Set(systems.map(\.id)).count == systems.count,
                      systems.allSatisfy({ AgentToken.isValid($0.id) && $0.keys.isEmpty }) else {
                    throw HostIdentityFailure("Confirm the operating systems for this address before approving a key.")
                }
                for system in systems where !endpoint.systems.contains(where: { $0.id == system.id }) { endpoint.systems.append(system) }
                for key in unassigned {
                    guard let group = legacyAssignments[key.id], let index = endpoint.systems.firstIndex(where: { $0.id == group }) else {
                        throw HostIdentityFailure("Assign every existing fingerprint to its operating system before adding another key.")
                    }
                    guard !endpoint.systems[index].keys.contains(where: { $0.algorithm == key.algorithm && $0 != key }) else {
                        throw HostIdentityFailure("Two different keys for one algorithm cannot belong to the same operating system.")
                    }
                    if !endpoint.systems[index].keys.contains(key) { endpoint.systems[index].keys.append(key) }
                }
            }
            guard let index = endpoint.systems.firstIndex(where: { $0.id == selected }) else {
                throw HostIdentityFailure("Choose a locally confirmed operating system for this key.")
            }
            guard endpoint.systems[index].keys.isEmpty || endpoint.systems[index].keys == offered else {
                throw HostIdentityFailure("That operating system already has approved keys. A replacement requires separate trust management.")
            }
            let others = Set(endpoint.systems.enumerated().filter { $0.offset != index }.flatMap { $0.element.keys })
            guard offered.allSatisfy({ !others.contains($0) }) else {
                throw HostIdentityFailure("This scan overlaps another operating system's keys. Nothing was added.")
            }
            endpoint.systems[index].keys = offered
            document.endpoints.removeAll { $0.address == context.address }
            document.endpoints.append(endpoint)
            document.revision += 1
            let encoder = JSONEncoder(); encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try write(encoder.encode(document), url)
            let existingBytes = context.pinFiles[context.knownHosts] ?? Data()
            var updated = existingBytes
            if !updated.isEmpty, updated.last != 10 { updated.append(10) }
            for key in offered where !context.existing.contains(key) {
                updated.append(Data("\(context.lookup) \(key.algorithm) \(key.blob)\n".utf8))
            }
            try write(updated, context.knownHosts)
        }
    }
}
