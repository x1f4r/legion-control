import Foundation
import Observation

/// The controller config: which machines exist, how to reach them, how to wake them, and which
/// systems each of them can boot into.
///
/// Nothing in this app knows a machine until this file names one. It lives at
/// `~/.config/legion-control/config.json`, it is never written by an installer, and the only thing
/// that ever writes it from here is the example the setup page offers when there is nothing there.

// MARK: - The document

struct ControllerConfig: Decodable, Sendable, Equatable {
    var version: Int?
    var machines: [Machine]
    var local: LocalConfig?

    /// Where both apps look for their own updates. One key, one repository, and the same releases
    /// on the phone and on the Mac.
    var appUpdates: AppUpdatesConfig?

    struct AppUpdatesConfig: Decodable, Sendable, Equatable {
        /// This project's own releases, which is where an unmodified build should look. A fork
        /// names itself here instead.
        static let defaultRepo = "x1f4r/legion-control"

        var githubRepo: String?
    }

    /// The repository to read releases from, with the default filled in. A config that leaves the
    /// key out, or leaves it empty, is not asking for updates to be turned off; it is not asking
    /// for anything, and the answer to that is this project.
    var updateRepo: String {
        guard let named = appUpdates?.githubRepo, !named.trimmingCharacters(in: .whitespaces).isEmpty else {
            return AppUpdatesConfig.defaultRepo
        }
        return named
    }

    /// The machines the app can actually drive, in the order the file lists them.
    var isEmpty: Bool { machines.isEmpty }

    var localIsEnabled: Bool { local?.enabled == true }

    func machine(id: String) -> Machine? { machines.first { $0.id == id } }
}

/// One physical box: one network card, one hardware address, one thing to wake.
struct Machine: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    var ssh: SSHTarget?
    var endpoints: [Endpoint] = []
    var wake: WakeConfig?
    var systems: [SystemConfig]

    private enum CodingKeys: String, CodingKey {
        case id, name, ssh, endpoints, wake, systems
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        name = try container.decodeIfPresent(String.self, forKey: .name) ?? id
        ssh = try container.decodeIfPresent(SSHTarget.self, forKey: .ssh)
        endpoints = try container.decodeIfPresent([Endpoint].self, forKey: .endpoints) ?? []
        wake = try container.decodeIfPresent(WakeConfig.self, forKey: .wake)
        systems = try container.decodeIfPresent([SystemConfig].self, forKey: .systems) ?? []
    }

    func system(id: String) -> SystemConfig? { systems.first { $0.id == id } }

    /// The first configured system running the given platform. This is the second chance a status
    /// gets to be matched: an agent whose system id we do not know still says which platform it is,
    /// and one Linux system in the config is unambiguously the Linux one.
    func system(platform: Platform) -> SystemConfig? { systems.first { $0.platform == platform } }

    /// What to hand to ssh. Either the explicit block or, failing that, the first endpoint: an
    /// address list is written for the phone, but the first entry is still a machine this Mac can
    /// dial, and it beats having nothing to try.
    var sshTarget: SSHTarget? {
        if let ssh, !ssh.host.isEmpty { return ssh }
        guard let first = endpoints.first else { return nil }
        return SSHTarget(host: first.host, user: first.user, port: first.port, identityFile: nil)
    }
}

/// Everything ssh needs. An alias out of `~/.ssh/config` is the simplest thing to put here, and it
/// lets ssh do the routing rather than this app.
struct SSHTarget: Decodable, Sendable, Equatable {
    var host: String
    var user: String?
    var port: Int?
    var identityFile: String?

    /// `user@host`, or just the host when the config leaves the user to ssh.
    var destination: String {
        guard let user, !user.isEmpty else { return host }
        return "\(user)@\(host)"
    }

    /// What the "On the network" row shows: the destination, and the port when it is not the usual one.
    var display: String {
        guard let port, port != 22 else { return destination }
        return "\(destination):\(port)"
    }

    /// The flags that go in front of the destination.
    var sshOptions: [String] {
        var options: [String] = []
        if let port { options += ["-p", String(port)] }
        if let identityFile, !identityFile.isEmpty {
            options += ["-i", (identityFile as NSString).expandingTildeInPath]
        }
        return options
    }
}

/// An address to dial directly. Written for the phone, which has no ssh config to lean on; read here
/// only when the machine has no `ssh` block of its own.
struct Endpoint: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var kind: String?
    var host: String
    var port: Int?
    var user: String?
    var system: String?
    var label: String?

    private enum CodingKeys: String, CodingKey {
        case id, kind, host, port, user, system, label
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        host = try container.decode(String.self, forKey: .host)
        id = try container.decodeIfPresent(String.self, forKey: .id) ?? host
        kind = try container.decodeIfPresent(String.self, forKey: .kind)
        port = try container.decodeIfPresent(Int.self, forKey: .port)
        user = try container.decodeIfPresent(String.self, forKey: .user)
        system = try container.decodeIfPresent(String.self, forKey: .system)
        label = try container.decodeIfPresent(String.self, forKey: .label)
    }
}

