import Foundation

/// Deciding what to do when a machine is holding a different setup document from this device:
/// section 9.5 of the contract.
///
/// Every device is a peer: any of them may edit the setup, and none of them is on all the time.
/// Revision numbers cannot tell "newer" from "different" — two devices that both edit revision 5
/// while apart both produce a revision 6 — so the document carries the hashes of the documents it
/// was made from, and descent decides:
///
///   - the machine holds one of my ancestors  → it is behind me, push, automatically
///   - I hold one of the machine's ancestors  → I am behind it, fetch, automatically
///   - neither                                → we have diverged, and a person decides
///
/// Both automatic moves follow strict descent, and descent is acyclic, so two devices can never take
/// turns overwriting each other. Everything else stops and asks.
///
/// Each step is one pure function, ported from `contract/tools/setup-sync.mjs` and held to
/// `contract/vectors/setup-reconcile.json`: `decide` (the eleven rules), `admit` (whether fetched
/// bytes may replace the applied document), `gate` (the guardrails) and `afterPush` (what a push
/// reply means). SetupPeerSync.swift runs them against a machine. They replace a decision written
/// from the status alone that adopted fetched bytes over whatever had been saved meanwhile (M16),
/// and a retry rule that held back only this device's own pushes for one document (C7).
enum SetupSync {

    /// How long nothing automatic goes to a machine for the same applied hash after a failed request.
    static let backoff: TimeInterval = 60

    // MARK: - Deciding

    enum Decision: String, Sendable, Equatable {
        /// An agent that cannot carry the setup. Left out, never a conflict.
        case unsupported
        /// Nothing to decide from: no applied document here, or one without a setup id.
        case unknown
        /// Send this device's document: the machine holds nothing, a copy nobody claims, or an
        /// ancestor of it.
        case push
        /// The machine holds exactly this document.
        case inSync
        /// Two different setups. A person chooses which one this fleet is on.
        case differentSetup
        /// Status does not carry the lineage: read `config meta` once, then decide again.
        case readMeta
        /// This device is behind: read the machine's document and adopt it.
        case fetch
        /// Same setup, neither side descends from the other. A person has to look.
        case diverged
    }

    /// A decision, why it was taken, and for a fetch the hash the fetched bytes must have.
    struct Verdict: Sendable, Equatable {
        var decision: Decision
        var reason: String?
        var expect: String?

        init(_ decision: Decision, reason: String? = nil, expect: String? = nil) {
            self.decision = decision
            self.reason = reason
            self.expect = expect
        }
    }

    /// This device's applied document, as the decision reads it.
    struct Local: Sendable, Equatable {
        var hash: String
        var id: String?
        var lineage: [String]

        init(hash: String, id: String?, lineage: [String]) {
            self.hash = hash
            self.id = id
            self.lineage = lineage
        }

        init(_ document: ControllerDocument) {
            self.init(hash: document.hash, id: document.identity.id, lineage: document.identity.ancestors)
        }
    }

    /// What a machine said about a document it holds, filed under that document's hash: a `config
    /// meta` answer, or the `current` of a refused push. A hash fixes the document, and with it the
    /// setup id and the lineage, so an entry never goes stale.
    struct Meta: Sendable, Equatable {
        /// Everything that was said, for the sentences; the decision reads only the id and lineage.
        var identity: ControllerIdentity

        var id: String? { identity.id }
        var lineage: [String] { identity.ancestors }

        init(identity: ControllerIdentity) { self.identity = identity }

        init(id: String?, lineage: [String]) { self.identity = ControllerIdentity(id: id, lineage: lineage) }
    }

    /// The first of the eleven rules that matches.
    ///
    /// `contract` and `remote` are the status reply's: the contract the agent speaks, nil before 3,
    /// and its `controller` block, nil when it sent none. Rule 7 is checked before rule 8 on
    /// purpose: a machine holding another setup whose hash happens to be in this device's lineage is
    /// still another setup, and pushing over it would replace somebody else's fleet.
    static func decide(local: Local?, contract: Int?, remote: ControllerCopy?, metaByHash: [String: Meta]) -> Verdict {
        guard let contract, contract >= 3, let remote else { return Verdict(.unsupported) }
        guard let local else { return Verdict(.unknown, reason: "no-local") }
        guard let localId = local.id, !localId.isEmpty else { return Verdict(.unknown, reason: "local-unidentified") }
        guard let remoteHash = remote.hash, !remoteHash.isEmpty else { return Verdict(.push, reason: "machine-empty") }
        if remoteHash == local.hash { return Verdict(.inSync) }
        let meta = metaByHash[remoteHash]
        // What was filed about the hash outranks the status: meta carries the id the bytes have.
        let id: String? = if let meta { meta.id } else { remote.id }
        guard let id, !id.isEmpty else { return Verdict(.push, reason: "machine-unidentified") }
        if id != localId { return Verdict(.differentSetup) }
        if local.lineage.contains(remoteHash) { return Verdict(.push, reason: "machine-behind") }
        guard let meta else { return Verdict(.readMeta) }
        if meta.lineage.contains(local.hash) { return Verdict(.fetch, expect: remoteHash) }
        return Verdict(.diverged)
    }

