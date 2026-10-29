import CryptoKit
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

    /// Who this document is and what it descends from. Absent from a hand written file, which is
    /// why every reader has to cope with it being missing rather than refusing the document.
    var controller: ControllerIdentity?

    /// Where the machines physically are. Optional: a setup with one network needs none.
    var sites: [Site]?

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

    /// The identity to compare replicas by. A document with no `controller` block is revision zero
    /// with no id, which is exactly what it should be: it can be replaced by anything, and it can
    /// never be used to argue that a numbered document is out of date.
    var identity: ControllerIdentity { controller ?? ControllerIdentity() }

    func machine(id: String) -> Machine? { machines.first { $0.id == id } }

    func site(id: String) -> Site? { (sites ?? []).first { $0.id == id } }

    var allSites: [Site] { sites ?? [] }
}

/// One place the machines are: a house, a flat, an office.
///
/// A site exists so the app can tell "I am on that network, I can send a magic packet myself" from
/// "I am somewhere else and need a helper over there". Its address prefixes are a hint and never a
/// proof: two households behind stock routers are both on 192.168.50, so a prefix that matches says
/// this *could* be the site and nothing stronger. What proves which machine answered is the pinned
/// host key, and that is checked on every connection whatever this says.
struct Site: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    /// Address prefixes a controller on this site would have. Advisory.
    var lanPrefixes: [String]
    /// Where a magic packet for a machine at this site goes when the machine names none of its own.
    var broadcast: [String]

    private enum CodingKeys: String, CodingKey {
        case id, name, lanPrefixes, broadcast
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        name = try container.decodeIfPresent(String.self, forKey: .name) ?? id
        lanPrefixes = try container.decodeIfPresent([String].self, forKey: .lanPrefixes) ?? []
        broadcast = try container.decodeIfPresent([String].self, forKey: .broadcast) ?? []
    }

    init(id: String, name: String? = nil, lanPrefixes: [String] = [], broadcast: [String] = []) {
        self.id = id
        self.name = name ?? id
        self.lanPrefixes = lanPrefixes
        self.broadcast = broadcast
    }
}

/// One physical box: one network card, one hardware address, one thing to wake.
struct Machine: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    /// Deprecated in a shared document: an ssh alias and an identity file only mean something on one
    /// device. Still read, never written by the editor, and never stripped on its own — removing it
    /// would change the bytes and therefore the hash, on one device only.
    var ssh: SSHTarget?
    var endpoints: [Endpoint] = []
    var wake: WakeConfig?
    var systems: [SystemConfig]
    /// Which site this machine is at, when the setup names any.
    var site: String?
    /// A hint that this machine is normally left on, so it can be relied on as a wake helper. Text
    /// only: nothing acts on it, and nothing is ever kept awake because of it.
    var alwaysOn: Bool?

    private enum CodingKeys: String, CodingKey {
        case id, name, ssh, endpoints, wake, systems, site, alwaysOn
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        name = try container.decodeIfPresent(String.self, forKey: .name) ?? id
        ssh = try container.decodeIfPresent(SSHTarget.self, forKey: .ssh)
        endpoints = try container.decodeIfPresent([Endpoint].self, forKey: .endpoints) ?? []
        wake = try container.decodeIfPresent(WakeConfig.self, forKey: .wake)
        systems = try container.decodeIfPresent([SystemConfig].self, forKey: .systems) ?? []
        site = try container.decodeIfPresent(String.self, forKey: .site)
        alwaysOn = try container.decodeIfPresent(Bool.self, forKey: .alwaysOn)
    }

    /// Built rather than decoded, for the tests and the guided setup.
    init(id: String, name: String? = nil, ssh: SSHTarget? = nil, endpoints: [Endpoint] = [],
         wake: WakeConfig? = nil, systems: [SystemConfig] = [], site: String? = nil,
         alwaysOn: Bool? = nil) {
        self.id = id
        self.name = name ?? id
        self.ssh = ssh
        self.endpoints = endpoints
        self.wake = wake
        self.systems = systems
        self.site = site
        self.alwaysOn = alwaysOn
    }

    func system(id: String) -> SystemConfig? { systems.first { $0.id == id } }

    /// The first configured system running the given platform. This is the second chance a status
    /// gets to be matched: an agent whose system id we do not know still says which platform it is,
    /// and one Linux system in the config is unambiguously the Linux one.
    func system(platform: Platform) -> SystemConfig? { systems.first { $0.platform == platform } }

    /// What to hand to ssh. Either the explicit block or, failing that, the first endpoint: an
    /// address list is written for the phone, but the first entry is still a machine this Mac can
    /// dial, and it beats having nothing to try.
    var sshTarget: SSHTarget? { routes.first?.target }

    /// Every way this Mac could reach the machine, best first.
    ///
    /// The phone has always walked this list; the Mac used to take the first entry and stop, which
    /// meant a machine reachable on the LAN but not on the tailnet read as asleep. Walking it here
    /// is safe because only a command that provably never reached the far side is ever tried on the
    /// next route: see the dispatch rules in the transport.
    ///
    /// A private binding alias goes first because it is an explicit choice on this device. The
    /// deprecated shared alias is only a compatibility route for a machine that has no endpoints;
    /// trying it after configured endpoints fail could run a legacy ProxyCommand that wakes the
    /// machine against the setup's current energy policy.
    var routes: [MachineRoute] { routes(bindings: .empty) }

    /// Every way to reach the machine, with this device's private settings applied.
    ///
    /// The identity file is the thing that used to go missing: a machine with an `ssh` block and a
    /// list of endpoints would dial the endpoints with no key at all, because the key lived on the
    /// block and the endpoints were built without one. Every route inherits it now, from this
    /// device's bindings first and from the deprecated shared field only as a fallback.
    func routes(bindings: Bindings) -> [MachineRoute] {
        var routes: [MachineRoute] = []
        var seen = Set<String>()
        let binding = bindings.binding(forMachine: id)
        let identity = bindings.identityFile(forMachine: id, sharedFallback: ssh?.identityFile)

        func add(_ route: MachineRoute) {
            let key = "\(route.target.destination):\(route.target.port ?? 22)"
            guard seen.insert(key).inserted else { return }
            routes.append(route)
        }

        // A private alias goes first: it names an entry in this device's own ~/.ssh/config, which is
        // where ProxyCommand, jump hosts and everything else the user has already set up lives.
        if let alias = binding?.sshAlias, !alias.isEmpty {
            let target = SSHTarget(host: alias, user: binding?.user, port: binding?.port, identityFile: identity)
            add(MachineRoute(id: "private-alias", label: alias, target: target, systemId: nil,
                             kind: "private-alias"))
        }
        for endpoint in endpoints {
            add(MachineRoute(
                id: endpoint.id,
                label: endpoint.label ?? endpoint.host,
                target: SSHTarget(host: endpoint.host, user: endpoint.user, port: endpoint.port,
                                  identityFile: identity),
                systemId: endpoint.system,
                kind: endpoint.kind
            ))
        }
        if endpoints.isEmpty, let ssh, !ssh.host.isEmpty {
            var target = ssh
            target.identityFile = identity
            add(MachineRoute(id: "shared-alias", label: ssh.display, target: target, systemId: nil,
                             kind: "shared-alias"))
        }
        return routes
    }
}

