import Foundation
import Observation

/// What sharing the setup needs of a device: three requests to its agent, and what the device knows.
///
/// Every device implements it the same way through its agent (AgentDeviceSetup.swift), and a test
/// can implement it with a script, which is how a read held open while a person saves an edit is
/// tested without ssh.
@MainActor
protocol SetupChannel: AnyObject, Sendable {
    /// What sentences about the agent's copy call whoever holds it.
    var setupHolder: String { get }
    /// What the agent last said in `status`, which every decision starts from.
    var status: AgentStatus? { get }
    /// True while this device has an action of its own in flight on the agent.
    var isWorking: Bool { get }
    /// The document this device applies now, or nil when it applies none or only a hand edit that
    /// has not been given a revision.
    func appliedSetup() -> ControllerDocument?
    /// Adopt bytes in place of the applied document, but only if that is still `expectedHead`.
    func commitSetup(_ bytes: Data, describedAs description: String, replacing expectedHead: String?) -> ConfigStore.AdoptOutcome
    func configMeta() async throws -> AgentConfigResult
    func configRead() async throws -> RemoteControllerDocument
    func configSet(_ document: ControllerDocument, replace: Bool) async throws -> AgentConfigResult
    /// Say something on the device's own line.
    func saySetup(_ line: String, isError: Bool, detail: String?)
    /// A transport failure in the device's own words.
    func describe(_ failure: AgentFailure) -> String
    /// Something a viewer draws has changed.
    func setupDidChange()
}

/// Sharing the setup with one device's agent: decide, act, admit, commit (section 9.5).
///
/// On every status the decision is made again from the applied hash, the machine's hash and what
/// was filed about it; nothing is remembered as handled. The guardrails decide whether the request
/// it calls for goes out now. A fetch is admitted only when the bytes hash to what was expected, pass
/// section 9.7, are this setup and descend from the applied document, and is committed as a
/// compare-and-swap: an edit saved while the read was out stays, and the next status finds the branch
/// (M16). A push the machine refuses for a reason trying again cannot fix is held for that document
/// until it changes or a person asks, and nothing is pushed while the machine is busy with an
/// operation, whoever started it (C7).
///
/// The local agent is a replica like any other. It is not privileged, it is not the source of truth,
/// and it gets the same fast-forward-or-ask treatment as a machine across the world.
@MainActor
@Observable
final class SetupPeerSync {
    /// The guardrails and what was filed about the machine's documents.
    private(set) var state = SetupSync.PeerState()
    /// What was last decided. Written by every decision; a test may set it to draw a state.
    var verdict = SetupSync.Verdict(.unknown)
    /// The last push and what it ended in: a sentence when it failed.
    private(set) var lastShare: Share?
    /// Why a push of an applied hash is held, as the person reads it.
    @ObservationIgnored private var heldReasons: [String: String] = [:]
    /// Why a remembered admission refused what the machine holds, by `SetupSync.settledKey`.
    @ObservationIgnored private var admissionNotes: [String: String] = [:]
    /// Counts the agents this device has been pointed at, so a reply from a replaced one is dropped.
    @ObservationIgnored private var generation = 0

    /// The clock the guardrails read. A test replaces it.
    @ObservationIgnored var now: @MainActor () -> Date = { Date() }

    struct Share: Sendable, Equatable {
        var hash: String
        var at: Date
        var failure: String?
    }

    /// A setup request is out.
    var isSharing: Bool { state.inFlight }

    /// What the machine said about the document it holds now: what was filed for its hash, else what
    /// its status carried.
    func remoteIdentity(for status: AgentStatus?) -> ControllerIdentity? {
        guard let copy = status?.controller else { return nil }
        if let hash = copy.hash, let meta = state.metaByHash[hash] { return meta.identity }
        return copy.identity
    }

