import Foundation
import Testing
@testable import LegionControlCore

/// Waking a machine from wherever this device happens to be.
///
/// A magic packet is a broadcast on one network, so the interesting cases are all about being
/// somewhere else: which helper to ask, in what order, and what to say when none of them can.
struct WakeTests {

    static let setup = """
    {
      "version": 1,
      "sites": [
        { "id": "attic", "name": "Attic house", "lanPrefixes": ["192.168.178."], "broadcast": ["192.168.178.255"] },
        { "id": "flat", "name": "New flat", "lanPrefixes": ["192.168.178."], "broadcast": ["192.168.178.255"] }
      ],
      "machines": [
        { "id": "pi", "name": "Pi", "site": "attic", "alwaysOn": true,
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.168.178.5" } ],
          "wake": { "mac": "AA:BB:CC:DD:EE:01" },
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "legion", "name": "Legion", "site": "attic",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.168.178.9" } ],
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "tower", "name": "Tower", "site": "flat",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.168.178.20" } ],
          "wake": { "mac": "AA:BB:CC:DD:EE:FF", "broadcast": ["192.168.178.255"],
                    "helpers": [ { "machine": "pi", "action": "wake-tower" },
                                 { "machine": "legion", "action": "wake-tower" } ] },
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] }
      ]
    }
    """

    static func config() throws -> ControllerConfig {
        try JSONDecoder().decode(ControllerConfig.self, from: Data(setup.utf8)).validated()
    }

    static let awake = WakePlan.HelperState(isAwake: true, isCommandable: true, canBeWoken: false, lastCheckedAt: Date())
    static let asleep = WakePlan.HelperState(isAwake: false, isCommandable: false, canBeWoken: true, lastCheckedAt: Date())
    static let noAgent = WakePlan.HelperState(isAwake: true, isCommandable: false, canBeWoken: false, lastCheckedAt: Date())

    // MARK: - Where this device is

    @Test("two sites on the same private range cannot be told apart, and neither is chosen")
    func overlappingSubnets() {
        // Stock routers hand out the same range in every house. Picking the first match would
        // broadcast into the wrong building, so an ambiguous answer behaves as though this device
        // were somewhere else entirely — which works either way.
        let config = try! Self.config()
        let placement = SiteAwareness.placement(sites: config.allSites,
                                                addresses: ["192.168.178.42"], confirmed: nil)
        #expect(placement.matching.sorted() == ["attic", "flat"])
        #expect(placement.isAmbiguous)
        #expect(placement.effective == nil)
        #expect(placement.description(sites: config.allSites).contains("Overlapping private ranges"))
    }

    @Test("the user saying which site this is settles it")
    func confirmedSite() {
        let config = try! Self.config()
        let placement = SiteAwareness.placement(sites: config.allSites,
                                                addresses: ["192.168.178.42"], confirmed: "flat")
        #expect(placement.effective == "flat")
        #expect(!placement.isAmbiguous)
        #expect(placement.description(sites: config.allSites).contains("confirmed"))
    }

    @Test("a confirmed site the addresses do not support is shown as unconfirmed rather than trusted")
    func confirmedButNotMatching() {
        let config = try! Self.config()
        let placement = SiteAwareness.placement(sites: config.allSites,
                                                addresses: ["100.64.0.3"], confirmed: "flat")
        #expect(placement.effective == "flat")
        #expect(placement.description(sites: config.allSites).contains("Site unconfirmed"))
    }

    @Test("one matching site is enough to act on, and is still not called proof")
    func singleMatch() {
        let sites = [Site(id: "attic", name: "Attic", lanPrefixes: ["10.0.0."], broadcast: ["10.0.0.255"])]
        let placement = SiteAwareness.placement(sites: sites, addresses: ["10.0.0.7"], confirmed: nil)
        #expect(placement.effective == "attic")
        #expect(placement.description(sites: sites).contains("hint, not proof"))
    }

    // MARK: - The plan