    // MARK: - Admitting a fetch

    enum Admission: String, Sendable, Equatable {
        /// The applied document changed while the bytes were read. Nothing is written; decide again.
        case headMoved
        /// The machine changed since the decision. Decide on the next status.
        case moved
        /// The bytes do not reproduce the hash the machine reports, so they cannot be verified.
        case unverifiable
        /// The bytes break section 9.7.
        case invalid
        /// The bytes are not this setup, or do not descend from the applied document.
        case inconsistent
        case adopt
    }

    /// What the decision to fetch was made from.
    struct Expected: Sendable, Equatable {
        var localHash: String
        var localId: String
        var remoteHash: String
    }

    /// What a fetch read: the canonical hash of the bytes, the `hash` the reply carried, and the id
    /// and lineage of the bytes' own controller block.
    struct Fetched: Sendable, Equatable {
        var hash: String?
        var replyHash: String
        var id: String?
        var lineage: [String]
    }

    /// Whether fetched bytes may be adopted, in this order: `headMoved`, `moved`, `unverifiable`,
    /// `invalid`, `inconsistent`, `adopt`.
    ///
    /// The head check is first because it is the one that protects a person: an edit saved while the
    /// read was out must never be overwritten by bytes chosen against the document it replaced. A
    /// hash that is not the expected one but is the reply's own means the machine moved on; one that
    /// is neither means the bytes could not be reproduced, which is what an agent before 3.1, which
    /// sends no `raw`, gives a client that rebuilt them in another spelling (C1). A reply without a
    /// hash never gets here: it describes no document and counts as a failed request.
    static func admit(expected: Expected, currentLocalHash: String?, fetched: Fetched, accepted: Bool) -> Admission {
        if currentLocalHash != expected.localHash { return .headMoved }
        if fetched.hash != expected.remoteHash { return fetched.replyHash == fetched.hash ? .moved : .unverifiable }
        if !accepted { return .invalid }
        if fetched.id != expected.localId || !fetched.lineage.contains(expected.localHash) { return .inconsistent }
        return .adopt
    }

    /// The admissions a device remembers, so it does not read the same refused document on every
    /// status. Nothing else is remembered as handled: the decision is recomputed from the hashes.
    static let rememberedAdmissions: Set<Admission> = [.invalid, .unverifiable, .inconsistent]

    /// The key a remembered admission is filed under. It changes when either hash or the agent does,
    /// so a machine whose agent is updated is read again.
    static func settledKey(localHash: String, remoteHash: String, agentVersion: String?) -> String {
        "\(localHash) \(remoteHash) \(agentVersion ?? "null")"
    }

    // MARK: - The guardrails

    /// What is held per machine, in memory only. Times are this device's own clock.
    struct PeerState: Sendable, Equatable {
        /// `config meta` answers and refusals' `current`, by the hash they describe. The four most
        /// recent: a machine moves through few documents between two of this device's edits.
        var metaByHash: [String: Meta] = [:]
        var metaOrder: [String] = []
        /// A setup request is out.
        var inFlight = false
        /// When a request about this applied hash last failed.
        var failedAt: [String: Date] = [:]
        /// The refusal code a push of this applied hash was held after.
        var held: [String: String] = [:]
        /// This device's applied document breaks section 9.7.
        var localInvalid = false
        /// This device has an action of its own in flight on the machine.
        var localMutation = false
        /// What the machine's latest status lists as running.
        var runningOperations = 0
        /// When a push was last refused because the machine's lock was held.
        var lockedAt: Date?
        /// When the latest status was read.
        var statusAt: Date?
        /// Remembered admissions, by settledKey.
        var settled: Set<String> = []

        static let keptMeta = 4

        mutating func file(_ meta: Meta, under hash: String) {
            metaByHash[hash] = meta
            metaOrder.removeAll { $0 == hash }
            metaOrder.append(hash)
            while metaOrder.count > Self.keptMeta { metaByHash[metaOrder.removeFirst()] = nil }
        }
    }

    enum GateAction: String, Sendable, Equatable {
        case act
        case hold
        case none
    }