    /// The device now runs a different agent. What the old one said goes, a refused push included,
    /// which would otherwise be held against the new agent. A request still out stays out, so the new
    /// agent's first read starts no second one beside it; its reply is dropped when it comes.
    func agentDidChange() {
        generation += 1
        let inFlight = state.inFlight
        state = SetupSync.PeerState()
        state.inFlight = inFlight
        verdict = SetupSync.Verdict(.unknown)
        lastShare = nil
        heldReasons = [:]
        admissionNotes = [:]
    }

    // MARK: - Deciding

    /// Decide what to do about the agent's copy from a status just read, and do the safe half.
    func reconcile(over channel: some SetupChannel) {
        guard let status = channel.status else { return }
        state.statusAt = now()
        state.runningOperations = status.runningOperations.count
        decide(over: channel, remote: status.controller)
    }

    /// Decide again from the status already read, because something here changed: a hold lifted, or
    /// a setup saved while the divergence sheet was open. Not a status read, so a push held after a
    /// lock refusal still waits for one.
    func decideAgain(over channel: some SetupChannel) {
        decide(over: channel, remote: channel.status?.controller)
    }

    /// One decision, and the request it calls for when the guardrails let it go. `remote` is the
    /// status's `controller` block, or the `current` of a refusal deciding again at once.
    @discardableResult
    private func decide(over channel: some SetupChannel, remote: ControllerCopy?) -> SetupSync.Verdict {
        guard let status = channel.status else { return verdict }
        let local = channel.appliedSetup()
        state.localMutation = channel.isWorking
        state.localInvalid = !(local?.errors.isEmpty ?? true)
        verdict = SetupSync.decide(local: local.map(SetupSync.Local.init), contract: status.contractVersion,
                                   remote: remote, metaByHash: state.metaByHash)
        channel.setupDidChange()
        guard let local else { return verdict }
        let gate = SetupSync.gate(decision: verdict.decision, localHash: local.hash, remoteHash: remote?.hash,
                                  agentVersion: status.version, now: now(), state: state)
        guard gate.action == .act else { return verdict }
        switch verdict.decision {
        case .push: push(local, over: channel)
        case .readMeta: readMeta(about: remote?.hash, for: local, over: channel)
        case .fetch: if let expect = verdict.expect { fetch(expecting: expect, for: local, over: channel) }
        default: break
        }
        return verdict
    }

    /// File what a read of the machine's document says about its hash, then decide again from it:
    /// the divergence sheet is built only after the machine turns out still to be on another branch,
    /// and a machine that has moved to something this device can simply take is taken from instead.
    func decideAgain(after read: VerifiedRead, over channel: some SetupChannel) -> SetupSync.Verdict {
        state.file(SetupSync.Meta(identity: read.identity), under: read.hash)
        return decide(over: channel, remote: ControllerCopy(hash: read.hash, id: read.identity.id))
    }

    /// "Try again" after a held push: the hold goes, and the decision may send it at once.
    func tryAgain(over channel: some SetupChannel) {
        guard let local = channel.appliedSetup() else { return }
        state.held[local.hash] = nil
        heldReasons[local.hash] = nil
        decideAgain(over: channel)
    }

    /// A person replaced the agent's copy with this device's document.
    func didReplace(with hash: String) {
        lastShare = Share(hash: hash, at: now(), failure: nil)
        state.failedAt[hash] = nil
        state.held[hash] = nil
        heldReasons[hash] = nil
    }

    // MARK: - Acting

