import Foundation
import Testing
@testable import LegionControlCore

/// Reconciling the setup between peers.
///
/// Every device may edit and publish, and none of them is on all the time, so the rule that decides
/// what happens when two copies differ is the one thing standing between an offline edit and its
/// silent disappearance. The contract's vectors pin each function case by case
/// (ContractVectorTests); these drive the runner that strings them together, over a scripted agent.
@MainActor
struct SetupSyncTests {

    // MARK: - Documents

    static let setupId = "setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846"

    /// A valid document with an identity, its canonical bytes and hash.
    static func document(name: String, revision: Int, lineage: [String]) throws -> ControllerDocument {
        let text = """
        { "version": 1,
          "controller": { "id": "\(setupId)", "name": "Home", "revision": \(revision), "lineage": [\(lineage.map { "\"\($0)\"" }.joined(separator: ","))] },
          "machines": [ { "id": "pi", "name": "\(name)",
            "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5", "user": "me" } ],
            "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] } ] }
        """
        let canonical = try Canonical.bytes(Data(text.utf8))
        return ControllerDocument(bytes: canonical, hash: Canonical.sha256(canonical),
                                  identity: ControllerIdentity(id: setupId, name: "Home", revision: revision, lineage: lineage))
    }

    static func status(holding hash: String?, id: String? = setupId, version: String = "3.1.0", running: Int = 0) -> AgentStatus {
        let operations = (0..<running).map { #"{ "id": "op-\#($0)", "kind": "update", "state": "running" }"# }.joined(separator: ",")
        let controller = hash.map { #"{ "hash": "\#($0)", "id": \#(id.map { "\"\($0)\"" } ?? "null") }"# } ?? "{}"
        let json = """
        { "ok": true, "contract": 3, "agentVersion": "\(version)", "agent": { "version": "\(version)", "contract": 3 },
          "operations": { "running": [\(operations)], "queued": [], "recent": [] },
          "controller": \(controller) }
        """
        return try! JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    }

    static func meta(of document: ControllerDocument) -> String {
        let lineage = document.identity.ancestors.map { "\"\($0)\"" }.joined(separator: ",")
        return #"{ "ok": true, "contract": 3, "hash": "\#(document.hash)", "meta": { "hash": "\#(document.hash)", "id": "\#(setupId)", "revision": \#(document.identity.revisionNumber), "lineage": [\#(lineage)] } }"#
    }

    static func read(of document: ControllerDocument, reporting hash: String? = nil) throws -> String {
        let reply: [String: Any] = ["ok": true, "contract": 3, "raw": String(decoding: document.bytes, as: UTF8.self),
                                    "hash": hash ?? document.hash]
        return String(decoding: try JSONSerialization.data(withJSONObject: reply), as: UTF8.self)
    }

    // MARK: - A scripted agent

    final class Channel: SetupChannel {
        let setupHolder = "Pi"
        var status: AgentStatus?
        var isWorking = false
        var applied: ControllerDocument?
        private(set) var adopted: [Data] = []
        private(set) var lines: [String] = []

        /// Replies by request, in order: `meta`, `read` and `set`. A missing reply is a dropped link.
        var replies: [String: [String]] = [:]
        private(set) var sent: [String] = []
        private(set) var pushed: [Data] = []

        init(applied: ControllerDocument?, status: AgentStatus?) {
            self.applied = applied
            self.status = status
        }

        func appliedSetup() -> ControllerDocument? { applied }

        func commitSetup(_ bytes: Data, describedAs description: String, replacing expectedHead: String?) -> ConfigStore.AdoptOutcome {
            guard applied?.hash == expectedHead else { return .headMoved(current: applied?.hash) }
            adopted.append(bytes)
            let identity = ControllerIdentity(block: try? SetupJSON.parse(bytes)["controller"])
            applied = ControllerDocument(bytes: bytes, hash: Canonical.sha256(bytes), identity: identity)
            return .adopted(hash: Canonical.sha256(bytes))
        }

        private func next(_ request: String) throws -> Data {
            sent.append(request)
            guard var queue = replies[request], !queue.isEmpty else {
                throw AgentFailure(.linkLost, detail: "no scripted reply", dispatch: .unknown)
            }
            let reply = queue.removeFirst()
            replies[request] = queue
            return Data(reply.utf8)
        }

        func configMeta() async throws -> AgentConfigResult {
            try AgentReplyDecoder.decode(AgentConfigResult.self, from: try next("meta"))
        }

        func configRead() async throws -> RemoteControllerDocument {
            try AgentReplyDecoder.decode(RemoteControllerDocument.self, from: try next("read"))
        }

        func configSet(_ document: ControllerDocument, replace: Bool) async throws -> AgentConfigResult {
            pushed.append(document.bytes)
            return try AgentReplyDecoder.decode(AgentConfigResult.self, from: try next(replace ? "replace" : "set"))
        }

        func saySetup(_ line: String, isError: Bool, detail: String?) { lines.append(line) }

        func describe(_ failure: AgentFailure) -> String { failure.detailText ?? "failed" }

        func setupDidChange() {}
    }

    /// Wait for the runner's requests to finish, the way a status poll would before the next one.
    static func settle(_ sync: SetupPeerSync) async {
        let deadline = Date().addingTimeInterval(15)
        repeat { try? await Task.sleep(for: .milliseconds(5)) } while sync.isSharing && Date() < deadline
    }

    /// A clock the test moves by hand.
    final class Clock {
        var now = Date(timeIntervalSince1970: 1_000_000)
    }

    static func sync(_ clock: Clock = Clock()) -> SetupPeerSync {
        let sync = SetupPeerSync()
        sync.now = { clock.now }
        return sync
    }

    // MARK: - Deciding and fetching

    @Test("a machine ahead of this device is read, verified and adopted after one meta read, with no second status")
    func metaThenFetchInOneRound() async throws {
        // M9: deciding again after the meta read used to run while the read still counted as in
        // flight, so it did nothing, and the fetch waited for the next poll.
        let mine = try Self.document(name: "Pi", revision: 1, lineage: [])
        let theirs = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [mine.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: theirs.hash))
        channel.replies = ["meta": [Self.meta(of: theirs)], "read": [try Self.read(of: theirs)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)

        #expect(channel.sent == ["meta", "read"])
        #expect(channel.adopted == [theirs.bytes])
        #expect(channel.applied?.hash == theirs.hash)
    }

    @Test("a machine that moved between the meta read and the fetch is not adopted, and the next status decides")
    func remoteMovedBetweenMetaAndFetch() async throws {
        let mine = try Self.document(name: "Pi", revision: 1, lineage: [])
        let theirs = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [mine.hash])
        let later = try Self.document(name: "Hall Pi", revision: 3, lineage: [theirs.hash, mine.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: theirs.hash))
        channel.replies = ["meta": [Self.meta(of: theirs), Self.meta(of: later)],
                           "read": [try Self.read(of: later), try Self.read(of: later)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        // The bytes are the machine's own, but not the ones the decision was made for.
        #expect(channel.adopted.isEmpty)
        #expect(channel.applied?.hash == mine.hash)

        channel.status = Self.status(holding: later.hash)
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.adopted == [later.bytes])
    }

    @Test("bytes that do not reproduce the reported hash are remembered as unverifiable until the agent changes")
    func unverifiableIsRemembered() async throws {
        let mine = try Self.document(name: "Pi", revision: 1, lineage: [])
        let theirs = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [mine.hash])
        // An agent before 3.1 whose rebuilt document came out in another spelling: the bytes hash
        // to neither the expected hash nor the one the reply reports.
        let respelled = try Self.document(name: "Kitchen  Pi", revision: 2, lineage: [mine.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: theirs.hash, version: "3.0.1"))
        channel.replies = ["meta": [Self.meta(of: theirs)],
                           "read": [try Self.read(of: respelled, reporting: theirs.hash), try Self.read(of: theirs)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.adopted.isEmpty)
        #expect(channel.lines.last?.contains("has to be updated") == true)
        guard case .failed(let sentence) = sync.sharing(holder: "Pi", local: mine, status: channel.status) else {
            Issue.record("the refusal is not shown")
            return
        }
        #expect(sentence.contains("cannot be checked"))

        // Not read again for the same two hashes and the same agent.
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["meta", "read"])

        // An updated agent is asked again.
        channel.status = Self.status(holding: theirs.hash, version: "3.1.0")
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.adopted == [theirs.bytes])
    }