/// One way to reach a machine: an address, a name for it, and the system it is known to answer as
/// when the config pins one.
struct MachineRoute: Sendable, Equatable, Identifiable {
    var id: String
    var label: String
    var target: SSHTarget
    /// A routing hint for the system usually reachable here. Authenticated status may report another
    /// configured system, so this never authorizes a key or excludes another command shape.
    var systemId: String?
    /// `lan`, `remote`, `alias`, or whatever the config called it.
    var kind: String?

    var isLAN: Bool { kind == "lan" }
    var isRemote: Bool { kind == "remote" }
    var isPrivateAlias: Bool { kind == "private-alias" }
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

    /// Built rather than decoded, for the tests and the guided setup.
    init(id: String? = nil, kind: String? = nil, host: String, port: Int? = nil, user: String? = nil,
         system: String? = nil, label: String? = nil) {
        self.host = host
        self.id = id ?? host
        self.kind = kind
        self.port = port
        self.user = user
        self.system = system
        self.label = label
    }
}

/// Wake on LAN. A machine without this block simply has no Wake button.
struct WakeConfig: Decodable, Sendable, Equatable {
    /// The hardware address a magic packet carries. A block without one is woken only through its
    /// helpers: the schema allows it, and section 9.7 makes it a warning rather than a refusal.
    var mac: String?
    /// The broadcast addresses as the setup lists them, none when it lists none. Which of them a
    /// packet goes to, and what it falls back to, is the plan's to decide (section 10.2): the site's
    /// own broadcast comes before 255.255.255.255, and this used to default to the latter here,
    /// where the plan could no longer tell "none" from "255.255.255.255".
    var broadcast: [String]
    /// The ports as the setup lists them; the plan falls back to 9 and 7.
    var ports: [UInt16]
    /// Not used: readiness is an authenticated status poll, never a TCP probe. Kept for 1.2 setups.
    var probe: Probe?
    /// A machine that names no site falls back to this prefix. Advisory, exactly as a site's
    /// prefixes are.
    var lanPrefix: String?
    /// Machines that can send the packet on this one's behalf, best first.
    ///
    /// One helper is not enough once there is more than one network: a Raspberry Pi in one house
    /// cannot wake a tower in another, whatever the document says. So this is an ordered list, it is
    /// walked until one of them answers, and when none can the app says so plainly instead of
    /// quietly leaving an expensive machine awake to serve as a relay.
    var helpers: [WakeHelper]?
    /// The single-helper form. Written alongside `helpers` so a 1.2 client keeps working, and read
    /// as the only helper when `helpers` is absent.
    var helper: WakeHelper?

    struct Probe: Decodable, Sendable, Equatable {
        var host: String
        var port: UInt16?

        var probePort: UInt16 { port ?? 22 }
    }

    private enum CodingKeys: String, CodingKey {
        case mac, broadcast, ports, probe, lanPrefix, helpers, helper
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        mac = try container.decodeIfPresent(String.self, forKey: .mac)
        broadcast = try container.decodeIfPresent([String].self, forKey: .broadcast) ?? []
        ports = try container.decodeIfPresent([UInt16].self, forKey: .ports) ?? []
        probe = try container.decodeIfPresent(Probe.self, forKey: .probe)
        lanPrefix = try container.decodeIfPresent(String.self, forKey: .lanPrefix)
        helpers = try container.decodeIfPresent([WakeHelper].self, forKey: .helpers)
        helper = try container.decodeIfPresent(WakeHelper.self, forKey: .helper)
    }

    init(mac: String?, broadcast: [String] = [], ports: [UInt16] = [],
         probe: Probe? = nil, lanPrefix: String? = nil, helpers: [WakeHelper]? = nil,
         helper: WakeHelper? = nil) {
        self.mac = mac
        self.broadcast = broadcast
        self.ports = ports
        self.probe = probe
        self.lanPrefix = lanPrefix
        self.helpers = helpers
        self.helper = helper
    }

    /// Every helper, in the order to try them, with the compatibility alias folded in.
    var orderedHelpers: [WakeHelper] {
        if let helpers, !helpers.isEmpty { return helpers }
        return helper.map { [$0] } ?? []
    }

    /// The six bytes of the hardware address, or nil when there is none or it is not one. The schema
    /// refuses a malformed one, so nil here means the block names no address and only its helpers
    /// can wake the machine.
    var macBytes: [UInt8]? {
        guard let mac else { return nil }
        let parts = mac.components(separatedBy: CharacterSet(charactersIn: ":-"))
        let bytes = parts.compactMap { UInt8($0, radix: 16) }
        guard parts.count == 6, bytes.count == 6 else { return nil }
        return bytes
    }
}

/// A machine that can send a magic packet for another one, by running a configured action on it.
///
/// No new wire verb: this is the ordinary `run <action>` command, aimed at a machine that happens to
/// be on the right network. The action itself is the helper's business, and the agent's `wol` action
/// kind means a Pi or a tower can be a helper without any third-party tool installed.
struct WakeHelper: Decodable, Sendable, Equatable, Identifiable {
    /// The id of the machine to run the action on.
    var machine: String
    /// The id of the configured action there.
    var action: String

    var id: String { "\(machine)/\(action)" }

    private enum CodingKeys: String, CodingKey { case machine, action }

    init(machine: String, action: String) {
        self.machine = machine
        self.action = action
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        machine = try container.decode(String.self, forKey: .machine)
        action = try container.decode(String.self, forKey: .action)
    }
}

