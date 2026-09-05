import Foundation
import Testing
@testable import LegionControlCore

/// Putting two diverged setups back together without losing either side's work.
///
/// The scenario these are written against is the one the amendment calls A2: two devices edit the
/// same revision while apart, both publish, and the result has to contain both changes.
struct SetupMergeTests {

    static func json(_ object: [String: Any]) throws -> Data {
        try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    static func machine(_ id: String, name: String, endpoints: [[String: Any]] = [],
                        extra: [String: Any] = [:]) -> [String: Any] {
        var entry: [String: Any] = [
            "id": id,
            "name": name,
            "endpoints": endpoints,
            "systems": [["id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"]] as [String: Any]]
        ]
        for (key, value) in extra { entry[key] = value }
        return entry
    }

    static func document(machines: [[String: Any]], extra: [String: Any] = [:]) throws -> Data {
        var root: [String: Any] = ["version": 1, "machines": machines]
        for (key, value) in extra { root[key] = value }
        return try json(root)
    }

    // MARK: - Differences

    @Test("an entry only one side changed is taken from that side without a question")
    func oneSidedChanges() throws {
        let base = try Self.document(machines: [
            Self.machine("pi", name: "Pi"),
            Self.machine("tower", name: "Tower")
        ])
        // One device renames the Pi.
        let mine = try Self.document(machines: [
            Self.machine("pi", name: "Raspberry Pi"),
            Self.machine("tower", name: "Tower")
        ])
        // The other adds an address to the tower.
        let theirs = try Self.document(machines: [
            Self.machine("pi", name: "Pi"),
            Self.machine("tower", name: "Tower", endpoints: [["id": "lan", "host": "10.0.0.9"]])
        ])

        let differences = try SetupMerge.differences(mine: mine, theirs: theirs, base: base)
        #expect(!differences.isEmpty)
        // Neither change collides, so nothing needs a person.
        #expect(differences.allSatisfy { !$0.needsDecision })

        let merged = try SetupMerge.merge(
            mine: mine, theirs: theirs, base: base,
            differences: differences, choices: [:],
            identity: ControllerIdentity(id: "setup-1", revision: 7, lineage: ["a", "b"])
        )
        let result = try #require((try JSONSerialization.jsonObject(with: merged)) as? [String: Any])
        let machines = try #require(result["machines"] as? [[String: Any]])

        // Both edits survive. That is the whole point.
        let pi = try #require(machines.first { $0["id"] as? String == "pi" })
        #expect(pi["name"] as? String == "Raspberry Pi")
        let tower = try #require(machines.first { $0["id"] as? String == "tower" })
        #expect((tower["endpoints"] as? [[String: Any]])?.count == 1)
    }

    @Test("an entry both sides changed is the only thing a person is asked about")
    func bothSidesChanged() throws {
        let base = try Self.document(machines: [Self.machine("pi", name: "Pi")])
        let mine = try Self.document(machines: [Self.machine("pi", name: "Attic Pi")])
        let theirs = try Self.document(machines: [Self.machine("pi", name: "Kitchen Pi")])

        let differences = try SetupMerge.differences(mine: mine, theirs: theirs, base: base)
        let clash = try #require(differences.first { $0.needsDecision })
        #expect(clash.kind == .bothChanged)
        #expect(clash.mine?.contains("Attic Pi") == true)
        #expect(clash.theirs?.contains("Kitchen Pi") == true)
        // The default is the copy the machines are already carrying: taking "mine" by default would
        // quietly undo an edit made on another device.
        #expect(clash.defaultChoice == .theirs)

        let keepingMine = try SetupMerge.merge(
            mine: mine, theirs: theirs, base: base, differences: differences,
            choices: [clash.id: .mine],
            identity: ControllerIdentity(id: "setup-1", revision: 7)
        )
        let result = try #require((try JSONSerialization.jsonObject(with: keepingMine)) as? [String: Any])
        let machines = try #require(result["machines"] as? [[String: Any]])
        #expect(machines.first?["name"] as? String == "Attic Pi")
    }