/// Wake on LAN. A machine without this block simply has no Wake button.
struct WakeConfig: Decodable, Sendable, Equatable {
    var mac: String
    var broadcast: [String]
    var ports: [UInt16]
    var probe: Probe?
    /// Android only: the phone counts as at home when one of its addresses starts with this. Decoded
    /// so the same document works on both apps.
    var lanPrefix: String?

    struct Probe: Decodable, Sendable, Equatable {
        var host: String
        var port: UInt16?

        var probePort: UInt16 { port ?? 22 }
    }

    private enum CodingKeys: String, CodingKey {
        case mac, broadcast, ports, probe, lanPrefix
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        mac = try container.decode(String.self, forKey: .mac)
        broadcast = try container.decodeIfPresent([String].self, forKey: .broadcast) ?? ["255.255.255.255"]
        ports = try container.decodeIfPresent([UInt16].self, forKey: .ports) ?? [9, 7]
        probe = try container.decodeIfPresent(Probe.self, forKey: .probe)
        lanPrefix = try container.decodeIfPresent(String.self, forKey: .lanPrefix)
    }

    /// The six bytes of the hardware address, or nil when it is not one. Validation refuses a config
    /// that gets this far with nil, so nothing downstream has to explain a wake that silently did
    /// nothing.
    var macBytes: [UInt8]? {
        let parts = mac.components(separatedBy: CharacterSet(charactersIn: ":-"))
        let bytes = parts.compactMap { UInt8($0, radix: 16) }
        guard parts.count == 6, bytes.count == 6 else { return nil }
        return bytes
    }
}

/// One operating system installed on a machine. A dual boot machine has two of these and only one
/// of them is up at a time.
struct SystemConfig: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    var platform: Platform
    /// The exact argv run over ssh. It is invoked unquoted, so it must not need quoting.
    var agent: [String]
    /// Overrides the platform's icon with any SF Symbol name.
    var symbol: String?

    private enum CodingKeys: String, CodingKey {
        case id, name, platform, agent, symbol
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        platform = try container.decodeIfPresent(Platform.self, forKey: .platform) ?? .linux
        name = try container.decodeIfPresent(String.self, forKey: .name) ?? platform.defaultName
        agent = try container.decodeIfPresent([String].self, forKey: .agent) ?? []
        symbol = try container.decodeIfPresent(String.self, forKey: .symbol)
    }

    var symbolName: String { symbol ?? platform.symbolName }
}

enum Platform: String, Decodable, Sendable, Equatable, CaseIterable {
    case linux
    case windows
    case mac

    /// Only used when a system leaves its name out.
    var defaultName: String {
        switch self {
        case .linux: "Linux"
        case .windows: "Windows"
        case .mac: "macOS"
        }
    }

    /// The icon a system gets when it does not name one of its own.
    var symbolName: String {
        switch self {
        case .linux: "terminal"
        case .windows: "macwindow"
        case .mac: "laptopcomputer"
        }
    }
}

/// The device the app itself runs on, looked after by an agent running here rather than over ssh.
struct LocalConfig: Decodable, Sendable, Equatable {
    var enabled: Bool
    var name: String
    /// Where the local agent's entry point is. A leading tilde is expanded.
    var agent: String

    private enum CodingKeys: String, CodingKey {
        case enabled, name, agent
    }

    static let defaultAgentPath = "~/.legion-control/agent/src/index.mjs"

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        enabled = try container.decodeIfPresent(Bool.self, forKey: .enabled) ?? true
        name = try container.decodeIfPresent(String.self, forKey: .name) ?? "This Mac"
        agent = try container.decodeIfPresent(String.self, forKey: .agent) ?? Self.defaultAgentPath
    }

    var agentPath: String { (agent as NSString).expandingTildeInPath }
}

// MARK: - Validation

/// What a config can be wrong about. Every case names the thing in the file that has to change,
/// because the only place these are ever read is the setup page, and "invalid config" on its own
/// helps nobody.
struct ConfigProblem: Error, Sendable {
    var message: String

    init(_ message: String) { self.message = message }
}