/// One operating system installed on a machine. A dual boot machine has two of these and only one
/// of them is up at a time.
struct SystemConfig: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    var platform: Platform
    /// The exact argv run over ssh. Quoted for the system's shell on the way out, so a path with a
    /// space in it, or an id with a shell metacharacter in it, goes over as the one word it is.
    var agent: [String]
    /// Overrides the platform's icon with any SF Symbol name.
    var symbol: String?
    /// Which shell the far side hands an ssh command to, as the document spells it. Stock Windows
    /// OpenSSH runs cmd.exe, and that is what a Windows system without one gets; an account whose
    /// `DefaultShell` points at PowerShell says `"shell": "powershell"`. Kept as written, because a
    /// spelling this app does not know is a system it refuses to drive, not one to guess at.
    var shell: String?
    /// True when this system's key is restricted to a forced command through the dispatcher.
    ///
    /// A forced command bypasses the login shell entirely: sshd hands the request to the dispatcher
    /// as `SSH_ORIGINAL_COMMAND` and nothing interprets it as PowerShell or as cmd. So the argv is
    /// written in the dispatcher's own grammar whatever platform the machine is.
    var restricted: Bool?
    /// The grammar this system's command lines are written in (section 15.1 of the contract), or
    /// why it has none, worked out from `platform`, `shell` and `restricted` as the document has
    /// them. A route whose last status reported a restricted session can still move it to the
    /// dispatcher's grammar at run time; nothing moves it anywhere else.
    ///
    /// On a model the app runs, this and `agent` are the launch gate's (LaunchGate.swift): the tuple
    /// this device approved, whatever the document says now.
    var grammar: Result<CommandGrammar, CommandLineRefusal>
    /// Whether this device runs this system's command, and which one. Never read from a document:
    /// a decoded system starts not driven, so the launch gate is the only thing that makes one
    /// runnable, and a transport built from the document as it arrived rather than the gated model
    /// sends nothing. A system built in code rather than decoded is taken as approved.
    var launchState: LaunchState = .approved

    private enum CodingKeys: String, CodingKey {
        case id, name, platform, agent, symbol, shell, restricted
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        platform = try container.decodeIfPresent(Platform.self, forKey: .platform) ?? .linux
        name = try container.decodeIfPresent(String.self, forKey: .name) ?? platform.defaultName
        agent = try container.decodeIfPresent([String].self, forKey: .agent) ?? []
        symbol = try container.decodeIfPresent(String.self, forKey: .symbol)
        shell = try container.decodeIfPresent(String.self, forKey: .shell)
        restricted = try container.decodeIfPresent(Bool.self, forKey: .restricted)
        grammar = CommandGrammar.resolve(platform: platform, shell: shell, restricted: restricted,
                                         restrictedSession: false, agentLength: agent.count)
        launchState = .notDriven
    }

    /// Used by the tests and by the guided setup, which build systems rather than decode them.
    init(id: String, name: String? = nil, platform: Platform = .linux, agent: [String] = [],
         symbol: String? = nil, shell: String? = nil, restricted: Bool? = nil) {
        self.id = id
        self.platform = platform
        self.name = name ?? platform.defaultName
        self.agent = agent
        self.symbol = symbol
        self.shell = shell
        self.restricted = restricted
        self.grammar = CommandGrammar.resolve(platform: platform, shell: shell, restricted: restricted,
                                              restrictedSession: false, agentLength: agent.count)
    }

    var symbolName: String { symbol ?? platform.symbolName }

    /// True when this system's lines are written in the dispatcher's grammar, because its key is
    /// restricted to a forced command. Read from the grammar rather than from `restricted`: on a
    /// gated model that is the approved tuple's grammar (section 13 of the contract), and a peer
    /// clearing `restricted` in the document does not make this device treat the key as a full one.
    var isRestricted: Bool { grammar == .success(.dispatch) }
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

    init(enabled: Bool, name: String, agent: String) {
        self.enabled = enabled
        self.name = name
        self.agent = agent
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
    /// The typed model of a document this device applies, errors and all.
    ///
    /// Section 9.7 keeps the applied document whatever it breaks, as long as section 12 accepts its
    /// bytes, and a machine an error is inside is not driven. So that machine is left out of the
    /// model: nothing dials it and nothing runs on it, and the rest of the setup works as before. An
    /// error outside the machines that stops the model being read at all, a site list that is not
    /// a list, takes the part it is in out of the model too.
    static func applied(_ canonical: Data, errors: [DocumentAcceptance.Problem]) throws -> ControllerConfig {
        var tree = try SetupJSON.parse(canonical, repeats: .lastWins)
        let broken = Set(errors.compactMap(\.machineIndex))
        if !broken.isEmpty, let machines = tree["machines"]?.items {
            tree["machines"] = .array(machines.indices.filter { !broken.contains($0) }.map { machines[$0] })
        }
        let decoder = JSONDecoder()
        if let config = try? decoder.decode(ControllerConfig.self, from: tree.documentData) { return config }
        for problem in errors {
            let key = String(problem.path.prefix { $0 != "." && $0 != "[" })
            if !key.isEmpty, key != "machines", key != "version" { tree[key] = nil }
        }
        return try decoder.decode(ControllerConfig.self, from: tree.documentData)
    }

    /// What section 9.7 does not cover but this app cannot use either: a system with a command and
    /// no grammar to write it in (section 15.1), which is never sent anything.
    func commandWarnings() -> [String] {
        machines.flatMap { machine in
            machine.systems.compactMap { system -> String? in
                guard !system.agent.isEmpty, case .failure(let refusal) = system.grammar else { return nil }
                return "\(system.name) on \(machine.name) is never sent anything. \(refusal.message)"
            }
        }
    }
}

/// Who a setup is, and what it descends from.
///
/// `id` names the setup, not a device: every peer that carries this document carries the same id.
/// `revision` still only goes up, but it is no longer what decides whether a push is accepted, and
/// it cannot be: two devices that both edit revision 5 offline both produce a revision 6, and a
/// number cannot tell "newer" from "different". What can is ancestry, so the document carries the
/// hashes of the documents it was made from and a machine accepts a push only when what it is
/// holding is one of them.
struct ControllerIdentity: Codable, Sendable, Equatable {
    /// The setup id. Stable for the life of the setup, shared by every peer.
    var id: String?
    /// Display name for the setup itself.
    var name: String?
    /// Monotonic within one id: one more than the highest revision it descends from.
    var revision: Int?
    /// When it was written, as ISO 8601. Shown, never compared: clocks disagree.
    var updatedAt: String?
    /// Which kind of client wrote it: mac | desktop | phone | cli | legacy.
    var source: String?
    /// The human name of the device that wrote it.
    var device: String?
    /// The canonical hashes of the documents this one was made from, newest first, at most 32.
    var lineage: [String]?