    @Test("on the machine's own network the packet is sent from here and no helper is contacted")
    func directWhenOnSite() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(
            for: tower, in: config,
            placement: SiteAwareness.Placement(matching: ["flat"], confirmed: "flat"),
            addresses: ["192.168.178.42"],
            helperStates: ["pi": Self.awake, "legion": Self.awake]
        )
        guard case .direct(let broadcasts, _)? = plan.steps.first else {
            Issue.record("expected a direct send first")
            return
        }
        #expect(broadcasts == ["192.168.178.255"])
    }

    @Test("off-site the helpers are walked in the order the setup lists them")
    func helpersInOrder() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(
            for: tower, in: config,
            placement: SiteAwareness.Placement(matching: [], confirmed: nil),
            addresses: ["100.64.0.3"],
            helperStates: ["pi": Self.awake, "legion": Self.awake]
        )
        let helpers = plan.steps.compactMap { step -> String? in
            if case .helper(let helper, _) = step { return helper.machine }
            return nil
        }
        #expect(helpers == ["pi", "legion"])
        #expect(plan.unavailable.contains { $0.reason.contains("not on") })
    }

    @Test("a helper that is asleep is skipped, and waking it is offered rather than done")
    func asleepHelperIsSkippedNotWoken() throws {
        // A cascade would turn the machine the user is trying not to run into one that runs whenever
        // anything else needs waking, which is the opposite of what was asked for.
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(
            for: tower, in: config,
            placement: SiteAwareness.Placement(matching: [], confirmed: nil),
            addresses: ["100.64.0.3"],
            helperStates: ["pi": Self.asleep, "legion": Self.awake]
        )
        let helpers = plan.steps.compactMap { step -> String? in
            if case .helper(let helper, _) = step { return helper.machine }
            return nil
        }
        #expect(helpers == ["legion"])
        let skipped = try #require(plan.unavailable.first { $0.what == "Pi" })
        #expect(skipped.reason.contains("always on"))
        // Offered as a separate step, because the Pi has a wake block of its own.
        #expect(skipped.wakeableFirst == "Pi")
    }

    @Test("a helper that is awake without its agent is skipped with the real reason")
    func helperWithoutAgent() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(
            for: tower, in: config,
            placement: SiteAwareness.Placement(matching: [], confirmed: nil),
            addresses: ["100.64.0.3"],
            helperStates: ["pi": Self.noAgent, "legion": Self.asleep]
        )
        #expect(plan.steps.isEmpty)
        #expect(plan.unavailable.contains { $0.reason.contains("control agent is not answering") })
        #expect(plan.nothingToTry.contains("Nothing here can wake it"))
    }

    @Test("a helper with no reading yet is attempted rather than reported asleep")
    func unreadHelperIsNotAsleep() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(for: tower, in: config,
                                 placement: SiteAwareness.Placement(matching: [], confirmed: nil),
                                 addresses: [], helperStates: ["pi": .init(isAwake: false, isCommandable: false,
                                                                          canBeWoken: true, lastCheckedAt: nil)])
        #expect(plan.steps.contains { if case .helper(let helper, _) = $0 { return helper.machine == "pi" }; return false })
    }

    @Test("an ambiguous site does not send a broadcast into the wrong house")
    func ambiguousSiteUsesHelpers() throws {
        let config = try Self.config()
        let tower = try #require(config.machine(id: "tower"))
        let plan = WakePlan.plan(
            for: tower, in: config,
            placement: SiteAwareness.Placement(matching: ["attic", "flat"], confirmed: nil),
            addresses: ["192.168.178.42"],
            helperStates: ["pi": Self.awake, "legion": Self.awake]
        )
        #expect(!plan.steps.contains { if case .direct = $0 { return true } else { return false } })
        #expect(plan.unavailable.contains { $0.reason.contains("more than one of the configured sites") })
    }

    @Test("a machine with a wake block, no site and no helper says so plainly")
    func nothingCanWakeIt() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "x", "endpoints": [{"id":"e","host":"h"}],
          "wake": { "mac": "AA:BB:CC:DD:EE:FF" },
          "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let config = try JSONDecoder().decode(ControllerConfig.self, from: Data(json.utf8)).validated()
        let machine = try #require(config.machine(id: "x"))
        let plan = WakePlan.plan(for: machine, in: config,
                                 placement: SiteAwareness.Placement(matching: [], confirmed: nil),
                                 addresses: ["100.64.0.3"], helperStates: [:])
        #expect(plan.isEmpty)
        #expect(!plan.nothingToTry.isEmpty)
    }

    @Test("the old per-machine LAN prefix still works for a machine with no site")
    func lanPrefixFallback() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "x", "endpoints": [{"id":"e","host":"h"}],
          "wake": { "mac": "AA:BB:CC:DD:EE:FF", "broadcast": ["10.0.0.255"], "lanPrefix": "10.0.0." },
          "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let config = try JSONDecoder().decode(ControllerConfig.self, from: Data(json.utf8)).validated()
        let machine = try #require(config.machine(id: "x"))
        let plan = WakePlan.plan(for: machine, in: config,
                                 placement: SiteAwareness.Placement(matching: [], confirmed: nil),
                                 addresses: ["10.0.0.7"], helperStates: [:])
        guard case .direct(let broadcasts, _)? = plan.steps.first else {
            Issue.record("expected a direct send")
            return
        }
        #expect(broadcasts == ["10.0.0.255"])
    }

    // MARK: - Failing over safely

    @Test("only a declared magic packet may be followed by another helper after an unknown outcome")
    func failoverRespectsOperationIdentity() {
        let unknown = AgentFailure(.timedOut(seconds: 30), detail: "", dispatch: .unknown)
        let never = AgentFailure(.connectionRefused, detail: "", dispatch: .never)

        // A `wol` action is an idempotent UDP send: repeating it costs three datagrams.
        #expect(WakePlan.mayTryNextHelper(after: unknown, actionIsDeclaredWakePacket: true))
        // A general command may do anything, and "the link dropped" must not turn one requested
        // action into two performed ones.
        #expect(!WakePlan.mayTryNextHelper(after: unknown, actionIsDeclaredWakePacket: false))
        // Nothing ran, so anything may be tried.
        #expect(WakePlan.mayTryNextHelper(after: never, actionIsDeclaredWakePacket: false))
    }

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
    }

    // MARK: - The packet itself

    @Test("the magic packet is six 0xFF bytes then the address sixteen times")
    func magicPacket() {
        let bytes: [UInt8] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]
        let packet = WakeOnLAN.magicPacket(for: bytes)
        #expect(packet.count == 102)
        #expect(Array(packet.prefix(6)) == [UInt8](repeating: 0xFF, count: 6))
        #expect(Array(packet.suffix(6)) == bytes)
    }

    @Test("a hardware address that is not six bytes is refused before anything is sent")
    func macValidation() throws {
        let good = WakeConfig(mac: "AA:BB:CC:DD:EE:FF")
        #expect(good.macBytes?.count == 6)
        #expect(WakeConfig(mac: "AA-BB-CC-DD-EE-FF").macBytes?.count == 6)
        #expect(WakeConfig(mac: "not an address").macBytes == nil)
        #expect(WakeConfig(mac: "AA:BB:CC:DD:EE").macBytes == nil)
    }
}