    /// Send the applied document, because the machine holds nothing, a copy nobody claims, or one
    /// of its ancestors.
    private func push(_ local: ControllerDocument, over channel: some SetupChannel) {
        state.inFlight = true
        let generation = generation
        Task { @MainActor in
            var reply: AgentConfigResult?
            var failure: String?
            do {
                reply = try await channel.configSet(local, replace: false)
            } catch let error as AgentFailure {
                failure = error.detailText ?? channel.describe(error)
            } catch let invalid as AgentRequest.Invalid {
                failure = invalid.message
            } catch {
                failure = error.localizedDescription
            }
            // The agent this went to was replaced while it was out. What it made of the document says
            // nothing about the new one, which gets a push of its own.
            guard self.generation == generation else {
                self.state.inFlight = false
                channel.setupDidChange()
                return
            }
            let effect = SetupSync.afterPush(reply, localHash: local.hash)
            SetupSync.record(effect, localHash: local.hash, now: self.now(), in: &self.state)
            switch effect {
            case .clear:
                self.lastShare = Share(hash: local.hash, at: self.now(), failure: nil)
            case .fail:
                let why = failure ?? Self.refusal(reply)
                let sentence = "The setup could not be shared with \(channel.setupHolder): \(why)"
                self.lastShare = Share(hash: local.hash, at: self.now(), failure: sentence)
                channel.saySetup(sentence, isError: true, detail: nil)
            case .fileMeta(let hash, _):
                // Information, not a fault: what the machine holds is filed, and the decision is made
                // again at once from it.
                self.lastShare = nil
                self.decide(over: channel, remote: ControllerCopy(hash: hash, id: reply?.current?.id))
            case .locked:
                // The machine is busy with an operation. The push goes once a status read after this
                // one says nothing is running.
                self.lastShare = nil
            case .hold(let code):
                self.lastShare = nil
                self.heldReasons[local.hash] = code == "restricted"
                    ? "\(Self.opening(channel.setupHolder)) takes this revision only from a device whose key is not restricted. A device with a full key shares it on its next read; nothing is sent from here again until the setup changes or you try again."
                    : "\(Self.opening(channel.setupHolder)) refused this revision: \(Self.ending(reply?.problems?.first?.message ?? Self.refusal(reply))) Nothing is sent from here again until the setup changes or you try again."
            }
            channel.setupDidChange()
        }
    }

    /// Read the machine's lineage, which `status` deliberately does not carry.
    private func readMeta(about hash: String?, for local: ControllerDocument, over channel: some SetupChannel) {
        state.inFlight = true
        let generation = generation
        Task { @MainActor in
            let result = try? await channel.configMeta()
            // M9: the request is over before anything is decided from it. Deciding again while it
            // still counted as in flight held back the very fetch the read was for.
            self.state.inFlight = false
            guard self.generation == generation else {
                channel.setupDidChange()
                return
            }
            guard let result, result.ok != false else {
                self.state.failedAt[local.hash] = self.now()
                channel.setupDidChange()
                return
            }
            if let answered = result.meta?.hash ?? result.hash {
                self.state.file(SetupSync.Meta(identity: result.identity), under: answered)
                // An answer about another hash means the machine changed in between: it is filed, and
                // the next status decides.
                if answered == hash { self.decide(over: channel, remote: channel.status?.controller) }
            }
            channel.setupDidChange()
        }
    }

    /// Read the machine's document and adopt it, because this device is behind.
    private func fetch(expecting remoteHash: String, for local: ControllerDocument, over channel: some SetupChannel) {
        guard let localId = local.identity.id else { return }
        let expected = SetupSync.Expected(localHash: local.hash, localId: localId, remoteHash: remoteHash)
        let agentVersion = channel.status?.version
        state.inFlight = true
        let generation = generation
        Task { @MainActor in
            let read = await self.readDocument(over: channel)
            self.state.inFlight = false
            guard self.generation == generation else {
                channel.setupDidChange()
                return
            }
            switch read {
            case .failed(let sentence, let detail):
                self.state.failedAt[local.hash] = self.now()
                channel.saySetup(sentence, isError: true, detail: detail)
            case .read(let fetched):
                let admission = SetupSync.admit(
                    expected: expected,
                    currentLocalHash: channel.appliedSetup()?.hash,
                    fetched: SetupSync.Fetched(hash: fetched.hash, replyHash: fetched.replyHash,
                                               id: fetched.identity.id, lineage: fetched.identity.ancestors),
                    accepted: fetched.verdict.valid
                )
                self.commit(admission, of: fetched, expected: expected, agentVersion: agentVersion, over: channel)
            }
            channel.setupDidChange()
        }
    }