    init(id: String? = nil, name: String? = nil, revision: Int? = nil, updatedAt: String? = nil,
         source: String? = nil, device: String? = nil, lineage: [String]? = nil) {
        self.id = id
        self.name = name
        self.revision = revision
        self.updatedAt = updatedAt
        self.source = source
        self.device = device
        self.lineage = lineage
    }

    /// How many ancestors a document is allowed to remember. Past this, two branches that are merely
    /// far apart are reported as divergence, which fails towards a question rather than towards a
    /// loss.
    static let lineageLimit = 32

    /// What this client calls itself in `controller.source`.
    static let sourceKind = "mac"

    var revisionNumber: Int { revision ?? 0 }

    var ancestors: [String] { lineage ?? [] }

    /// Whether `hash` is one of the documents this one descends from.
    func descends(from hash: String) -> Bool { ancestors.contains(hash) }

    /// Every hash has to be a sha256 in lower case hex, and there may be no duplicates. A document
    /// that fails this is refused rather than half-trusted, because the whole reconciliation rests
    /// on these strings meaning what they say.
    var lineageIsValid: Bool {
        guard let lineage else { return true }
        guard lineage.count <= Self.lineageLimit else { return false }
        guard Set(lineage).count == lineage.count else { return false }
        return lineage.allSatisfy(AgentToken.isValidHash)
    }

    /// The identity for a document derived from this one.
    func next(parent: String, device: String, now: Date = Date()) -> ControllerIdentity {
        var chain = [parent] + ancestors.filter { $0 != parent }
        if chain.count > Self.lineageLimit { chain = Array(chain.prefix(Self.lineageLimit)) }
        return ControllerIdentity(
            id: id,
            name: name,
            revision: revisionNumber + 1,
            updatedAt: ISO8601DateFormatter.lenient.string(from: now),
            source: Self.sourceKind,
            device: device,
            lineage: chain
        )
    }

    /// Who wrote it, in words, for the divergence sheet.
    var authorDescription: String {
        let who = [device, source].compactMap { $0 }.first ?? "an unknown device"
        guard let when = ISO8601DateFormatter.lenient.date(from: updatedAt) else { return who }
        return "\(who), \(when.formatted(date: .abbreviated, time: .shortened))"
    }
}

/// The controller config as a machine gets it: the bytes, and the sha256 the two sides compare.
///
/// The bytes are canonical, not raw. The agent stores what it is given trimmed with exactly one
/// trailing newline and hashes what it stored, so a file saved without a final newline used to hash
/// one way here and another way on every machine, and the Mac would push the same document every ten
/// minutes forever without ever agreeing with anybody. There is one canonical form and all three
/// implementations use it.
struct ControllerDocument: Sendable, Equatable {
    /// The canonical bytes: the file trimmed of surrounding whitespace, with one trailing newline.
    var bytes: Data
    /// The sha256 of exactly those bytes, lower case hex.
    var hash: String
    var identity: ControllerIdentity
    /// What section 9.7 refuses about it. The applied document is kept with its errors, and a
    /// document with any is not published until they are fixed (section 9.5).
    var errors: [DocumentAcceptance.Problem] = []

    init(bytes: Data, hash: String, identity: ControllerIdentity = ControllerIdentity(),
         errors: [DocumentAcceptance.Problem] = []) {
        self.bytes = bytes
        self.hash = hash
        self.identity = identity
        self.errors = errors
    }

    /// Build one from whatever was read off disk. Throws for bytes that are not valid UTF-8, which
    /// have no canonical form and therefore no hash.
    init(raw: Data, identity: ControllerIdentity = ControllerIdentity()) throws {
        let canonical = try Canonical.bytes(raw)
        self.bytes = canonical
        self.hash = Canonical.sha256(canonical)
        self.identity = identity
    }

    /// The canonicalisation rule lives in one place, next to the vectors that prove it.
    static func canonicalBytes(_ raw: Data) throws -> Data { try Canonical.bytes(raw) }

    static func sha256(_ data: Data) -> String { Canonical.sha256(data) }
}

/// How far the setup has got to one machine, as its section draws it.
///
/// The push itself is never something the user asked for, so this is the only place it is ever
/// mentioned unless it fails: a row that says the machine is holding the same file this Mac is.
enum SetupSharing: Sendable, Equatable {
    /// The machine reports the hash this Mac's file has. Nothing to do.
    case upToDate
    /// The document went over and was accepted, and the machine has not been read since. The next
    /// status turns this into `upToDate`.
    case justShared
    /// An agent from before the setup was shared. It has nowhere to put the document and would
    /// reject the command, so it is left alone.
    case unsupported
    /// The whole sentence to show, ready to read.
    case failed(String)
    /// The machine and this device have both moved on from a common ancestor, or they are carrying
    /// two different setups. Nothing is pushed either way: a fast-forward is safe and automatic,
    /// and everything else is a decision, because a silent overwrite is the one failure nobody
    /// would ever notice.
    case conflict(String)
    /// This device is behind and is fetching what the machine holds.
    case fetching
    /// The machine is behind and is being given this document. Automatic: it descends from what
    /// they hold, so nothing of theirs is lost.
    case publishing
    /// A push the machine refused for a reason trying again cannot fix, kept back until the setup
    /// changes or a person asks. The sentence says why.
    case held(String)
    /// Nothing to say: the machine has not been read, or this Mac has no usable config to share.
    case unknown
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
    /// The document the machines are given: the file in its canonical form, and the sha256 of
    /// exactly those bytes. Recomputed on every load.
    private(set) var bytes: Data?
    private(set) var hash: String?
    /// The raw file, kept for the editor, which has to start from what is actually on disk.
    private(set) var rawBytes: Data?
    /// What section 9.7 refuses about the applied document. It is applied all the same, with every
    /// machine an error is inside left out, and it is not published until these are gone.
    private(set) var errors: [DocumentAcceptance.Problem] = []
    /// What section 9.7 says this app cannot use, for a document with no errors.
    private(set) var warnings: [DocumentAcceptance.Problem] = []

