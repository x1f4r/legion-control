import Foundation
import Testing
@testable import LegionControlCore

/// Waking a machine from wherever this device happens to be.
///
/// A magic packet is a broadcast on one network, so the interesting cases are all about being
/// somewhere else: which helper to ask, in what order, and what to say when none of them can. The
/// shared wake vectors pin the plan itself (ContractVectorTests); these name the rules a person
/// would get wrong, in this app's own types.
struct WakeTests {

    static let setup = """
    {
      "version": 1,
      "sites": [
        { "id": "attic", "name": "Attic house", "lanPrefixes": ["192.168.50."], "broadcast": ["192.168.50.255"] },
        { "id": "flat", "name": "New flat", "lanPrefixes": ["192.168.50."] },
        { "id": "cabin", "name": "Cabin" }
      ],
      "machines": [
        { "id": "pi", "name": "Pi", "site": "attic", "alwaysOn": true,
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.168.50.5", "user": "me" } ],
          "wake": { "mac": "02:00:00:00:00:01" },
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "nas", "name": "NAS", "site": "attic",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.168.50.9", "user": "me" } ],
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "tower", "name": "Tower", "site": "flat",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.168.50.20", "user": "me" } ],
          "wake": { "mac": "02:00:00:00:00:02",
                    "helpers": [ { "machine": "pi", "action": "wake-tower" },
                                 { "machine": "nas", "action": "wake-tower" } ] },
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "shed", "name": "Shed", "site": "cabin",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "shed.example.net", "user": "me" } ],
          "wake": { "mac": "02:00:00:00:00:03" },
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] }
      ]
    }
    """

    static func config() throws -> ControllerConfig {
        try ControllerConfig.accepted(Data(setup.utf8))
    }

    static func here(_ addresses: [String], confirmed: String? = nil) throws -> SiteAwareness.Placement {
        SiteAwareness.placement(sites: try config().allSites, addresses: addresses.compactMap(LocalAddress.init), confirmed: confirmed)
    }

    static let wol = WakePlan.HelperState(.ready, actionKinds: ["wake-tower": "wol"])
    static let command = WakePlan.HelperState(.ready, actionKinds: ["wake-tower": "command"])

    static func helpers(_ plan: WakePlan.Plan) -> [String] {
        plan.steps.compactMap { step in
            if case .helper(let helper, _, _) = step { return helper.machine }
            return nil
        }
    }

    // MARK: - Where this device is

    @Test("two sites on the same private range cannot be told apart, and neither is chosen")
    func overlappingSubnets() throws {
        // Stock routers hand out the same range in every house. Picking the first match would
        // broadcast into the wrong building, so an ambiguous answer behaves as though this device
        // were somewhere else entirely — which works either way.
        let placement = try Self.here(["192.168.50.42/24"])
        #expect(placement.matching == ["attic", "flat"])
        #expect(placement.isAmbiguous)
        #expect(placement.effective == nil)
        #expect(placement.description(sites: try Self.config().allSites).contains("Overlapping private ranges"))
    }

    @Test("the user saying which site this is settles it, while the addresses agree")
    func confirmedSite() throws {
        let placement = try Self.here(["192.168.50.42/24"], confirmed: "flat")
        #expect(placement.effective == "flat")
        #expect(!placement.isAmbiguous)
        #expect(placement.description(sites: try Self.config().allSites).contains("confirmed"))
    }

    @Test("a chosen site the addresses do not support is not taken")
    func confirmedButNotMatching() throws {
        // It used to win outright, so a laptop set to its home site broadcast at home's network
        // from anywhere at all (C8).
        let placement = try Self.here(["198.51.100.3/24"], confirmed: "flat")
        #expect(placement.effective == nil)
        #expect(placement.description(sites: try Self.config().allSites).contains("Site unconfirmed"))
        // A site with no prefixes is taken on the person's word, and a site nobody has is ignored.
        #expect(try Self.here(["198.51.100.3/24"], confirmed: "cabin").effective == "cabin")
        let nowhere = try Self.here(["192.168.50.42/24"], confirmed: "nowhere")
        #expect(nowhere.effective == nil)
        #expect(nowhere.isAmbiguous)
    }

