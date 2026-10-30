import Foundation
import Testing
@testable import LegionControlCore

/// Putting two diverged setups back together without losing either side's work.
///
/// The scenario these are written against: two devices edit the same revision while apart, both
/// publish, and the result has to contain both changes, ask about what both changed, and write back
/// untouched what neither did. The contract's vectors pin the merge case by case
/// (ContractVectorTests); these name the defects it exists to prevent (C6).
struct SetupMergeTests {

    static let setupId = "setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846"
    static let writer = SetupMerge.Writer(source: "mac", device: "MacBook", now: "2026-09-05T14:00:00.000Z")

    /// A document with an identity, written as given: the text is the spelling under test.
    static func document(revision: Int, lineage: [String] = [], machines: String, extra: String = "") -> Data {
        let lineage = lineage.map { "\"\($0)\"" }.joined(separator: ", ")
        return Data("""
        {
          "version": 1,
          "controller": { "id": "\(setupId)", "revision": \(revision), "lineage": [\(lineage)] },\(extra)
          "machines": [\(machines)]
        }
        """.utf8)
    }

    static func machine(_ id: String, name: String, endpoints: String = #"{ "id": "lan", "host": "192.0.2.10", "user": "me" }"#,
                        extra: String = "") -> String {
        """
        { "id": "\(id)", "name": "\(name)",\(extra)
          "endpoints": [ \(endpoints) ],
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] }
        """
    }

    static func hash(_ data: Data) throws -> String { try Canonical.hash(data) }

    static func merge(base: Data?, mine: Data, theirs: Data, choices: [String: SetupMerge.Choice] = [:]) throws -> SetupMerge.Outcome {
        try SetupMerge.merge(base: base, mine: mine, theirs: theirs, mineHash: try hash(mine), theirsHash: try hash(theirs),
                             writer: writer, choices: choices)
    }

    static func tree(_ data: Data?) throws -> SetupJSON { try SetupJSON.parse(try #require(data)) }

    // MARK: - Differences

    @Test("an entry only one side changed is taken from that side without a question")
    func oneSidedChanges() throws {
        let base = Self.document(revision: 5, machines: Self.machine("pi", name: "Pi") + "," + Self.machine("tower", name: "Tower"))
        // One device renames the Pi; the other adds an address to the tower.
        let mine = Self.document(revision: 6, lineage: [try Self.hash(base)],
                                 machines: Self.machine("pi", name: "Raspberry Pi") + "," + Self.machine("tower", name: "Tower"))
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Pi") + "," + Self.machine(
            "tower", name: "Tower",
            endpoints: #"{ "id": "lan", "host": "192.0.2.10", "user": "me" }, { "id": "wifi", "host": "192.0.2.11", "user": "me" }"#))

        let outcome = try Self.merge(base: base, mine: mine, theirs: theirs)
        #expect(outcome.differences.map(\.entry) == ["machines[pi].fields", "machines[tower].endpoints[wifi]"])
        #expect(outcome.differences.map(\.kind) == [.mine, .theirs])
        #expect(outcome.differences.allSatisfy { !$0.needsDecision })

        // Both edits survive. That is the whole point.
        let result = try Self.tree(outcome.result)
        let machines = try #require(result["machines"]?.items)
        #expect(machines[0]["name"] == .string("Raspberry Pi"))
        #expect(machines[1]["endpoints"]?.items?.compactMap(\.id) == ["lan", "wifi"])
        // And the result descends from both, so both machines take it as a fast-forward.
        let block = try #require(result["controller"])
        #expect(block["revision"] == .number("7"))
        #expect(block["lineage"]?.items?.prefix(2).compactMap(\.stringValue) == [try Self.hash(mine), try Self.hash(theirs)])
        #expect(block["source"] == .string("mac"))
        #expect(block["device"] == .string("MacBook"))
    }

    @Test("an entry both sides changed is asked about, and the default is the copy the machines carry")
    func bothSidesChanged() throws {
        let base = Self.document(revision: 5, machines: Self.machine("pi", name: "Pi"))
        let mine = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Attic Pi"))
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Kitchen Pi"))

        let differences = try SetupMerge.differences(base: base, mine: mine, theirs: theirs)
        let clash = try #require(differences.first)
        #expect(clash.entry == "machines[pi].fields")
        #expect(clash.kind == .both)
        #expect(clash.mine?.contains("Attic Pi") == true)
        #expect(clash.theirs?.contains("Kitchen Pi") == true)
        // Taking this device's by default would quietly undo an edit made on another one.
        #expect(clash.defaultChoice == .theirs)

        let keepingMine = try Self.merge(base: base, mine: mine, theirs: theirs, choices: [clash.id: .mine])
        #expect(try Self.tree(keepingMine.result)["machines"]?.items?.first?["name"] == .string("Attic Pi"))
    }

    @Test("deleting a machine the other side edited is asked about rather than decided by the deletion")
    func deleteVersusEditAsks() throws {
        // Android and the desktop used to drop the edit without a word.
        let base = Self.document(revision: 5, machines: Self.machine("pi", name: "Pi") + "," + Self.machine("tower", name: "Tower"))
        let mine = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Pi"))
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)],
                                   machines: Self.machine("pi", name: "Pi") + "," + Self.machine("tower", name: "Big Tower"))