    @Test("without a common ancestor every difference needs a decision")
    func noBase() throws {
        let mine = try Self.document(machines: [Self.machine("pi", name: "Pi")])
        let theirs = try Self.document(machines: [Self.machine("pi", name: "Other Pi")])
        let differences = try SetupMerge.differences(mine: mine, theirs: theirs, base: nil)
        #expect(differences.allSatisfy { $0.kind == .unknownBase })
        #expect(differences.allSatisfy { $0.needsDecision })
    }

    @Test("a machine added on one side only is one entry, not a dozen")
    func addedMachine() throws {
        let base = try Self.document(machines: [Self.machine("pi", name: "Pi")])
        let mine = try Self.document(machines: [Self.machine("pi", name: "Pi")])
        let theirs = try Self.document(machines: [
            Self.machine("pi", name: "Pi"),
            Self.machine("tower", name: "Tower")
        ])
        let differences = try SetupMerge.differences(mine: mine, theirs: theirs, base: base)
        let added = try #require(differences.first { $0.path == ["machines", "tower"] })
        #expect(added.kind == .onlyTheirsChanged)
        #expect(added.mine == nil)

        let merged = try SetupMerge.merge(mine: mine, theirs: theirs, base: base,
                                          differences: differences, choices: [:],
                                          identity: ControllerIdentity(id: "s", revision: 2))
        let result = try #require((try JSONSerialization.jsonObject(with: merged)) as? [String: Any])
        #expect((result["machines"] as? [[String: Any]])?.count == 2)
    }

    @Test("keys this build has never seen survive a merge")
    func unknownKeysSurvive() throws {
        // A newer client, or one only the phone reads. Round-tripping through the typed model would
        // delete these silently and on one device only.
        let base = try Self.document(machines: [Self.machine("pi", name: "Pi")],
                                     extra: ["somethingNew": ["a": 1]])
        let mine = try Self.document(machines: [Self.machine("pi", name: "Attic Pi")],
                                     extra: ["somethingNew": ["a": 1]])
        let theirs = try Self.document(machines: [Self.machine("pi", name: "Pi",
                                                               extra: ["futureKey": "kept"])],
                                       extra: ["somethingNew": ["a": 1]])

        let differences = try SetupMerge.differences(mine: mine, theirs: theirs, base: base)
        let merged = try SetupMerge.merge(mine: mine, theirs: theirs, base: base,
                                          differences: differences, choices: [:],
                                          identity: ControllerIdentity(id: "s", revision: 2))
        let result = try #require((try JSONSerialization.jsonObject(with: merged)) as? [String: Any])
        #expect(result["somethingNew"] != nil)
    }

    @Test("the merged document is canonical, so its hash is the one everyone will compute")
    func mergedIsCanonical() throws {
        let mine = try Self.document(machines: [Self.machine("pi", name: "Pi")])
        let theirs = try Self.document(machines: [Self.machine("pi", name: "Pi")])
        let merged = try SetupMerge.merge(mine: mine, theirs: theirs, base: nil,
                                          differences: [], choices: [:],
                                          identity: ControllerIdentity(id: "s", revision: 2, lineage: []))
        #expect(try Canonical.bytes(merged) == merged)
    }

    @Test("the identity block round-trips through the encoder")
    func identityEncoding() throws {
        let identity = ControllerIdentity(id: "setup-1", name: "Home", revision: 4,
                                          updatedAt: "2026-09-05T09:00:00Z", source: "mac",
                                          device: "MacBook", lineage: ["aa", "bb"])
        let encoded = SetupMerge.encode(identity)
        let data = try JSONSerialization.data(withJSONObject: ["controller": encoded])
        let decoded = try JSONDecoder().decode(Wrapper.self, from: data).controller
        #expect(decoded == identity)
    }

    private struct Wrapper: Decodable {
        var controller: ControllerIdentity
    }
}
