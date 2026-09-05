import Foundation
import Observation

/// This device's own settings, which are never published to anybody.
///
/// The shared setup describes the fleet: which machines exist, how to reach them, what they can boot
/// into. It is the same document on every device. What is *not* the same on every device is which of
/// those machines this one is, which key it offers, what its `~/.ssh/config` calls things, and how to
/// run an agent here without going through ssh to reach itself. Those used to sit in the shared
/// document — `machine.ssh.identityFile` is an absolute path that only means anything on one Mac,
/// and `local` means "this Mac" on whichever device happens to be reading it — which made the
/// document unpublishable in a fleet of peers.
///
/// So they live here, beside the config and never inside it. Nothing in this file is ever sent to a
/// machine, and no private key is ever copied anywhere.
struct Bindings: Codable, Sendable, Equatable {
    /// The human name for this device, written into `controller.device` on every edit so a
    /// divergence can say who made which side.
    var deviceName: String?
    /// Which machine in the shared document this device is, when it is one at all.
    var selfBinding: SelfBinding?
    /// How to run the agent here without ssh.
    var localAgent: LocalAgentBinding?
    /// The key this device offers by default.
    var identityFile: String?
    /// Per-machine overrides.
    var machines: [String: MachineBinding]?
    /// Which site this device believes it is at right now.
    ///
    /// A private-address prefix is a hint and never proof: two households behind stock routers both
    /// live on 192.168.178, so matching a prefix says "this could be site A" and nothing more. This
    /// is the user's own answer, used only to order routes and to decide whether a direct magic
    /// packet is worth trying.
    var currentSite: String?

    struct SelfBinding: Codable, Sendable, Equatable {
        var machine: String
        var system: String?
    }

    /// The argv to spawn. No shell: the binary and its arguments go straight to the process, so
    /// there is nothing to quote and nothing to escape. Quoting rules exist for ssh, which joins
    /// words and hands them to a remote shell; a local spawn has no such step.
    struct LocalAgentBinding: Codable, Sendable, Equatable {
        var argv: [String]

        var isUsable: Bool { !argv.isEmpty && !argv[0].isEmpty }
    }

    struct MachineBinding: Codable, Sendable, Equatable {
        var identityFile: String?
        /// An entry in this device's own `~/.ssh/config`. Private because it names something only
        /// this device has.
        var sshAlias: String?
        /// A port to go with the alias, when the alias does not carry one.
        var port: Int?
        var user: String?
    }

    private enum CodingKeys: String, CodingKey {
        case deviceName
        case selfBinding = "self"
        case localAgent, identityFile, machines, currentSite
    }

    init(
        deviceName: String? = nil,
        selfBinding: SelfBinding? = nil,
        localAgent: LocalAgentBinding? = nil,
        identityFile: String? = nil,
        machines: [String: MachineBinding]? = nil,
        currentSite: String? = nil
    ) {
        self.deviceName = deviceName
        self.selfBinding = selfBinding
        self.localAgent = localAgent
        self.identityFile = identityFile
        self.machines = machines
        self.currentSite = currentSite
    }

    static let empty = Bindings()

    /// The name to write into an edit, falling back to what the Mac calls itself.
    var effectiveDeviceName: String {
        if let deviceName, !deviceName.isEmpty { return deviceName }
        return Host.current().localizedName ?? ProcessInfo.processInfo.hostName
    }

    /// The key to offer for one machine: its own override, then this device's default, then the
    /// deprecated one in the shared document, then the key this app would generate.
    func identityFile(forMachine id: String, sharedFallback: String?) -> String? {
        if let specific = machines?[id]?.identityFile, !specific.isEmpty { return specific }
        if let identityFile, !identityFile.isEmpty { return identityFile }
        if let sharedFallback, !sharedFallback.isEmpty { return sharedFallback }
        let generated = AppPaths.defaultIdentityFile.path(percentEncoded: false)
        return FileManager.default.isReadableFile(atPath: generated) ? generated : nil
    }

    func binding(forMachine id: String) -> MachineBinding? { machines?[id] }

    /// Whether this device is the given machine.
    func isSelf(_ machineId: String) -> Bool { selfBinding?.machine == machineId }

    /// Whether this device can drive itself without ssh.
    var canControlSelfLocally: Bool { selfBinding != nil && localAgent?.isUsable == true }
}

/// Holds the bindings file and writes it back.
///
/// Deliberately not observable through the config store: an edit here changes how this device talks
/// to machines and nothing about the document every device shares, so the two must not be able to
/// drift into each other.
@MainActor
@Observable
final class BindingsStore {
    private(set) var bindings: Bindings = .empty
    /// Whether this device has opted into private bindings at all. Once it has, the deprecated
    /// shared `local` block no longer gets to describe this device.
    private(set) var isPresent = false
    /// Why the file could not be read or written, when that happened.
    private(set) var problem: String?

    let url: URL

    var onChange: (@MainActor () -> Void)?

    init(url: URL? = nil) {
        self.url = url ?? AppPaths.bindingsFile
        load()
    }

    var path: String { url.path(percentEncoded: false) }

    func reloadIfChanged() {
        let previous = bindings
        let existed = isPresent
        load()
        if bindings != previous || isPresent != existed { onChange?() }
    }

    func load() {
        guard let data = try? Data(contentsOf: url) else {
            // No file is the normal state: a device with no private settings simply has none, and
            // everything falls back to the shared document and this app's own defaults.
            bindings = .empty
            isPresent = false
            problem = nil
            return
        }
        isPresent = true
        do {
            bindings = try JSONDecoder().decode(Bindings.self, from: data)
            problem = nil
        } catch {
            // Kept as it was rather than reset. A broken bindings file is a file someone is editing,
            // and throwing away which machine this device is would be a surprising way to react.
            problem = "\(path) could not be read, so this device's private settings are the ones from before it. \(error.localizedDescription)"
        }
    }

    @discardableResult
    func update(_ change: (inout Bindings) -> Void) -> String? {
        var edited = bindings
        change(&edited)
        do {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
            try AtomicFile.write(encoder.encode(edited), to: url)
            bindings = edited
            isPresent = true
            problem = nil
            onChange?()
            return nil
        } catch let failure as AtomicFile.WriteFailure {
            problem = failure.message
            return failure.message
        } catch {
            problem = error.localizedDescription
            return error.localizedDescription
        }
    }
}