extension ControllerConfig {
    /// Read on load. A config that fails this is not half-applied: the previous one stays and the
    /// window says why, because a machine with no systems or an unparseable hardware address has no
    /// sensible default to fall back to.
    func validated() throws -> ControllerConfig {
        var seenMachines = Set<String>()
        for machine in machines {
            guard !machine.id.isEmpty else {
                throw ConfigProblem("A machine has an empty id.")
            }
            guard seenMachines.insert(machine.id).inserted else {
                throw ConfigProblem("Two machines share the id \"\(machine.id)\". Ids have to be unique.")
            }
            guard !machine.systems.isEmpty else {
                throw ConfigProblem("Machine \"\(machine.id)\" lists no systems. Every machine needs at least one.")
            }
            guard machine.sshTarget != nil else {
                throw ConfigProblem("Machine \"\(machine.id)\" has neither an \"ssh\" block nor any endpoints, so there is no way to reach it.")
            }

            var seenSystems = Set<String>()
            for system in machine.systems {
                guard !system.id.isEmpty else {
                    throw ConfigProblem("A system on machine \"\(machine.id)\" has an empty id.")
                }
                guard seenSystems.insert(system.id).inserted else {
                    throw ConfigProblem("Machine \"\(machine.id)\" has two systems with the id \"\(system.id)\".")
                }
                guard !system.agent.isEmpty else {
                    throw ConfigProblem("System \"\(system.id)\" on machine \"\(machine.id)\" has no \"agent\" command. It has to be the argv to run over ssh.")
                }
            }

            if let wake = machine.wake, wake.macBytes == nil {
                throw ConfigProblem("The wake address \"\(wake.mac)\" on machine \"\(machine.id)\" is not a six byte hardware address.")
            }
        }
        return self
    }
}

// MARK: - Loading and watching

/// Holds whatever the config file last said, and notices when it changes.
///
/// The rule the window depends on: a file that does not parse never replaces one that did. A
/// half-saved file is the normal state of a file being edited, and throwing the machines away every
/// time an editor writes a partial document would make the app flicker through its own setup page.
@MainActor
@Observable
final class ConfigStore {
    /// The last config that parsed and validated, or nil when none ever has.
    private(set) var config: ControllerConfig?
    /// Why the file on disk was refused, or nil when it was read cleanly. Non-nil alongside a
    /// config means the file was edited into something broken and the app is still on the old one.
    private(set) var problem: String?
    /// True when there is simply nothing at the path. Told apart from a broken file because the
    /// setup page offers to write an example only into empty space.
    private(set) var isMissing = false

    let url: URL

    @ObservationIgnored private var watcher: DispatchSourceFileSystemObject?
    @ObservationIgnored private var watchedDescriptor: CInt = -1
    @ObservationIgnored private var lastSignature: String?

    /// Called after a load that actually changed something, so the app can rebuild its machines.
    var onChange: (@MainActor () -> Void)?

    init(url: URL? = nil) {
        self.url = url ?? Self.defaultURL
        load()
        startWatching()
    }

    deinit {
        watcher?.cancel()
    }

    /// `~/.config/legion-control/config.json`, or whatever LEGION_CONTROL_CONFIG names. The override
    /// exists so a second config can be tried without touching the real one.
    static var defaultURL: URL {
        if let override = ProcessInfo.processInfo.environment["LEGION_CONTROL_CONFIG"], !override.isEmpty {
            return URL(fileURLWithPath: (override as NSString).expandingTildeInPath)
        }
        return FileManager.default.homeDirectoryForCurrentUser
            .appending(path: ".config/legion-control/config.json")
    }

    var path: String { url.path(percentEncoded: false) }

    /// The machines to draw, or an empty list when there is nothing usable.
    var machines: [Machine] { config?.machines ?? [] }

    var local: LocalConfig? {
        guard let local = config?.local, local.enabled else { return nil }
        return local
    }

    /// Whether the window has anything at all to show besides the setup page.
    var hasAnything: Bool { !machines.isEmpty || local != nil }

    // MARK: - Reading

    /// A cheap "did it change" check, called from the poll and whenever a viewer comes back. The
    /// vnode watcher below catches almost everything; this catches the rest, including a directory
    /// that did not exist when the watcher was armed.
    func reloadIfChanged() {
        let signature = Self.signature(of: url)
        if signature != lastSignature { load() }
        if watcher == nil { startWatching() }
    }

    private func load() {
        lastSignature = Self.signature(of: url)

        guard let data = try? Data(contentsOf: url) else {
            isMissing = true
            // Not an error worth showing: no file is the state a fresh install is in, and the setup
            // page says what to do about it. An earlier config is kept, so a file that is being
            // rewritten in place does not blank the window on its way past zero bytes.
            problem = nil
            if config != nil, FileManager.default.fileExists(atPath: path) { return }
            if config != nil {
                config = nil
                onChange?()
            }
            return
        }

        isMissing = false
        do {
            let decoded = try JSONDecoder().decode(ControllerConfig.self, from: data)
            let checked = try decoded.validated()
            problem = nil
            guard checked != config else { return }
            config = checked
            onChange?()
        } catch let problem as ConfigProblem {
            self.problem = problem.message
        } catch let error as DecodingError {
            problem = Self.describe(error)
        } catch {
            problem = error.localizedDescription
        }
    }