    @Test("one matching site is enough to act on, and is still not called proof")
    func singleMatch() throws {
        let sites = [Site(id: "home", name: "Home", lanPrefixes: ["10.0.0."], broadcast: ["10.0.0.255"])]
        let placement = SiteAwareness.placement(sites: sites, addresses: [LocalAddress("10.0.0.7/24")!], confirmed: nil)
        #expect(placement.effective == "home")
        #expect(placement.description(sites: sites).contains("hint, not proof"))
    }

    @Test("a prefix matches on octet and bit boundaries, never as text")
    func prefixesAreNotText() throws {
        // `192.168.1` used to take 192.168.10.7 because the address text starts with it, and a
        // device two subnets away believed it was at home (C8, A9).
        let sites = [Site(id: "home", lanPrefixes: ["192.168.1"]), Site(id: "office", lanPrefixes: ["10.0.0.0/20"])]
        #expect(SiteAwareness.placement(sites: sites, addresses: [LocalAddress("192.168.10.7/24")!], confirmed: nil).matching.isEmpty)
        #expect(SiteAwareness.placement(sites: sites, addresses: [LocalAddress("192.168.1.7/24")!], confirmed: nil).matching == ["home"])
        #expect(SiteAwareness.placement(sites: sites, addresses: [LocalAddress("10.0.10.7/24")!], confirmed: nil).matching == ["office"])
        #expect(SiteAwareness.placement(sites: sites, addresses: [LocalAddress("10.0.16.7/24")!], confirmed: nil).matching.isEmpty)
        // A prefix that is no prefix never matches anything.
        let broken = [Site(id: "x", lanPrefixes: ["192.168.1.5", "192.168.*"])]
        #expect(SiteAwareness.placement(sites: broken, addresses: [LocalAddress("192.168.1.5/24")!], confirmed: nil).matching.isEmpty)
    }

    @Test("this device's own addresses: loopback is on no network, and a mask gives the directed broadcast")
    func localAddresses() throws {
        #expect(LocalAddress("127.0.0.1/8") == nil)
        #expect(LocalAddress("127.10.0.1") == nil)
        #expect(LocalAddress("192.168.1.256/24") == nil)
        #expect(LocalAddress("192.168.1.7/33") == nil)
        #expect(LocalAddress("192.168.1.7/024") == nil)
        #expect(LocalAddress("192.168.1.7/24")?.directedBroadcast == "192.168.1.255")
        #expect(LocalAddress("10.0.16.9/20")?.directedBroadcast == "10.0.31.255")
        // Without a length, or with too few host bits, there is nothing to derive.
        #expect(LocalAddress("192.168.1.7")?.directedBroadcast == nil)
        #expect(LocalAddress("192.168.1.7/31")?.directedBroadcast == nil)
        #expect(LocalAddress.prefixLength(ofMask: 0xFFFF_FF00) == 24)
        #expect(LocalAddress.prefixLength(ofMask: 0xFFFF_FFFF) == 32)
        #expect(LocalAddress.prefixLength(ofMask: 0) == 0)
        #expect(LocalAddress.prefixLength(ofMask: 0xFF00_FF00) == nil)
        // The interfaces of the Mac the tests run on, whatever they are: never a loopback address.
        #expect(!SiteAwareness.localAddresses().contains { $0.address >> 24 == 127 })
    }

    // MARK: - The plan

    @Test("on the machine's own network the site's broadcast comes before the limited one")
    func directSendsToTheSiteBroadcast() throws {
        // A tower that names no broadcast of its own used to be sent 255.255.255.255, which a
        // router may drop, while its site named the address that reaches it (C8).
        let config = try Self.config()
        let pi = try #require(config.machine(id: "pi"))
        let plan = WakePlan.plan(for: pi, in: config, placement: try Self.here(["192.168.50.42/24"], confirmed: "attic"),
                                 helpers: [:], holds: [])
        #expect(plan.steps == [.direct(broadcasts: ["192.168.50.255"], ports: [9, 7])])
    }