    /// True while the bytes on disk differ from the last document this app applied but still carry
    /// that document's controller block. Until the controller block is advanced, `document` stays
    /// nil so the unrevisioned edit cannot be published to another machine.
    private(set) var hasPendingExternalEdit = false

    /// True while there is a file at the path that is not a setup document: unreadable, refused by
    /// section 12, or not a setup this app can read. The app goes on running on the one it read
    /// before, but that one is no longer what storage holds. A half-saved edit is the person's all
    /// the same, so `document` is nil until the file is a document again and nothing is adopted
    /// over it (M16).
    private(set) var hasBrokenFile = false

    let url: URL
    private let revisionsURL: URL
    private let appliedStateURL: URL

    private struct AppliedState: Codable {
        var hash: String
    }

    @ObservationIgnored private var appliedHash: String?
    @ObservationIgnored private var expectedWriteHash: String?

    @ObservationIgnored private var watcher: DispatchSourceFileSystemObject?
    @ObservationIgnored private var watchedDescriptor: CInt = -1
    @ObservationIgnored private var lastSignature: String?
    /// The bytes the last load read, nil when it could read none. An adoption holds storage to these
    /// rather than to the signature, which a save can leave as it was.
    @ObservationIgnored private var lastRead: Data?

    /// Called after a load that actually changed something, so the app can rebuild its machines.
    var onChange: (@MainActor () -> Void)?

    /// Called with the document an edit made on this device started from, nil when there was none,
    /// and the document it produced, just before that one is written. This is how a local edit
    /// approves the launch values it changed (section 9.9 of the contract), before they can flash up
    /// for review. Nothing that arrives from a peer passes through here.
    var onLocalEdit: (@MainActor (_ before: Data?, _ after: Data) -> Void)?

    /// This device's record of approved launch values: beside the applied-setup state, so it moves
    /// with `LEGION_CONTROL_HOME` exactly as that does.
    var approvalsURL: URL {
        appliedStateURL.deletingLastPathComponent().appending(path: "approved-launch.json")
    }

    init(url: URL? = nil, revisionsDirectory: URL? = nil, appliedStateURL: URL? = nil) {
        let resolvedURL = url ?? Self.defaultURL
        self.url = resolvedURL
        if url == nil {
            self.revisionsURL = revisionsDirectory ?? AppPaths.revisionsDirectory
            self.appliedStateURL = appliedStateURL ?? AppPaths.file("applied-setup.json")
        } else {
            self.revisionsURL = revisionsDirectory ?? resolvedURL.deletingLastPathComponent().appending(path: "revisions")
            self.appliedStateURL = appliedStateURL
                ?? resolvedURL.deletingLastPathComponent().appending(path: "state/applied-setup.json")
        }
        if let data = try? Data(contentsOf: self.appliedStateURL),
           let state = try? JSONDecoder().decode(AppliedState.self, from: data),
           state.hash.count == 64 {
            self.appliedHash = state.hash
        }
        load()
        startWatching()
    }

    deinit {
        watcher?.cancel()
    }

    /// `~/.config/legion-control/config.json`, or wherever `LEGION_CONTROL_HOME` /
    /// `LEGION_CONTROL_CONFIG` point. See AppPaths: an override relocates everything this app
    /// writes, not only the document.
    static var defaultURL: URL { AppPaths.configFile }

    var path: String { url.path(percentEncoded: false) }

    /// The machines to draw, or an empty list when there is nothing usable.
    var machines: [Machine] { config?.machines ?? [] }

    var local: LocalConfig? {
        guard let local = config?.local, local.enabled else { return nil }
        return local
    }

    /// Whether the window has anything at all to show besides the setup page.
    var hasAnything: Bool { !machines.isEmpty || local != nil }