    private func commit(_ admission: SetupSync.Admission, of fetched: VerifiedRead, expected: SetupSync.Expected,
                        agentVersion: String?, over channel: some SetupChannel) {
        let holder = channel.setupHolder
        switch admission {
        case .headMoved:
            headMoved(over: channel)
        case .moved:
            // The machine changed between the decision and the read; the next status decides.
            break
        case .unverifiable, .invalid, .inconsistent:
            let key = SetupSync.settledKey(localHash: expected.localHash, remoteHash: expected.remoteHash, agentVersion: agentVersion)
            state.settled.insert(key)
            let note = switch admission {
            case .unverifiable:
                "The setup on \(holder) does not reproduce the hash its agent reports, so it cannot be checked and was not adopted. Its agent has to be updated before its setup can be adopted automatically."
            case .invalid:
                "\(holder) holds a newer setup that breaks a rule every device holds a setup to, so it was not adopted: \(fetched.verdict.errors.first?.message ?? "")"
            default:
                "\(holder) holds a setup whose own identity is not what its agent reported, so it was not adopted."
            }
            admissionNotes[key] = note
            channel.saySetup(note, isError: true, detail: nil)
        case .adopt:
            switch channel.commitSetup(fetched.bytes, describedAs: "adopted from \(holder)", replacing: expected.localHash) {
            case .adopted:
                channel.saySetup("Adopted the setup from \(holder): revision \(fetched.identity.revisionNumber).", isError: false, detail: nil)
            case .headMoved:
                headMoved(over: channel)
            case .refused(let problem):
                state.failedAt[expected.localHash] = now()
                channel.saySetup("The setup from \(holder) could not be adopted: \(problem)", isError: true, detail: nil)
            }
        }
    }

    /// The setup here is not the one the fetch was decided against, so nothing was written. An edit
    /// saved while the read was out stays, and deciding again finds the branch. A file saved as
    /// something that is not a setup document yet leaves this device nothing to decide from, so the
    /// row has nothing to say until it is one; the line says why the newer setup was not taken.
    private func headMoved(over channel: some SetupChannel) {
        if channel.appliedSetup() == nil {
            let holder = channel.setupHolder
            channel.saySetup("The setup file here was saved while the setup on \(holder) was being read, and it is not a setup this device can share yet, so the one on \(holder) was not adopted over it.",
                             isError: true, detail: nil)
        }
        decide(over: channel, remote: channel.status?.controller)
    }

    // MARK: - Reading the machine's document

    /// A document read off the agent, checked as far as can be without a decision: its canonical
    /// bytes and their hash, the hash the agent reported, the identity in the bytes themselves, and
    /// section 9.7 under peer intake.
    struct VerifiedRead: Sendable {
        var bytes: Data
        /// The canonical hash of the bytes.
        var hash: String
        /// The hash the reply carried. Equal to `hash` exactly when the bytes are the agent's own.
        var replyHash: String
        var identity: ControllerIdentity
        var verdict: DocumentAcceptance.Verdict

        var isVerified: Bool { hash == replyHash }
    }

    enum ReadOutcome: Sendable {
        case read(VerifiedRead)
        case failed(String, detail: String?)
    }