    /// Decoding errors read as one long sentence about coding paths. Cut them down to the two things
    /// worth knowing: which key, and what was wrong with it.
    private static func describe(_ error: DecodingError) -> String {
        func path(_ context: DecodingError.Context) -> String {
            let parts = context.codingPath.map { key -> String in
                if let index = key.intValue { return "[\(index)]" }
                return key.stringValue
            }
            return parts.isEmpty ? "the document" : parts.joined(separator: ".")
        }
        switch error {
        case .keyNotFound(let key, let context):
            return "\(path(context)) is missing the key \"\(key.stringValue)\"."
        case .typeMismatch(_, let context), .valueNotFound(_, let context):
            return "\(path(context)): \(context.debugDescription)"
        case .dataCorrupted(let context):
            return context.codingPath.isEmpty
                ? "The file is not valid JSON. \(context.debugDescription)"
                : "\(path(context)): \(context.debugDescription)"
        @unknown default:
            return "The file could not be read."
        }
    }

    /// Modification date and size together. Either alone misses an edit, and reading the whole file
    /// on every poll to hash it would be work for nothing.
    private static func signature(of url: URL) -> String? {
        guard let values = try? url.resourceValues(forKeys: [.contentModificationDateKey, .fileSizeKey]),
              let modified = values.contentModificationDate
        else { return nil }
        return "\(modified.timeIntervalSince1970):\(values.fileSize ?? -1)"
    }

    // MARK: - Watching

    /// The directory is watched rather than the file. Editors save by writing a temporary file and
    /// renaming it over the top, which replaces the inode, and a descriptor held on the old inode
    /// would never see another thing.
    private func startWatching() {
        stopWatching()
        let directory = url.deletingLastPathComponent()
        let descriptor = open(directory.path(percentEncoded: false), O_EVTONLY)
        guard descriptor >= 0 else { return }

        let source = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: descriptor,
            eventMask: [.write, .rename, .delete, .attrib],
            queue: .main
        )
        source.setEventHandler { [weak self] in
            MainActor.assumeIsolated { self?.reloadIfChanged() }
        }
        source.setCancelHandler { close(descriptor) }
        watcher = source
        watchedDescriptor = descriptor
        source.resume()
    }

    private func stopWatching() {
        watcher?.cancel()
        watcher = nil
        watchedDescriptor = -1
    }

    // MARK: - Writing the example

    /// Puts the documented example at the path, and refuses to write over anything. Returns nil on
    /// success or a sentence to show.
    @discardableResult
    func writeExample() -> String? {
        if FileManager.default.fileExists(atPath: path) {
            return "There is already a file at \(path). Open it instead."
        }
        do {
            try FileManager.default.createDirectory(
                at: url.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            try Data(Self.exampleConfig.utf8).write(to: url, options: .atomic)
            reloadIfChanged()
            return nil
        } catch {
            return "The example could not be written to \(path). \(error.localizedDescription)"
        }
    }

    /// The example from the documentation, with the comments taken out so it is a file this app can
    /// read back. Everything in it is a placeholder: it is meant to be edited, not used.
    static let exampleConfig = """
    {
      "version": 1,
      "machines": [
        {
          "id": "legion",
          "name": "Legion",
          "ssh": { "host": "legion" },
          "endpoints": [
            { "id": "tailnet-linux", "kind": "remote", "host": "100.64.0.10", "port": 22, "user": "me", "system": "cachyos", "label": "Tailnet, CachyOS" },
            { "id": "tailnet-windows", "kind": "remote", "host": "100.64.0.11", "user": "me", "system": "windows", "label": "Tailnet, Windows" },
            { "id": "lan", "kind": "lan", "host": "10.0.0.40", "user": "me", "label": "Home LAN" }
          ],
          "wake": {
            "mac": "AA:BB:CC:DD:EE:FF",
            "broadcast": ["10.0.0.255"],
            "ports": [9, 7],
            "probe": { "host": "10.0.0.40", "port": 22 },
            "lanPrefix": "10.0.0."
          },
          "systems": [
            { "id": "cachyos", "name": "CachyOS", "platform": "linux",
              "agent": ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"] },
            { "id": "windows", "name": "Windows 11", "platform": "windows",
              "agent": ["node", "C:\\\\Users\\\\me\\\\.legion-control\\\\agent\\\\src\\\\index.mjs"] }
          ]
        }
      ],
      "local": { "enabled": true, "name": "This Mac", "agent": "~/.legion-control/agent/src/index.mjs" }
    }

    """
}