    /// What there is to share, and what an adoption is swapped against: nil while nothing usable has
    /// been read, while a hand edit waits for its revision, and while the file is not a document at
    /// all. A file that was edited into something broken leaves the previous config in force, the
    /// last thing that made sense, but storage no longer holds that document, and section 9.5 has a
    /// device holding only an unrevisioned hand edit decide nothing until it is one.
    var document: ControllerDocument? {
        guard !hasPendingExternalEdit, !hasBrokenFile, let config, let bytes, let hash else { return nil }
        return ControllerDocument(bytes: bytes, hash: hash, identity: config.identity, errors: errors)
    }

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
            lastRead = nil
            isMissing = true
            // Not an error worth showing: no file is the state a fresh install is in, and the setup
            // page says what to do about it. An earlier config is kept, so a file that is being
            // rewritten in place does not blank the window on its way past zero bytes. A file that
            // is there and cannot be read is still somebody's, and nothing is written over it.
            problem = nil
            hasBrokenFile = FileManager.default.fileExists(atPath: path)
            if config != nil, hasBrokenFile { return }
            if config != nil {
                config = nil
                bytes = nil
                hash = nil
                rawBytes = nil
                onChange?()
            }
            return
        }

        lastRead = data
        isMissing = false
        // Section 12 first: bytes it refuses are no document at all, and not even the document this
        // device applies is kept on them. The one read before stays in force.
        if let refusal = Canonical.rejection(data) {
            hasBrokenFile = true
            problem = "The setup file is not a setup document: \(refusal.sentence) This app keeps using the one it read before."
            return
        }
        // Everything else is applied intake (section 9.7): a hand edit, or this device's own file at
        // start, is kept with whatever it breaks. Its errors are shown and stop it being published,
        // and a machine one is inside is not driven, but the rest of the setup goes on working.
        let verdict = DocumentAcceptance.check(data, intake: .applied)
        do {
            let canonical = try Canonical.bytes(data)
            let checked = try ControllerConfig.applied(canonical, errors: verdict.errors)
            problem = nil
            hasBrokenFile = false
            // Before the equality check below: a file that was reformatted without changing anything
            // this app reads is still a different document to the machines, and they compare bytes.
            let document = ControllerDocument(bytes: canonical, hash: Canonical.sha256(canonical),
                                              identity: checked.identity, errors: verdict.errors)
            let previousHash = hash
            rawBytes = data
            bytes = document.bytes
            hash = document.hash
            errors = verdict.errors
            warnings = verdict.warnings
            let changed = checked != config || previousHash != document.hash
            config = checked
            updateAppliedState(for: document)
            if changed { onChange?() }
        } catch let error as DecodingError {
            hasBrokenFile = true
            problem = "The setup file could not be read, so this app keeps using the one it read before. \(Self.describe(error))"
        } catch {
            hasBrokenFile = true
            problem = "The setup file could not be read, so this app keeps using the one it read before. \(error.localizedDescription)"
        }
    }

    /// Compare a loaded file with the private snapshot of the last applied document.
    ///
    /// A changed document with an unchanged controller block is a hand edit. It is held locally until
    /// `reconcileExternalEdit` advances the revision and lineage. A document whose controller block
    /// changed is already an explicit revision (or a new setup id), so it becomes the new baseline.
    private func updateAppliedState(for document: ControllerDocument) {
        if expectedWriteHash == document.hash {
            expectedWriteHash = nil
            hasPendingExternalEdit = false
            markApplied(document.bytes, hash: document.hash)
            return
        }
        guard let appliedHash else {
            hasPendingExternalEdit = false
            markApplied(document.bytes, hash: document.hash)
            return
        }
        guard appliedHash != document.hash else {
            hasPendingExternalEdit = false
            return
        }
        guard let previous = revisionBytes(hash: appliedHash) else {
            hasPendingExternalEdit = true
            problem = "The private copy of the last applied setup is missing. This file will not be published until its ancestry is restored."
            return
        }
        if Self.sameControllerBlock(previous, document.bytes) {
            hasPendingExternalEdit = true
        } else {
            hasPendingExternalEdit = false
            markApplied(document.bytes, hash: document.hash)
        }
    }

    /// Turn a hand edit into a proper child revision by changing only its controller block.
    ///
    /// Called after bindings are available, including during app startup. Unknown JSON elsewhere in
    /// the hand-edited document stays in place because ControllerEditor works on the raw JSON object.
    @discardableResult
    func reconcileExternalEdit(deviceName: String) -> String? {
        guard hasPendingExternalEdit else { return nil }
        guard let parentHash = appliedHash,
              let previous = revisionBytes(hash: parentHash),
              let current = rawBytes,
              let parentConfig = try? JSONDecoder().decode(ControllerConfig.self, from: previous)
        else {
            return problem ?? "The last applied setup could not be read, so the hand edit was not published."
        }
        do {
            let edited = try ControllerEditor.apply(
                to: current,
                parentHash: parentHash,
                parentIdentity: parentConfig.identity,
                deviceName: deviceName
            ) { _ in }
            // A hand edit that breaks a rule stays applied, errors shown, but is not recorded as a
            // revision: writing the identity into a file a person is in the middle of editing is
            // only worth doing once it is a setup every device would accept.
            if let error = DocumentAcceptance.check(edited.bytes, intake: .peer).errors.first {
                throw ConfigProblem("The hand edit was not recorded as a new revision, so it is not shared: \(error.message)")
            }
            // A hand edit that kept the controller block is an edit made on this device, so it
            // approves the launch values it changed since the revision it was made to.
            onLocalEdit?(previous, edited.bytes)
            expectedWriteHash = Canonical.sha256(edited.bytes)
            try AtomicFile.write(edited.bytes, to: url)
            load()
            lastEditDescription = "recorded a hand edit"
            return nil
        } catch let failure as ControllerEditor.EditFailure {
            problem = failure.message
        } catch let configProblem as ConfigProblem {
            problem = configProblem.message
        } catch {
            problem = "The hand edit could not be recorded as a new revision. \(error.localizedDescription)"
        }
        return problem
    }

    /// Whether two documents carry the same controller block, as JSON values: a reformatted block,
    /// or `22.0` written as `22`, is the same block.
    private static func sameControllerBlock(_ lhs: Data, _ rhs: Data) -> Bool {
        func block(_ data: Data) -> SetupJSON? {
            (try? SetupJSON.parse(try Canonical.bytes(data), repeats: .lastWins))?["controller"]
        }
        return SetupJSON.equal(block(lhs), block(rhs))
    }

    private func markApplied(_ bytes: Data, hash: String) {
        guard keepRevision(bytes) != nil else {
            problem = "The applied setup could not be kept in the private revision history, so it will not be published."
            hasPendingExternalEdit = true
            return
        }
        do {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try AtomicFile.write(encoder.encode(AppliedState(hash: hash)), to: appliedStateURL)
            appliedHash = hash
        } catch {
            problem = "The applied setup pointer could not be saved, so later hand edits cannot be published safely. \(error.localizedDescription)"
            hasPendingExternalEdit = true
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
    ///
    /// Read through `FileManager` rather than `URL.resourceValues`, which caches on the URL object:
    /// the store holds one URL for its whole life, so the cached values would be answered forever and
    /// an edit made while the app was open would never be noticed.
    static func signature(of url: URL) -> String? {
        let path = url.path(percentEncoded: false)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: path),
              let modified = attributes[.modificationDate] as? Date
        else { return nil }
        let size = (attributes[.size] as? NSNumber)?.int64Value ?? -1
        return "\(modified.timeIntervalSince1970):\(size)"
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

    // MARK: - Editing and revisions

    /// Superseded documents, one file per canonical hash.
    ///
    /// Keyed by hash rather than by number because that is what a merge needs: to work out which
    /// side changed what, it has to read the exact bytes of the document both sides were made from,
    /// and the only name that identifies those bytes is their hash.
    var revisionsDirectory: URL { revisionsURL }

    struct StoredRevision: Sendable, Equatable, Identifiable {
        var hash: String
        var url: URL
        var savedAt: Date
        var identity: ControllerIdentity?

        var id: String { hash }
        var shortHash: String { String(hash.prefix(12)) }
        var name: String {
            guard let identity else { return shortHash }
            return "revision \(identity.revisionNumber) · \(shortHash)"
        }
    }

    /// Every kept revision, newest first.
    func storedRevisions() -> [StoredRevision] {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: revisionsDirectory,
            includingPropertiesForKeys: [.contentModificationDateKey],
            options: [.skipsHiddenFiles]
        )) ?? []
        return files
            .filter { $0.pathExtension == "json" }
            .map { url in
                let modified = (try? url.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
                let identity = (try? Data(contentsOf: url))
                    .flatMap { try? JSONDecoder().decode(ControllerConfig.self, from: $0) }?
                    .controller
                return StoredRevision(hash: url.deletingPathExtension().lastPathComponent,
                                      url: url, savedAt: modified ?? .distantPast, identity: identity)
            }
            .sorted { $0.savedAt > $1.savedAt }
    }

    /// The exact bytes of one kept revision, for a three-way merge.
    func revisionBytes(hash: String) -> Data? {
        try? Data(contentsOf: revisionsDirectory.appending(path: "\(hash).json"))
    }

    /// The base a merge needs (section 9.8): the newest document both lineages remember whose kept
    /// bytes still hash to the name they are kept under. A revision file that was changed on disk is
    /// not the document it is named after, and a merge built on it would take the wrong side.
    func commonAncestor(mineHash: String, mineLineage: [String], theirsHash: String, theirsLineage: [String]) -> Data? {
        let base = SetupMerge.commonBase(mineHash: mineHash, theirsHash: theirsHash, mineLineage: mineLineage,
                                         theirsLineage: theirsLineage, kept: revisionBytes(hash:))
        return base.flatMap(revisionBytes(hash:))
    }

    /// Keep a copy of a document under its own hash.
    @discardableResult
    func keepRevision(_ bytes: Data) -> String? {
        guard let hash = try? Canonical.hash(bytes) else { return nil }
        let url = revisionsDirectory.appending(path: "\(hash).json")
        guard !FileManager.default.fileExists(atPath: url.path(percentEncoded: false)) else { return hash }
        do {
            try AtomicFile.write(bytes, to: url)
            pruneRevisions()
            return hash
        } catch {
            return nil
        }
    }

    /// Keep the last thirty. A merge needs the common ancestor, not the whole history, and a
    /// directory nobody can read is not a safety net.
    private func pruneRevisions() {
        let kept = storedRevisions()
        guard kept.count > 30 else { return }
        for revision in kept.dropFirst(30) {
            try? FileManager.default.removeItem(at: revision.url)
        }
    }

    /// Apply a structured change to the shared document.
    ///
    /// The edit takes effect here immediately — an offline edit is a first-class edit — and reaches
    /// the machines on the next poll, as a fast-forward from whatever they are holding. Returns nil
    /// on success or a sentence to show.
    ///
    /// An edit approves the launch values it changed, unless `approvesLaunchChanges` is false:
    /// putting a kept revision back is an edit too, but what it restores was written by whoever
    /// wrote that revision, not by the person pressing the button.
    @discardableResult
    func applyEdit(
        describedAs description: String,
        deviceName: String,
        approvesLaunchChanges: Bool = true,
        change: (inout [String: Any]) throws -> Void
    ) -> String? {
        guard let raw = rawBytes ?? (try? Data(contentsOf: url)) else {
            return "There is no config file at \(path) to edit yet."
        }
        do {
            let parent = try Canonical.hash(raw)
            let edited = try ControllerEditor.apply(
                to: raw,
                parentHash: parent,
                parentIdentity: config?.identity ?? ControllerIdentity(),
                deviceName: deviceName,
                change: change
            )
            // Check the result before it replaces anything. An edit must not be what breaks a rule
            // every device holds a setup to: the result would be applied here and published nowhere.
            // One the document broke already does not stop an edit elsewhere, or the one that fixes
            // it. A fault is known by the machine or site it is in rather than by its place in the
            // list, because removing one machine moves every later one's place.
            let tree = { (data: Data) in try? SetupJSON.parse(Canonical.bytes(data), repeats: .lastWins) }
            let before = Set(errors.map { $0.identity(in: tree(raw)) })
            let after = tree(edited.bytes)
            if let added = DocumentAcceptance.check(edited.bytes, intake: .peer).errors.first(where: { !before.contains($0.identity(in: after)) }) {
                throw ConfigProblem(added.message)
            }

            if approvesLaunchChanges { onLocalEdit?(raw, edited.bytes) }
            keepRevision(try Canonical.bytes(raw))
            expectedWriteHash = Canonical.sha256(edited.bytes)
            try AtomicFile.write(edited.bytes, to: url)
            lastEditDescription = description
            load()
            return nil
        } catch let failure as ControllerEditor.EditFailure {
            return failure.message
        } catch let problem as ConfigProblem {
            return "That change would leave the file invalid, so nothing was written. \(problem.message)"
        } catch {
            return "The config could not be written to \(path). \(error.localizedDescription)"
        }
    }

    /// What became of an adoption.
    enum AdoptOutcome: Sendable, Equatable {
        /// The document is applied here, under this hash.
        case adopted(hash: String)
        /// The applied document is no longer the one the adoption was decided against, so nothing
        /// was written. What is applied now, if anything, is `current`.
        case headMoved(current: String?)
        /// Nothing was written, and this says why.
        case refused(String)
    }

    /// Adopt a document read off a machine, or produced by a merge, in place of the one applied.
    ///
    /// A compare-and-swap (section 9.5). The decision to adopt was made against the document
    /// applied then, `expectedHead`, nil when there was none; reading the bytes took a round trip,
    /// and a person may have saved an edit meanwhile. So the file is read again first, and when what
    /// it holds is not the document expected, a hand edit is waiting to be recorded, or it is no
    /// setup document at all, nothing is written: the edit stays, and the next status decides from
    /// it (M16). With nothing expected, a file that is not a document is refused outright rather
    /// than replaced. Take theirs, Keep mine and Merge come through here as well, expecting the
    /// document the divergence sheet was built from.
    ///
    /// The bytes are written exactly as they are, canonicalised and never re-serialised: this device
    /// is not their author, and writing them any other way would change the hash and turn a
    /// fast-forward into a divergence. They are held to section 9.7 under peer intake first.
    /// Adopting approves no launch value: what the document asks this device to run waits for a
    /// person.
    func adopt(_ bytes: Data, describedAs description: String, replacing expectedHead: String?) -> AdoptOutcome {
        adopt(bytes, describedAs: description, replacing: expectedHead, enrolling: false)
    }

    /// Adopt a document as this device's first setup, approving every launch value in it.
    ///
    /// Only for enrolment: a device that applies no setup yet, taking one a person chose after every
    /// launch value in it was shown to them and they confirmed. The approvals are recorded before
    /// the file is written, so nothing it runs flashes up for review. A setup applied meanwhile
    /// makes this a moved head like any other.
    @discardableResult
    func enrol(_ bytes: Data, describedAs description: String) -> AdoptOutcome {
        adopt(bytes, describedAs: description, replacing: nil, enrolling: true)
    }

    private func adopt(_ bytes: Data, describedAs description: String, replacing expectedHead: String?,
                       enrolling: Bool) -> AdoptOutcome {
        reloadIfChanged()
        // The swap is against what storage holds now, not against the last document read cleanly.
        // The signature the poll compares is the modification time and the size, and a save can
        // leave both as they were, so the file is read again and loaded again when it is not what
        // the last load read. Loaded, not only compared: when nothing is written, the next status
        // decides from what is applied here, and from a document storage no longer holds it would
        // only fetch the same bytes and find the head moved, round after round, until the file's
        // signature changed.
        if (try? Data(contentsOf: url)) != lastRead { load() }
        // A half-saved file is kept out of `config`, but it is the person's all the same, and a hand
        // edit not yet recorded as a revision is an edit made after whatever the adoption was
        // decided against. Neither is written over.
        if hasBrokenFile {
            guard expectedHead != nil else {
                return .refused("The file at \(path) is not a setup document, so nothing was written over it: it may be an edit somebody is in the middle of. Fix it in an editor or move it away first.")
            }
            return .headMoved(current: nil)
        }
        if isMissing {
            guard expectedHead == nil else { return .headMoved(current: nil) }
        } else {
            guard !hasPendingExternalEdit, hash == expectedHead else { return .headMoved(current: hash) }
        }
        if let refusal = Canonical.rejection(bytes) {
            return .refused("That setup could not be adopted: \(refusal.sentence)")
        }
        if let error = DocumentAcceptance.check(bytes, intake: .peer).errors.first {
            return .refused("That setup could not be adopted: \(error.message)")
        }
        do {
            let canonical = try Canonical.bytes(bytes)
            if enrolling { onLocalEdit?(nil, canonical) }
            if let raw = rawBytes ?? (try? Data(contentsOf: url)) {
                keepRevision(try Canonical.bytes(raw))
            }
            let adopted = Canonical.sha256(canonical)
            expectedWriteHash = adopted
            try AtomicFile.write(canonical, to: url)
            lastEditDescription = description
            load()
            return .adopted(hash: adopted)
        } catch {
            return .refused("That setup could not be written to \(path). \(error.localizedDescription)")
        }
    }

    /// The last change made from here, for the provenance line. Not persisted: it is about this run.
    private(set) var lastEditDescription: String?

    /// Put a kept revision back.
    ///
    /// Restoring is itself an edit, so it takes the next revision number and lists the current
    /// document as its parent. Winding the counter back would produce a document the machines would
    /// refuse, and rightly: it descends from nothing they hold.
    @discardableResult
    func restore(_ revision: StoredRevision, deviceName: String) -> String? {
        guard let archived = try? Data(contentsOf: revision.url),
              let object = (try? JSONSerialization.jsonObject(with: archived)) as? [String: Any]
        else {
            return "The kept revision \(revision.shortHash) could not be read."
        }
        return applyEdit(describedAs: "restored \(revision.name)", deviceName: deviceName,
                         approvesLaunchChanges: false) { root in
            // Everything is replaced except the identity, which carries on counting up and keeps its
            // lineage, so the machines see an ordinary new revision rather than a rollback.
            let identity = root["controller"]
            root = object
            root["controller"] = identity
        }
    }

    // MARK: - Writing the example

    /// Puts the documented example at the path, and refuses to write over anything. Returns nil on
    /// success or a sentence to show.
    @discardableResult
    func writeExample(deviceName exampleDeviceName: String = "This Mac") -> String? {
        if FileManager.default.fileExists(atPath: path) {
            return "There is already a file at \(path). Open it instead."
        }
        do {
            try FileManager.default.createDirectory(
                at: url.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            // The example is written here, so it is local intake: it gets its setup id now rather
            // than opening as a document with a controller block and no identity.
            let data = try ControllerEditor.adopting(Data(Self.exampleConfig.utf8), deviceName: exampleDeviceName).bytes
            expectedWriteHash = try Canonical.hash(data)
            // The example is this app's own, written because a person asked for it, so what it runs
            // is approved like any other edit made here.
            onLocalEdit?(nil, data)
            try data.write(to: url, options: .atomic)
            load()
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
      "controller": { "name": "My setup" },
      "sites": [
        { "id": "home", "name": "Home", "lanPrefixes": ["10.0.0."], "broadcast": ["10.0.0.255"] }
      ],
      "machines": [
        {
          "id": "workstation",
          "name": "Workstation",
          "site": "home",
          "endpoints": [
            { "id": "tailnet-linux", "kind": "remote", "host": "100.64.0.10", "port": 22, "user": "me", "system": "linux", "label": "Tailnet, Linux" },
            { "id": "tailnet-windows", "kind": "remote", "host": "100.64.0.11", "user": "me", "system": "windows", "label": "Tailnet, Windows" },
            { "id": "lan", "kind": "lan", "host": "10.0.0.40", "user": "me", "label": "Home LAN" }
          ],
          "wake": {
            "mac": "AA:BB:CC:DD:EE:FF",
            "broadcast": ["10.0.0.255"],
            "ports": [9, 7],
            "helper": { "machine": "helper", "action": "wake-workstation" },
            "helpers": [{ "machine": "helper", "action": "wake-workstation" }]
          },
          "systems": [
            { "id": "linux", "name": "Linux", "platform": "linux",
              "agent": ["/home/me/.legion-control/bin/legionctl"] },
            { "id": "windows", "name": "Windows 11", "platform": "windows", "shell": "powershell",
              "agent": ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "C:\\\\Users\\\\me\\\\.legion-control\\\\bin\\\\legionctl.ps1"] }
          ]
        },
        {
          "id": "helper",
          "name": "Always-on helper",
          "site": "home",
          "alwaysOn": true,
          "endpoints": [
            { "id": "lan", "kind": "lan", "host": "10.0.0.20", "user": "me", "label": "Home LAN" }
          ],
          "systems": [
            { "id": "linux", "name": "Linux", "platform": "linux",
              "agent": ["/home/me/.legion-control/bin/legionctl"] }
          ]
        }
      ],
      "appUpdates": { "githubRepo": "x1f4r/legion-control" }
    }

    """

    /// The private settings this device starts with. Written beside the config, never published, and
    /// never containing a key: only where to find one.
    static let exampleBindings = """
    {
      "deviceName": "This Mac",
      "identityFile": "~/.ssh/legion-control_ed25519",
      "machines": {
        "workstation": { "sshAlias": "workstation" }
      }
    }

    """

}