        let outcome = try Self.merge(base: base, mine: mine, theirs: theirs)
        #expect(outcome.differences.map(\.entry) == ["machines[tower]"])
        #expect(outcome.differences.first?.kind == .both)
        // Preselected theirs: the edit comes back, after its predecessor in theirs.
        #expect(try Self.tree(outcome.result)["machines"]?.items?.compactMap(\.id) == ["pi", "tower"])
        // Keeping the deletion is a choice a person makes.
        let deleted = try Self.merge(base: base, mine: mine, theirs: theirs, choices: ["machines[tower]": .mine])
        #expect(try Self.tree(deleted.result)["machines"]?.items?.compactMap(\.id) == ["pi"])
    }

    @Test("without a common ancestor every difference needs a decision")
    func noBase() throws {
        let mine = Self.document(revision: 6, machines: Self.machine("pi", name: "Pi"))
        let theirs = Self.document(revision: 6, machines: Self.machine("pi", name: "Other Pi"))
        let differences = try SetupMerge.differences(base: nil, mine: mine, theirs: theirs)
        #expect(differences.map(\.kind) == [.noBase])
        #expect(differences.allSatisfy { $0.needsDecision })
    }

    @Test("endpoints without an id are kept together, in place, and never dropped")
    func idlessEndpoints() throws {
        // Android used to drop every endpoint without an id from a merge.
        let base = Self.document(revision: 5, machines: Self.machine("pi", name: "Pi", endpoints: #"{ "host": "192.0.2.10" }"#))
        let mine = Self.document(revision: 6, lineage: [try Self.hash(base)],
                                 machines: Self.machine("pi", name: "Attic Pi", endpoints: #"{ "host": "192.0.2.10" }"#))
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)],
                                   machines: Self.machine("pi", name: "Pi", endpoints: #"{ "host": "192.0.2.10" }, { "host": "192.0.2.12" }"#))

        let outcome = try Self.merge(base: base, mine: mine, theirs: theirs)
        #expect(outcome.differences.map(\.entry) == ["machines[pi].endpoints[]", "machines[pi].fields"])
        let endpoints = try #require(try Self.tree(outcome.result)["machines"]?.items?.first?["endpoints"]?.items)
        #expect(endpoints.compactMap { $0["host"]?.stringValue } == ["192.0.2.10", "192.0.2.12"])
    }

    @Test("keys this build has never seen survive a merge, at the top level and in the controller block")
    func unknownKeysSurvive() throws {
        // The Mac used to drop unknown controller keys on every merge and edit, and to keep only
        // its own side of an unknown top-level key.
        let base = Self.document(revision: 5, machines: Self.machine("pi", name: "Pi"), extra: #" "notes": "a","#)
        var mine = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Attic Pi"),
                                 extra: #" "notes": "a","#)
        mine = Data(String(decoding: mine, as: UTF8.self).replacingOccurrences(of: #""lineage""#, with: #""policy": { "keep": true }, "lineage""#).utf8)
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Pi", extra: #" "futureKey": "kept","#),
                                   extra: #" "notes": "b","#)

        let outcome = try Self.merge(base: base, mine: mine, theirs: theirs)
        let result = try Self.tree(outcome.result)
        #expect(result["notes"] == .string("b"))
        #expect(result["controller"]?["policy"]?["keep"] == .bool(true))
        // The machine's fields differ on both sides, and the default takes theirs, unknown key and all.
        #expect(outcome.differences.first { $0.entry == "machines[pi].fields" }?.kind == .both)
        #expect(result["machines"]?.items?.first?["futureKey"] == .string("kept"))
    }

    @Test("what a merge does not replace is written back exactly: member order and number spelling")
    func spellingSurvives() throws {
        // The Mac used to rewrite 22.0 as 22, sort every key, and move a merged entry to the end of
        // its array, each of which turned a document nobody edited into a different one.
        let ports = #"{ "id": "lan", "host": "192.0.2.10", "user": "me", "port": 22.0 }"#
        let base = Self.document(revision: 5, machines: Self.machine("tower", name: "Tower", endpoints: ports, extra: #" "weight": 1e2,"#)
                                 + "," + Self.machine("pi", name: "Pi"))
        let mine = Self.document(revision: 6, lineage: [try Self.hash(base)],
                                 machines: Self.machine("tower", name: "Tower", endpoints: ports, extra: #" "weight": 1e2,"#)
                                 + "," + Self.machine("pi", name: "Attic Pi"))
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)],
                                   machines: Self.machine("tower", name: "Tower", endpoints: ports.replacingOccurrences(of: "22.0", with: "22"),
                                                          extra: #" "weight": 100,"#)
                                   + "," + Self.machine("pi", name: "Pi"))

        let outcome = try Self.merge(base: base, mine: mine, theirs: theirs)
        // `100` and `1e2` are one number, so the tower is no difference at all.
        #expect(outcome.differences.map(\.entry) == ["machines[pi].fields"])
        let text = String(decoding: try #require(outcome.result), as: UTF8.self)
        #expect(text.contains(#""port": 22.0"#))
        #expect(text.contains(#""weight": 1e2"#))
        let tower = try #require(try Self.tree(outcome.result)["machines"]?.items?.first)
        #expect(tower.members?.map(\.name) == ["id", "name", "weight", "endpoints", "systems"])
    }

    @Test("a merge that would break a rule is not written, and says which")
    func refusedResult() throws {
        let base = Self.document(revision: 5, machines: Self.machine("pi", name: "Pi") + "," + Self.machine("tower", name: "Tower"))
        // This device takes the Pi out; the other makes it the tower's wake helper.
        let mine = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("tower", name: "Tower"))
        let theirs = Self.document(revision: 6, lineage: [try Self.hash(base)], machines: Self.machine("pi", name: "Pi") + "," + Self.machine(
            "tower", name: "Tower", extra: #" "wake": { "mac": "02:00:00:00:00:01", "helpers": [ { "machine": "pi", "action": "wake-tower" } ] },"#))

        let kept = try Self.merge(base: base, mine: mine, theirs: theirs, choices: ["machines[pi]": .mine])
        #expect(kept.result == nil)
        #expect(kept.error == "helper-unknown")
    }

    @Test("a setup with an error a merge cannot carry is fixed before it is merged")
    func blockedMine() throws {
        let theirs = Self.document(revision: 6, machines: Self.machine("pi", name: "Pi"))
        let duplicate = Self.document(revision: 6, machines: Self.machine("pi", name: "Pi") + "," + Self.machine("pi", name: "Pi"))
        #expect(SetupMerge.blocker(mine: duplicate) != nil)
        #expect(throws: SetupMerge.MergeFailure.self) { _ = try Self.merge(base: nil, mine: duplicate, theirs: theirs) }

        // One that only names something missing merges, and taking theirs' side repairs it.
        let dangling = Self.document(revision: 6, machines: Self.machine("pi", name: "Pi", extra: #" "site": "gone","#))
            .replacingSites(#"[ { "id": "home" } ]"#)
        #expect(SetupMerge.blocker(mine: dangling) == nil)
    }

    @Test("the base is the newest common ancestor whose kept bytes still hash to it")
    func commonBase() throws {
        let older = Self.document(revision: 4, machines: Self.machine("pi", name: "Pi"))
        let newer = Self.document(revision: 5, lineage: [try Self.hash(older)], machines: Self.machine("pi", name: "Pi 2"))
        let kept = [try Self.hash(older): older, try Self.hash(newer): Data("tampered".utf8)]
        let base = SetupMerge.commonBase(mineHash: "m", theirsHash: "t",
                                         mineLineage: [try Self.hash(newer), try Self.hash(older)],
                                         theirsLineage: [try Self.hash(newer), try Self.hash(older)]) { kept[$0] }
        // The newest one was changed on disk, so it is not the document it is named after.
        #expect(base == (try Self.hash(older)))
    }

    @Test("the merged document is canonical, so its hash is the one everyone will compute")
    func mergedIsCanonical() throws {
        let mine = Self.document(revision: 6, machines: Self.machine("pi", name: "Pi"))
        let theirs = Self.document(revision: 7, machines: Self.machine("pi", name: "Pi"))
        let merged = try #require(try Self.merge(base: nil, mine: mine, theirs: theirs).result)
        #expect(try Canonical.bytes(merged) == merged)
        #expect(try Self.tree(merged)["controller"]?["revision"] == .number("8"))
    }
}

private extension Data {
    /// The same document with a sites list.
    func replacingSites(_ sites: String) -> Data {
        Data(String(decoding: self, as: UTF8.self).replacingOccurrences(of: #""machines": ["#, with: #""sites": \#(sites), "machines": ["#).utf8)
    }
}