    struct Gate: Sendable, Equatable {
        var action: GateAction
        var reason: String?

        init(_ action: GateAction, reason: String? = nil) {
            self.action = action
            self.reason = reason
        }
    }

    /// Whether the request a decision calls for goes out now.
    ///
    /// Only a push, a meta read and a fetch send anything. One request is in flight per machine, and
    /// a failure holds everything automatic for the same applied hash for a minute. The rest applies
    /// to pushes only, because reading a machine never changes it: a device whose own document breaks
    /// section 9.7 publishes nothing, a push the machine refused with `restricted` or
    /// `config-invalid` is not sent again for that hash, and nothing is pushed while this device has
    /// an action in flight on the machine, while its last status lists a running operation — whoever
    /// started it — or after a lock refusal until a status read after it says nothing is running.
    static func gate(decision: Decision, localHash: String, remoteHash: String?, agentVersion: String?,
                     now: Date, state: PeerState) -> Gate {
        guard [.push, .readMeta, .fetch].contains(decision) else { return Gate(.none) }
        if state.inFlight { return Gate(.hold, reason: "in-flight") }
        if let failed = state.failedAt[localHash], now.timeIntervalSince(failed) < backoff {
            return Gate(.hold, reason: "backoff")
        }
        if decision == .push {
            if state.localInvalid { return Gate(.hold, reason: "local-invalid") }
            if state.held[localHash] != nil { return Gate(.hold, reason: "held") }
            let lockStillHeld = state.lockedAt.map { locked in state.statusAt.map { $0 <= locked } ?? true } ?? false
            if state.localMutation || state.runningOperations > 0 || lockStillHeld {
                return Gate(.hold, reason: "operation-running")
            }
        }
        if decision == .fetch, let remoteHash,
           state.settled.contains(settledKey(localHash: localHash, remoteHash: remoteHash, agentVersion: agentVersion)) {
            return Gate(.hold, reason: "settled")
        }
        return Gate(.act)
    }

    // MARK: - Push replies

    enum PushEffect: Sendable, Equatable {
        /// The machine holds the applied document now.
        case clear
        /// A failure to back off from.
        case fail
        /// The refusal says what the machine holds: file it under its hash and decide again at once.
        case fileMeta(hash: String, meta: Meta)
        /// The machine is busy with an operation: try again once a later status says it is done.
        case locked
        /// A refusal trying again cannot fix: hold the push for this hash.
        case hold(String)

        /// The contract's name for the effect.
        var name: String {
            switch self {
            case .clear: "clear"
            case .fail: "fail"
            case .fileMeta: "fileMeta"
            case .locked: "locked"
            case .hold: "hold"
            }
        }
    }

    /// What a push reply means. `reply` is nil when none was read: no answer, a dropped link, a
    /// timeout, output that is not a reply. That and every refusal not named here is a failure to
    /// back off from.
    ///
    /// Two readings a port might find more natural are failures on purpose. Only a reply with
    /// `ok: false` is a refusal, because the agent sends one with every refusal code. And a `current`
    /// without a hash has nothing to be filed under, so it says nothing about what the machine holds.
    /// A reply that stored something other than the applied hash is a failure too: the machine now
    /// holds a document this device did not send.
    static func afterPush(_ reply: AgentConfigResult?, localHash: String) -> PushEffect {
        guard let reply else { return .fail }
        if reply.ok == true { return reply.hash == localHash ? .clear : .fail }
        guard reply.ok == false else { return .fail }
        switch reply.reasonCode {
        case "stale-revision", "controller-conflict":
            guard let current = reply.current, let hash = current.hash else { return .fail }
            return .fileMeta(hash: hash, meta: Meta(identity: current.identity))
        case "operation-in-progress", "lock-held":
            return .locked
        case "restricted", "config-invalid":
            return .hold(reply.reasonCode ?? "")
        default:
            return .fail
        }
    }

    /// The state after a push reply. `clear` forgets the failure, `fail` starts the back-off,
    /// `fileMeta` files `current` under its hash, `locked` waits for a later status, `hold` remembers
    /// the refusal for the applied hash. "Try again" deletes the hold, and nothing else clears one but
    /// a new applied hash.
    static func record(_ effect: PushEffect, localHash: String, now: Date, in state: inout PeerState) {
        state.inFlight = false
        switch effect {
        case .clear: state.failedAt[localHash] = nil
        case .fail: state.failedAt[localHash] = now
        case .fileMeta(let hash, let meta): state.file(meta, under: hash)
        case .locked: state.lockedAt = now
        case .hold(let code): state.held[localHash] = code
        }
    }
}