    /// Read `config` and take the document as the agent stored it.
    ///
    /// The `raw` text is hashed and adopted as it is (C1). An agent before 3.1 sends none, and the
    /// reply's `controller` is written out again in the order it arrived; the hash then decides
    /// whether that reproduced the stored bytes. A reply with no hash describes no document and is
    /// never adopted, and every way a read can fail is said rather than swallowed.
    func readDocument(over channel: some SetupChannel) async -> ReadOutcome {
        let holder = channel.setupHolder
        do {
            let reply = try await channel.configRead()
            guard reply.ok != false else {
                return .failed("The setup on \(holder) could not be read: \(reply.message ?? "the agent refused.")", detail: nil)
            }
            guard let replyHash = reply.hash, !replyHash.isEmpty else {
                return .failed("\(holder) sent no hash with its setup, so what came back cannot be checked and was not used.", detail: nil)
            }
            guard let document = reply.documentBytes else {
                return .failed("No setup document came back from \(holder).", detail: nil)
            }
            guard let canonical = try? Canonical.bytes(document) else {
                return .failed("The setup from \(holder) is not valid UTF-8, so it was not used.", detail: nil)
            }
            let tree = try? SetupJSON.parse(canonical, repeats: .lastWins)
            return .read(VerifiedRead(bytes: canonical, hash: Canonical.sha256(canonical), replyHash: replyHash,
                                      identity: ControllerIdentity(block: tree?["controller"]),
                                      verdict: DocumentAcceptance.check(canonical, intake: .peer)))
        } catch let error as AgentFailure {
            return .failed("The setup on \(holder) could not be read: \(channel.describe(error))", detail: error.detailText)
        } catch {
            return .failed("The setup on \(holder) could not be read: \(error.localizedDescription)", detail: nil)
        }
    }

    // MARK: - What the row says

    /// What the Setup row shows for a device.
    func sharing(holder: String, local: ControllerDocument?, status: AgentStatus?) -> SetupSharing {
        if let local {
            if state.held[local.hash] != nil, let reason = heldReasons[local.hash] { return .held(reason) }
            if let lastShare, lastShare.hash == local.hash, let failure = lastShare.failure { return .failed(failure) }
            if verdict.decision == .push, let error = local.errors.first {
                return .failed("This device's setup breaks a rule every device holds a setup to, so it is not shared until it is fixed: \(error.message)")
            }
            if verdict.decision == .fetch, let expect = verdict.expect,
               let note = admissionNotes[SetupSync.settledKey(localHash: local.hash, remoteHash: expect, agentVersion: status?.version)] {
                return .failed(note)
            }
        }
        let remote = remoteIdentity(for: status)
        switch verdict.decision {
        case .inSync: return .upToDate
        case .push: return .publishing
        case .fetch: return .fetching
        case .readMeta, .unknown: return .unknown
        case .unsupported: return .unsupported
        case .diverged:
            let mine = local?.identity
            return .conflict("""
                \(Self.opening(holder)) and this device have both changed the setup since they last agreed. \
                This device has revision \(mine?.revisionNumber ?? 0) (\(mine?.authorDescription ?? "unknown")), \
                \(holder) has revision \(remote?.revisionNumber ?? 0) (\(remote?.authorDescription ?? "unknown")). \
                Neither was made from the other, so nothing has been sent either way.
                """)
        case .differentSetup:
            return .conflict("""
                \(Self.opening(holder)) is carrying a different setup (\(remote?.name ?? remote?.id ?? "unnamed")), \
                not an older copy of this one. Nothing has been sent: replacing it is a decision.
                """)
        }
    }

    /// A holder's name where it opens a sentence. A machine's name already starts with a capital,
    /// but this Mac's own agent is "the local agent".
    static func opening(_ holder: String) -> String {
        holder.prefix(1).uppercased() + holder.dropFirst()
    }

    /// The agent's own words, closed as a sentence.
    private static func ending(_ text: String) -> String {
        guard let last = text.last, !".!?".contains(last) else { return text }
        return text + "."
    }

    /// A refusal or an unreadable answer, as the end of a sentence.
    private static func refusal(_ reply: AgentConfigResult?) -> String {
        guard let reply else { return "no answer came back." }
        if reply.ok == true { return "the agent stored something other than the document it was given." }
        if reply.ok == nil { return "the agent's answer could not be read." }
        return reply.reason?.sentence(subject: "the setup", message: reply.error ?? reply.message)
            ?? reply.error ?? reply.message ?? "the agent kept the copy it had."
    }
}

extension ControllerIdentity {
    /// The identity a document's own controller block gives, or none when it has no readable one.
    init(block: SetupJSON?) {
        self = block.flatMap { try? JSONDecoder().decode(ControllerIdentity.self, from: $0.documentData) } ?? ControllerIdentity()
    }
}