    @Test("a config reply without a hash is never adopted, and counts as a failed request")
    func readWithoutAHashIsAFailure() async throws {
        // C1: such a reply describes no document, whatever bytes came with it.
        let mine = try Self.document(name: "Pi", revision: 1, lineage: [])
        let theirs = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [mine.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: theirs.hash))
        let unhashed: [String: Any] = ["ok": true, "contract": 3, "raw": String(decoding: theirs.bytes, as: UTF8.self)]
        channel.replies = ["meta": [Self.meta(of: theirs)],
                           "read": [String(decoding: try JSONSerialization.data(withJSONObject: unhashed), as: UTF8.self)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.adopted.isEmpty)
        #expect(channel.lines.last?.contains("sent no hash") == true)
        // A failure holds the next attempt back for a minute.
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["meta", "read"])
    }

    @Test("a document that is not this setup's, or does not descend from the applied one, is never adopted")
    func inconsistentIsRefused() async throws {
        let mine = try Self.document(name: "Pi", revision: 1, lineage: [])
        let theirs = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [mine.hash])
        // The meta read says it descends; the bytes themselves do not.
        let orphan = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [])
        let channel = Channel(applied: mine, status: Self.status(holding: orphan.hash))
        channel.replies = ["meta": [Self.meta(of: theirs).replacingOccurrences(of: theirs.hash, with: orphan.hash)],
                           "read": [try Self.read(of: orphan)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.adopted.isEmpty)
        #expect(channel.lines.last?.contains("identity is not what its agent reported") == true)
    }

    @Test("a fetch decided against one document is never written over an edit saved while it was out")
    func fetchOverAMovedHeadWritesNothing() async throws {
        let mine = try Self.document(name: "Pi", revision: 1, lineage: [])
        let theirs = try Self.document(name: "Kitchen Pi", revision: 2, lineage: [mine.hash])
        let edit = try Self.document(name: "Attic Pi", revision: 2, lineage: [mine.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: theirs.hash))
        channel.replies = ["meta": [Self.meta(of: theirs)], "read": [try Self.read(of: theirs)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        // The person saves an edit while the read is out.
        channel.applied = edit
        await Self.settle(sync)

        #expect(channel.adopted.isEmpty)
        #expect(channel.applied?.hash == edit.hash)
        // The decision made again at once finds the branch.
        #expect(sync.verdict.decision == .diverged)
    }

    // MARK: - Pushing and the guardrails

    @Test("no push goes while the machine reports a running operation, whoever started it")
    func pushWaitsForRunningOperations() async throws {
        // C7: only this device's own action used to hold a push back.
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash, running: 1))
        channel.replies = ["set": [#"{ "ok": true, "contract": 3, "action": "stored", "hash": "\#(mine.hash)" }"#]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent.isEmpty)

        // Nor while this device has an action of its own in flight there.
        channel.status = Self.status(holding: old.hash)
        channel.isWorking = true
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent.isEmpty)

        // Reads are never held.
        #expect(SetupSync.gate(decision: .readMeta, localHash: mine.hash, remoteHash: old.hash, agentVersion: nil,
                               now: Date(), state: SetupSync.PeerState(localMutation: true, runningOperations: 2)).action == .act)

        channel.isWorking = false
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set"])
        #expect(channel.pushed == [mine.bytes])
        #expect(sync.lastShare == SetupPeerSync.Share(hash: mine.hash, at: Date(timeIntervalSince1970: 1_000_000), failure: nil))
    }

    @Test("a lock refusal waits for a status read after it that lists nothing running")
    func lockRefusalWaitsForALaterStatus() async throws {
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash))
        channel.replies = ["set": [#"{ "ok": false, "contract": 3, "reasonCode": "operation-in-progress" }"#,
                                   #"{ "ok": true, "contract": 3, "action": "stored", "hash": "\#(mine.hash)" }"#]]
        let clock = Clock()
        let sync = Self.sync(clock)

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set"])
        // Not a failure: nothing is said, and the next status read, a moment later, sends it again.
        #expect(channel.lines.isEmpty)
        clock.now += 1
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set", "set"])
    }

    @Test("after a failed push nothing goes for a minute, and a new document starts afresh")
    func failureBacksOff() async throws {
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        let newer = try Self.document(name: "Loft Pi", revision: 3, lineage: [mine.hash, old.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash))
        channel.replies = ["set": [#"{ "ok": false, "contract": 3, "reasonCode": "internal", "message": "disk full" }"#,
                                   #"{ "ok": true, "contract": 3, "action": "stored", "hash": "\#(newer.hash)" }"#,
                                   #"{ "ok": true, "contract": 3, "action": "stored", "hash": "\#(mine.hash)" }"#]]
        let clock = Clock()
        let sync = Self.sync(clock)

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.lines.last?.contains("could not be shared with Pi") == true)
        clock.now += 59
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set"])

        // A new applied hash is not held back by the old one's failure.
        channel.applied = newer
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set", "set"])

        channel.applied = mine
        clock.now += 1
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set", "set", "set"])
    }

    @Test("a push the machine refuses as restricted is held for that document until it changes or a person asks")
    func restrictedRefusalIsHeld() async throws {
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash))
        channel.replies = ["set": [#"{ "ok": false, "contract": 3, "reasonCode": "restricted", "current": { "hash": "\#(old.hash)", "id": "\#(Self.setupId)" } }"#,
                                   #"{ "ok": true, "contract": 3, "action": "stored", "hash": "\#(mine.hash)" }"#]]
        let clock = Clock()
        let sync = Self.sync(clock)

        sync.reconcile(over: channel)
        await Self.settle(sync)
        guard case .held(let reason) = sync.sharing(holder: "Pi", local: mine, status: channel.status) else {
            Issue.record("the hold is not shown")
            return
        }
        #expect(reason.contains("whose key is not restricted"))
        // Not a failure to back off from, and not sent again, however long it waits.
        #expect(channel.lines.isEmpty)
        clock.now += 3600
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set"])

        sync.tryAgain(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set", "set"])
        #expect(sync.sharing(holder: "Pi", local: mine, status: channel.status) == .publishing)
    }

    @Test("a config-invalid refusal is held, and the row says the first problem the machine found")
    func configInvalidIsHeld() async throws {
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash))
        channel.replies = ["set": [#"{ "ok": false, "contract": 3, "reasonCode": "config-invalid", "problems": [ { "path": "machines[0]", "message": "machines[0].wake: helper is unknown" } ] }"#]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(sync.sharing(holder: "Pi", local: mine, status: channel.status)
                == .held("Pi refused this revision: machines[0].wake: helper is unknown. Nothing is sent from here again until the setup changes or you try again."))
    }

    @Test("a document with a section 9.7 error is never pushed, and still fetches")
    func invalidLocalDocumentIsNotPushed() async throws {
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        var mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        mine.errors = [DocumentAcceptance.Problem(code: "helper-unknown", path: "machines[0].wake", message: "The wake helper is gone.")]
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash))
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent.isEmpty)
        #expect(sync.sharing(holder: "Pi", local: mine, status: channel.status)
                == .failed("This device's setup breaks a rule every device holds a setup to, so it is not shared until it is fixed: The wake helper is gone."))

        let theirs = try Self.document(name: "Kitchen Pi", revision: 3, lineage: [mine.hash, old.hash])
        channel.status = Self.status(holding: theirs.hash)
        channel.replies = ["meta": [Self.meta(of: theirs)], "read": [try Self.read(of: theirs)]]
        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.adopted == [theirs.bytes])
    }

    @Test("a stale-revision refusal is filed and decided again at once")
    func staleRevisionDecidesAgain() async throws {
        let base = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [base.hash])
        // The status said the machine was behind; by the time the push arrived it had moved on to a
        // descendant of this device's document.
        let theirs = try Self.document(name: "Loft Pi", revision: 3, lineage: [mine.hash, base.hash])
        let current = #"{ "hash": "\#(theirs.hash)", "id": "\#(Self.setupId)", "revision": 3, "lineage": ["\#(mine.hash)", "\#(base.hash)"] }"#
        let channel = Channel(applied: mine, status: Self.status(holding: base.hash))
        channel.replies = ["set": [#"{ "ok": false, "contract": 3, "reasonCode": "stale-revision", "current": \#(current) }"#],
                           "read": [try Self.read(of: theirs)]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        await Self.settle(sync)
        #expect(channel.sent == ["set", "read"])
        #expect(channel.adopted == [theirs.bytes])
        #expect(channel.lines.allSatisfy { !$0.contains("could not") })
    }

    @Test("a reply from an agent the device has since replaced is dropped")
    func replyFromAReplacedAgentIsDropped() async throws {
        let old = try Self.document(name: "Pi", revision: 1, lineage: [])
        let mine = try Self.document(name: "Attic Pi", revision: 2, lineage: [old.hash])
        let channel = Channel(applied: mine, status: Self.status(holding: old.hash))
        channel.replies = ["set": [#"{ "ok": false, "contract": 3, "reasonCode": "restricted" }"#]]
        let sync = Self.sync()

        sync.reconcile(over: channel)
        sync.agentDidChange()
        // Still out, so nothing new starts beside it.
        #expect(sync.isSharing)
        await Self.settle(sync)
        #expect(sync.lastShare == nil)
        #expect(sync.sharing(holder: "Pi", local: mine, status: nil) == .unknown)
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
        let merged = SetupMerge.mergedLineage(mineHash: "mine", theirsHash: "theirs",
                                              mineLineage: ["base", "older"], theirsLineage: ["base"])
        #expect(merged == ["mine", "theirs", "base", "older"])
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
    func divergenceSentence() async throws {
        let base = try Self.document(name: "Pi", revision: 5, lineage: [])
        var mine = try Self.document(name: "Attic Pi", revision: 6, lineage: [base.hash])
        mine.identity = ControllerIdentity(id: Self.setupId, revision: 6, updatedAt: "2026-09-05T09:00:00Z",
                                           source: "mac", device: "MacBook", lineage: [base.hash])
        let theirs = #"{ "ok": true, "contract": 3, "hash": "\#(String(repeating: "c", count: 64))", "meta": { "hash": "\#(String(repeating: "c", count: 64))", "id": "\#(Self.setupId)", "revision": 6, "updatedAt": "2026-09-05T10:00:00Z", "source": "phone", "device": "Pixel", "lineage": ["\#(base.hash)"] } }"#
        let channel = Channel(applied: mine, status: Self.status(holding: String(repeating: "c", count: 64)))
        channel.replies = ["meta": [theirs]]
        let sync = SetupPeerSync()

        sync.reconcile(over: channel)
        let deadline = Date().addingTimeInterval(15)
        while sync.verdict.decision != .diverged, Date() < deadline { try await Task.sleep(for: .milliseconds(5)) }
        guard case .conflict(let sentence) = sync.sharing(holder: "Pi", local: mine, status: channel.status) else {
            Issue.record("expected a conflict")
            return
        }
        #expect(sentence.contains("MacBook"))
        #expect(sentence.contains("Pixel"))
        #expect(sentence.contains("nothing has been sent"))
        // This Mac's own agent opens the sentence too, and is not "the local agent" there.
        guard case .conflict(let local) = sync.sharing(holder: "the local agent", local: mine, status: channel.status) else {
            Issue.record("expected a conflict")
            return
        }
        #expect(local.hasPrefix("The local agent and this device"))
        #expect(local.contains(", the local agent has revision 6"))
    }

    @Test("a holder's name opens a sentence with a capital, and is left alone otherwise")
    func holderOpensASentence() {
        #expect(SetupPeerSync.opening("the local agent") == "The local agent")
        #expect(SetupPeerSync.opening("Pi") == "Pi")
        #expect(SetupPeerSync.opening("") == "")
    }
}
