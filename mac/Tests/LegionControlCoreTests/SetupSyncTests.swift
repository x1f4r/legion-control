import Foundation
import Testing
@testable import LegionControlCore

/// Reconciling the setup between peers.
///
/// Every device may edit and publish, and none of them is on all the time, so the rule that decides
/// what happens when two copies differ is the one thing standing between an offline edit and its
/// silent disappearance. These are the cases from the amendment's acceptance list.
struct SetupSyncTests {

    // MARK: - Building documents

    static func document(_ body: String, identity: ControllerIdentity) throws -> ControllerDocument {
        var root = (try JSONSerialization.jsonObject(with: Data(body.utf8))) as! [String: Any]
        root["controller"] = SetupMerge.encode(identity)
        let bytes = try JSONSerialization.data(withJSONObject: root,
                                               options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
        return try ControllerDocument(raw: bytes, identity: identity)
    }

    static let minimal = """
    { "version": 1, "machines": [ { "id": "pi", "name": "Pi",
      "endpoints": [ { "id": "lan", "host": "10.0.0.5" } ],
      "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
    """

    static func view(hash: String?, identity: ControllerIdentity?, lineage: [String]? = nil,
                     contract: Int = 3, reportsCopy: Bool = true) -> SetupSync.MachineView {
        SetupSync.MachineView(reportsCopy: reportsCopy, contract: contract, hash: hash,
                              identity: identity, lineage: lineage)
    }

    // MARK: - The rules

    @Test("a machine holding nothing is given the document without asking")
    func machineHasNothing() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 1, lineage: []))
        let decision = SetupSync.decide(local: mine, machine: Self.view(hash: nil, identity: nil))
        #expect(decision == .push(.machineHasNothing))
    }

    @Test("a machine holding one of this document's ancestors is fast-forwarded")
    func machineIsBehind() throws {
        let old = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 1, lineage: []))
        let new = try Self.document(Self.minimal + " ",
                                    identity: .init(id: "setup-1", revision: 2, lineage: [old.hash]))
        let decision = SetupSync.decide(
            local: new,
            machine: Self.view(hash: old.hash, identity: old.identity)
        )
        #expect(decision == .push(.machineIsBehind))
    }

    @Test("this device being an ancestor of the machine's copy fetches, without asking")
    func weAreBehind() throws {
        // A4: a device that was offline while another published comes back and catches up on its own.
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 4, lineage: []))
        let theirs = ControllerIdentity(id: "setup-1", revision: 6, lineage: ["deadbeef", mine.hash])
        let decision = SetupSync.decide(
            local: mine,
            machine: Self.view(hash: "abc", identity: theirs, lineage: theirs.ancestors)
        )
        #expect(decision == .fetch)
    }

    @Test("hashes that differ with no descent either way ask for the lineage first")
    func readsMetaBeforeDeciding() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 6, lineage: ["aaa"]))
        let theirs = ControllerIdentity(id: "setup-1", revision: 6)
        let decision = SetupSync.decide(local: mine, machine: Self.view(hash: "bbb", identity: theirs))
        // Status deliberately does not carry the lineage, so one extra round trip decides it.
        #expect(decision == .readMeta)
    }

    @Test("two branches from the same base are a divergence, not a race won by revision number")
    func divergence() throws {
        // A3: A and B both edit revision 5. Revision numbers alone would let the later arrival win
        // and quietly delete the other's work; ancestry says neither descends from the other.
        let base = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 5, lineage: []))
        let mine = try Self.document(Self.minimal + " ",
                                     identity: .init(id: "setup-1", revision: 6, lineage: [base.hash]))
        let theirs = ControllerIdentity(id: "setup-1", revision: 6, lineage: [base.hash])
        let decision = SetupSync.decide(
            local: mine,
            machine: Self.view(hash: "theirs-hash", identity: theirs, lineage: theirs.ancestors)
        )
        #expect(decision == .diverged)
    }

    @Test("a different setup id is never overwritten on its own")
    func differentSetup() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 9, lineage: []))
        let theirs = ControllerIdentity(id: "setup-2", revision: 1)
        let decision = SetupSync.decide(
            local: mine,
            machine: Self.view(hash: "other", identity: theirs, lineage: [])
        )
        #expect(decision == .differentSetup)
    }

    @Test("an agent from before all this is left out rather than treated as a conflict")
    func legacyAgent() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 1))
        #expect(SetupSync.decide(local: mine, machine: Self.view(hash: "x", identity: nil, contract: 2)) == .unsupported)
        #expect(SetupSync.decide(local: mine, machine: Self.view(hash: nil, identity: nil, reportsCopy: false)) == .unsupported)
    }

    @Test("a copy with no identity is replaceable, because nobody claims it")
    func unidentifiedCopy() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 3, lineage: []))
        let decision = SetupSync.decide(
            local: mine,
            machine: Self.view(hash: "legacy", identity: ControllerIdentity(id: nil, revision: 0))
        )
        #expect(decision == .push(.machineHasUnidentifiedCopy))
    }

    @Test("the same document on both sides does nothing at all")
    func inSync() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 2, lineage: []))
        #expect(SetupSync.decide(local: mine, machine: Self.view(hash: mine.hash, identity: mine.identity)) == .inSync)
    }

    // MARK: - Lineage bookkeeping

    @Test("an edit lists the document it came from, newest first")
    func lineageGrows() {
        let first = ControllerIdentity(id: "setup-1", revision: 1, lineage: [])
        let second = first.next(parent: "aaa", device: "Tower")
        #expect(second.revisionNumber == 2)
        #expect(second.ancestors == ["aaa"])
        #expect(second.source == "mac")
        #expect(second.device == "Tower")

        let third = second.next(parent: "bbb", device: "Tower")
        #expect(third.ancestors == ["bbb", "aaa"])
    }

    @Test("the lineage is capped, and the cap fails towards a question rather than a loss")
    func lineageIsCapped() {
        var identity = ControllerIdentity(id: "setup-1", revision: 0, lineage: [])
        for index in 0..<50 {
            identity = identity.next(parent: String(format: "%064x", index), device: "Tower")
        }
        #expect(identity.ancestors.count == ControllerIdentity.lineageLimit)
        #expect(identity.revisionNumber == 50)
        // The newest ancestors are the ones kept: a branch that is far behind reports as divergence,
        // which stops at a person, rather than as a fast-forward, which would not.
        #expect(identity.ancestors.first == String(format: "%064x", 49))
    }

    @Test("a merge descends from both sides, so both machines accept it")
    func mergeLineage() {
        let mine = ControllerIdentity(id: "setup-1", revision: 6, lineage: ["base", "older"])
        let theirs = ControllerIdentity(id: "setup-1", revision: 7, lineage: ["base"])
        let merged = mine.merged(with: theirs, mineHash: "mine", theirsHash: "theirs", device: "Mac")
        #expect(merged.revisionNumber == 8)
        #expect(merged.ancestors.prefix(2) == ["mine", "theirs"])
        #expect(merged.ancestors.contains("base"))
    }

    @Test("a lineage of the wrong shape is refused rather than half-trusted")
    func lineageValidation() {
        #expect(ControllerIdentity(lineage: []).lineageIsValid)
        #expect(ControllerIdentity(lineage: [String(repeating: "a", count: 64)]).lineageIsValid)
        #expect(!ControllerIdentity(lineage: ["short"]).lineageIsValid)
        #expect(!ControllerIdentity(lineage: [String(repeating: "A", count: 64)]).lineageIsValid)
        let duplicate = String(repeating: "b", count: 64)
        #expect(!ControllerIdentity(lineage: [duplicate, duplicate]).lineageIsValid)
        #expect(!ControllerIdentity(lineage: (0..<33).map { String(format: "%064x", $0) }).lineageIsValid)
    }

    // MARK: - What the row says

    @Test("a divergence names both revisions and who wrote them")
    func divergenceSentence() throws {
        let mine = try Self.document(Self.minimal, identity: .init(id: "setup-1", revision: 6,
                                                                   updatedAt: "2026-09-05T09:00:00Z",
                                                                   source: "mac", device: "MacBook"))
        let theirs = ControllerIdentity(id: "setup-1", revision: 6, updatedAt: "2026-09-05T10:00:00Z",
                                        source: "phone", device: "Pixel")
        let sharing = SetupSync.sharing(for: .diverged, machine: "Pi", local: mine, remote: theirs)
        guard case .conflict(let sentence) = sharing else {
            Issue.record("expected a conflict")
            return
        }
        #expect(sentence.contains("MacBook"))
        #expect(sentence.contains("Pixel"))
        #expect(sentence.contains("nothing has been sent"))
    }
}