    @Test("with no broadcast anywhere, the placing address gives one before the limited broadcast")
    func directedBroadcast() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(for: tower, in: config, placement: try Self.here(["192.168.50.42/24"], confirmed: "flat"),
                                 helpers: ["pi": Self.wol, "nas": Self.wol], holds: [])
        #expect(plan.steps.first == .direct(broadcasts: ["192.168.50.255"], ports: [9, 7]))
        let shed = try #require(config.machine(id: "shed"))
        let anywhere = WakePlan.plan(for: shed, in: config, placement: try Self.here(["198.51.100.7/24"], confirmed: "cabin"),
                                     helpers: [:], holds: [])
        #expect(anywhere.steps == [.direct(broadcasts: ["255.255.255.255"], ports: [9, 7])])
    }

    @Test("off-site the helpers are walked in the order the setup lists them")
    func helpersInOrder() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(for: tower, in: config, placement: try Self.here(["198.51.100.3/24"]),
                                 helpers: ["pi": Self.wol, "nas": Self.command], holds: [])
        #expect(Self.helpers(plan) == ["pi", "nas"])
        #expect(plan.skipped.map(\.reason) == [.elsewhere])
        #expect(plan.skipped.first?.why.contains("not on New flat's network") == true)
    }

    @Test("a helper that is asleep is skipped, and waking it is offered rather than done")
    func asleepHelperIsSkippedNotWoken() throws {
        // A cascade would turn the machine the user is trying not to run into one that runs whenever
        // anything else needs waking, which is the opposite of what was asked for.
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(for: tower, in: config, placement: try Self.here(["198.51.100.3/24"]),
                                 helpers: ["pi": .init(.asleep), "nas": Self.command], holds: [])
        #expect(Self.helpers(plan) == ["nas"])
        let skipped = try #require(plan.skipped.first { $0.machine == "pi" })
        #expect(skipped.reason == .asleep)
        #expect(skipped.why.contains("always on"))
        // Offered as a separate step, because the Pi has a wake block of its own; the NAS has none.
        #expect(plan.offerWakeFirst == ["pi"])
    }

    @Test("a helper nothing here can drive is skipped with the real reason")
    func unusableHelpers() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let rejected = WakePlan.HelperState(after: AgentFailure(.authenticationFailed, dispatch: .never))
        #expect(rejected.state == .unusable)
        let plan = WakePlan.plan(for: tower, in: config, placement: try Self.here(["198.51.100.3/24"]),
                                 helpers: ["pi": rejected, "nas": .init(.notDriven, note: "none of its systems is approved here.")], holds: [])
        #expect(plan.steps.isEmpty)
        #expect(plan.skipped.map(\.reason) == [.elsewhere, .unusable, .unusable])
        #expect(plan.skipped.contains { $0.why.contains("key rejected") })
        #expect(plan.nothingToTry.contains("Nothing here can wake it"))
        // A failure that says nothing about the helper leaves it to be read just before its step.
        #expect(WakePlan.HelperState(after: AgentFailure(.linkLost, dispatch: .unknown)).state == .unknown)
        #expect(WakePlan.HelperState(after: AgentFailure(.noRoute, dispatch: .never)).state == .asleep)
    }

    @Test("a helper with no reading yet is a step, read just before its turn")
    func unreadHelperIsNotAsleep() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(for: tower, in: config, placement: .nowhere, helpers: [:], holds: [])
        #expect(plan.steps.first == .helper(WakeHelper(machine: "pi", action: "wake-tower"), machineName: "Pi", readFirst: true))
    }

    @Test("a helper that does not offer the action, or is listed twice, is not asked")
    func noActionAndDuplicates() throws {
        let document = """
        { "version": 1, "machines": [
          { "id": "pi", "endpoints": [{"id":"e","host":"pi.example.net","user":"me"}],
            "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] },
          { "id": "tv", "endpoints": [{"id":"e","host":"tv.example.net","user":"me"}],
            "wake": { "mac": "02:00:00:00:00:07", "helpers": [ { "machine": "pi", "action": "wake-tv" }, { "machine": "pi", "action": "wake-tv" } ] },
            "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let config = try ControllerConfig.accepted(Data(document.utf8))
        let tv = try #require(config.machine(id: "tv"))
        let plan = WakePlan.plan(for: tv, in: config, placement: .nowhere, helpers: ["pi": Self.wol], holds: [])
        #expect(plan.steps.isEmpty)
        #expect(plan.skipped.map(\.reason) == [.unplaced, .noAction, .duplicate])
    }

    @Test("while a hold stands only a declared magic packet runs, the held helper's included")
    func holdsLetOnlyWolThrough() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let held = [WakePlan.Hold(helper: "nas", action: "wake-tower")]
        let plan = WakePlan.plan(for: tower, in: config, placement: try Self.here(["198.51.100.3/24"]),
                                 helpers: ["pi": Self.wol, "nas": Self.command], holds: held)
        #expect(Self.helpers(plan) == ["pi"])
        #expect(plan.skipped.last?.reason == .held)
        // An unread helper cannot be shown to be exempt, so it waits too; the direct step does not.
        let unread = WakePlan.plan(for: tower, in: config, placement: try Self.here(["192.168.50.42/24"], confirmed: "flat"),
                                   helpers: [:], holds: held)
        #expect(unread.steps == [.direct(broadcasts: ["192.168.50.255"], ports: [9, 7])])
        #expect(unread.skipped.map(\.reason) == [.held, .held])
    }

    @Test("an ambiguous site does not send a broadcast into the wrong house")
    func ambiguousSiteUsesHelpers() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(for: tower, in: config, placement: try Self.here(["192.168.50.42/24"]),
                                 helpers: ["pi": Self.wol, "nas": Self.wol], holds: [])
        #expect(!plan.steps.contains { if case .direct = $0 { return true } else { return false } })
        #expect(plan.skipped.first?.reason == .ambiguous)
        #expect(plan.skipped.first?.why.contains("more than one of the configured sites") == true)
    }

    @Test("a wake block without a hardware address is woken only through its helpers")
    func noHardwareAddress() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "x", "endpoints": [{"id":"e","host":"h","user":"me"}],
          "wake": { "broadcast": ["10.0.0.255"], "lanPrefix": "10.0.0." },
          "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        // The schema allows it, so it is accepted with a warning rather than refused.
        #expect(DocumentAcceptance.check(Data(json.utf8), intake: .peer).warnings.map(\.code) == ["wake-no-mac"])
        let config = try ControllerConfig.accepted(Data(json.utf8))
        let machine = try #require(config.machine(id: "x"))
        let plan = WakePlan.plan(for: machine, in: config, placement: SiteAwareness.placement(
            sites: [], addresses: [LocalAddress("10.0.0.7/24")!], confirmed: nil), helpers: [:], holds: [])
        #expect(plan.steps.isEmpty)
        #expect(plan.skipped.map(\.reason) == [.noMac, .none])
        #expect(plan.skipped.first?.why.contains("no hardware address") == true)
    }

    @Test("the old per-machine LAN prefix places a machine with no site, as a prefix")
    func lanPrefixFallback() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "x", "endpoints": [{"id":"e","host":"h","user":"me"}],
          "wake": { "mac": "02:00:00:00:00:04", "lanPrefix": "10.0.1", "ports": [9] },
          "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let config = try ControllerConfig.accepted(Data(json.utf8))
        let machine = try #require(config.machine(id: "x"))
        func plan(_ address: String) -> WakePlan.Plan {
            WakePlan.plan(for: machine, in: config, placement: SiteAwareness.placement(
                sites: [], addresses: [LocalAddress(address)!], confirmed: nil), helpers: [:], holds: [])
        }
        #expect(plan("10.0.1.7/24").steps == [.direct(broadcasts: ["10.0.1.255"], ports: [9])])
        // The text of 10.0.10.12 starts with "10.0.1", and the shared plans cannot show it: their
        // only dot-less prefix ends in 0, where text happens to agree. The octet boundary decides.
        #expect(plan("10.0.10.12/24").skipped.first?.reason == .elsewhere)
        #expect(plan("10.0.10.12/24").steps.isEmpty)
    }

    @Test("readiness polls start at 5 s and back off by 8/5 to 20 s, for two minutes after a send")
    func readinessSchedule() {
        #expect(WakePlan.readinessSchedule(sentAnything: false) == [])
        #expect(WakePlan.readinessSchedule(sentAnything: true) == [5000, 13000, 25800, 45800, 65800, 85800, 105800])
    }

    // MARK: - Writing prefixes

    @Test("an editor writes prefixes the way every client reads them, and refuses what is no prefix")
    func editorPrefixes() throws {
        let site = try ControllerEditor.siteEntry(id: "home", name: "Home",
                                                  prefixes: ["192.168.1", "10.0.0.0/8", "10.20.0.0/16", "172.16.0.0/12", "192.168.1.5/32"],
                                                  broadcast: [])
        #expect(site["lanPrefixes"] as? [String] == ["192.168.1.", "10.", "10.20.", "172.16.0.0/12", "192.168.1.5/32"])
        #expect(site["broadcast"] is NSNull)
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.siteEntry(id: "home", name: "Home", prefixes: ["192.168.1.5"], broadcast: [])
        }
        // An empty list is never written: the schema refuses an empty lanPrefixes.
        var root: [String: Any] = ["version": 1, "machines": []]
        try ControllerEditor.upsertSite(try ControllerEditor.siteEntry(id: "cabin", name: "Cabin", prefixes: [], broadcast: []), in: &root)
        let written = try #require((root["sites"] as? [[String: Any]])?.first)
        #expect(Set(written.keys) == ["id", "name"])
    }

    // MARK: - The packet itself

    @Test("the agent says which actions are magic packets and which are arbitrary commands")
    func actionKindIsDecoded() throws {
        let json = """
        { "ok": true, "contract": 3, "actions": [
          { "id": "wake-tower", "name": "Wake Tower", "kind": "wol" },
          { "id": "restart-sunshine", "name": "Restart Sunshine", "kind": "command" },
          { "id": "old", "name": "From an older agent" } ] }
        """
        let status = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
        let actions = try #require(status.actions)
        #expect(actions[0].isDeclaredWakePacket)
        #expect(!actions[1].isDeclaredWakePacket)
        // An agent that says nothing is not assumed to be sending a packet.
        #expect(!actions[2].isDeclaredWakePacket)
        #expect(WakePlan.HelperState.kinds(actions) == ["wake-tower": "wol", "restart-sunshine": "command", "old": ""])
    }

    @Test("the magic packet is six 0xFF bytes then the address sixteen times")
    func magicPacket() {
        let bytes: [UInt8] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]
        let packet = WakeOnLAN.magicPacket(for: bytes)
        #expect(packet.count == 102)
        #expect(Array(packet.prefix(6)) == [UInt8](repeating: 0xFF, count: 6))
        #expect(Array(packet.suffix(6)) == bytes)
        #expect(WakeOnLAN.repeats == 3)
    }

    @Test("a hardware address that is not six bytes is refused before anything is sent")
    func macValidation() throws {
        #expect(WakeConfig(mac: "02:00:00:00:00:01").macBytes?.count == 6)
        #expect(WakeConfig(mac: "02-00-00-00-00-01").macBytes?.count == 6)
        #expect(WakeConfig(mac: "not an address").macBytes == nil)
        #expect(WakeConfig(mac: "02:00:00:00:00").macBytes == nil)
    }

    @Test("an absent broadcast and port list stay absent, for the plan to fall back from")
    func wakeBlockKeepsWhatItSays() throws {
        let wake = try JSONDecoder().decode(WakeConfig.self, from: Data(#"{"mac":"02:00:00:00:00:01"}"#.utf8))
        #expect(wake.broadcast.isEmpty)
        #expect(wake.ports.isEmpty)
        #expect(WakePlan.ports(for: wake) == [9, 7])
        #expect(WakePlan.broadcasts(for: wake, site: nil, placing: nil) == ["255.255.255.255"])
        let named = try JSONDecoder().decode(WakeConfig.self, from: Data(#"{"broadcast":["studio.local","192.168.50.255"],"ports":[9]}"#.utf8))
        #expect(WakePlan.broadcasts(for: named, site: nil, placing: nil) == ["192.168.50.255"])
    }
}
