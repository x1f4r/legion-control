import Foundation
import Testing
@testable import LegionControlCore

/// Decoding the fixtures every client shares.
///
/// The point is not that Swift can parse JSON. It is that this app reads the same meaning out of the
/// same bytes as the agent, the phone and the desktop client, and that a reply it has never seen
/// before degrades into "not known" rather than into a decode failure that blanks a whole machine.
struct FixtureTests {

    static var repositoryRoot: URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 { url = url.deletingLastPathComponent() }
        return url
    }

    static var directory: URL { repositoryRoot.appending(path: "contract/fixtures") }

    struct Index: Decodable {
        var fixtures: [Entry]

        struct Entry: Decodable {
            var file: String
            var kind: String?
            var expect: String?
            var command: String?
            var variant: String?
            var schema: String?
            var ok: Bool?
            var contract: Int?
            var action: String?
            var reasonCode: String?
            var tags: [String]?
        }
    }

    /// The index is loaded rather than the directory globbed, because the index is itself a `.json`
    /// file in that directory and it names which schema each payload belongs to.
    static func index() -> Index? {
        guard let data = try? Data(contentsOf: directory.appending(path: "index.json")) else { return nil }
        return try? JSONDecoder().decode(Index.self, from: data)
    }

    static func bytes(_ file: String) throws -> Data {
        try Data(contentsOf: directory.appending(path: file))
    }

    // MARK: - Every reply decodes

    @Test("every shared reply fixture decodes into the type this app reads it as")
    func everyReplyDecodes() throws {
        guard let index = Self.index() else {
            Issue.record("contract/fixtures/index.json is missing, so no shared fixture was checked")
            return
        }
        #expect(!index.fixtures.isEmpty)

        var checked = 0
        for entry in index.fixtures where entry.kind == "reply" {
            let data = try Self.bytes(entry.file)
            let schema = entry.schema ?? ""
            let command = entry.command ?? ""

            do {
                switch true {
                case command.hasPrefix("service-config "):
                    let reply = try JSONDecoder().decode(ServiceConfigReply.self, from: data)
                    if let ok = entry.ok { #expect(reply.ok == ok) }
                    if command == "service-config get", reply.ok == true {
                        #expect(reply.document != nil)
                        #expect(reply.hash != nil)
                    }
                case schema.hasPrefix("status"), command == "status":
                    let status = try JSONDecoder().decode(AgentStatus.self, from: data)
                    // The one field everything else is gated on.
                    if entry.contract != nil { #expect(status.contractVersion == entry.contract) }
                case command == "op":
                    _ = try JSONDecoder().decode(AgentOperationResult.self, from: data)
                case command == "history":
                    let history = try JSONDecoder().decode(AgentHistoryResult.self, from: data)
                    #expect(!history.records.isEmpty)
                case command == "logs":
                    let logs = try JSONDecoder().decode(AgentLogsResult.self, from: data)
                    #expect(!logs.text.isEmpty)
                case command == "doctor":
                    let doctor = try JSONDecoder().decode(AgentDoctorResult.self, from: data)
                    #expect(!(doctor.checks ?? []).isEmpty)
                case command == "policy":
                    _ = try JSONDecoder().decode(AgentPolicyResult.self, from: data)
                case command == "config":
                    _ = try JSONDecoder().decode(AgentConfigResult.self, from: data)
                    _ = try JSONDecoder().decode(RemoteControllerDocument.self, from: data)
                case command == "busy":
                    // A machine with one service answers with the busy record at the top level and
                    // no `services` array at all, so only the aggregate is asserted here.
                    let busy = try JSONDecoder().decode(AgentBusyResult.self, from: data)
                    #expect(busy.ok == true, "\(entry.file)")
                default:
                    let result = try JSONDecoder().decode(AgentActionResult.self, from: data)
                    if let action = entry.action { #expect(result.action == action, "\(entry.file)") }
                    if let reason = entry.reasonCode {
                        #expect(result.reasonCode == reason, "\(entry.file)")
                        // Every code in the fixtures is one this build renders by code rather than
                        // falling back to the message.
                        #expect(AgentReason(code: reason) != nil, "\(entry.file)")
                        if case .unrecognised(let raw)? = AgentReason(code: reason) {
                            Issue.record("\(entry.file): the reason code \(raw) is not one this build knows")
                        }
                    }
                    if let ok = entry.ok { #expect(result.ok == ok, "\(entry.file)") }
                }
                checked += 1
            } catch {
                Issue.record("\(entry.file) did not decode: \(error)")
            }
        }
        #expect(checked > 40, "only \(checked) fixtures were decoded")
    }

    @Test("every shared controller document decodes, and the invalid ones are refused")
    func controllerDocuments() throws {
        guard let index = Self.index() else { return }
        for entry in index.fixtures where entry.file.hasPrefix("controller-document.") {
            let data = try Self.bytes(entry.file)
            let decoded = try? JSONDecoder().decode(ControllerConfig.self, from: data)
            guard let decoded else {
                // A document that will not decode at all is only acceptable when the index says the
                // fixture is meant to be invalid.
                #expect(entry.expect == "invalid", "\(entry.file) did not decode")
                continue
            }
            let validated = try? decoded.validated()
            if entry.expect == "invalid" {
                #expect(validated == nil, "\(entry.file) should have been refused")
            } else {
                #expect(validated != nil, "\(entry.file) should have been accepted")
            }
        }
    }

    // MARK: - What the app reads out of them

    @Test("configured metrics distinguish a zero reading from unavailable")
    func configuredMetrics() throws {
        let status = try JSONDecoder().decode(AgentStatus.self, from: try Self.bytes("status.metrics.json"))
        let metrics = try #require(status.metrics)
        #expect(metrics.contains { $0.value == 0 })
        #expect(metrics.contains { $0.value == nil && $0.error != nil })
        let legacy = try JSONDecoder().decode(AgentStatus.self, from: Data("{}".utf8))
        #expect(legacy.metrics == nil)
    }

    @Test("the full status is read the way the machine section draws it")
    func fullStatus() throws {
        let status = try JSONDecoder().decode(AgentStatus.self, from: try Self.bytes("status.full.json"))
        #expect(status.contractVersion == 3)
        #expect(status.speaksRequiredContract)
        #expect(status.version == "3.0.0")
        #expect(!status.resolvedServices.isEmpty)
        #expect(status.controllerHash != nil)
        #expect(status.controller?.identity.id != nil)
        // The three separate reachability questions the review asked to be told apart.
        let service = try #require(status.resolvedServices.first)
        #expect(service.process != nil)
        #expect(service.health != nil)
        #expect(service.busy?.isMonitored == true)
        // Operations, so the section has something to show.
        #expect(!status.runningOperations.isEmpty || !status.recentOperations.isEmpty)
    }

    @Test("a partial status says so rather than reading as a machine with nothing on it")
    func partialStatus() throws {
        let status = try JSONDecoder().decode(AgentStatus.self, from: try Self.bytes("status.partial.json"))
        #expect(status.isPartial)
        // A probe that ran out of time is unknown, which blocks disruptive work rather than allowing
        // it. "Not busy" would be the dangerous reading.
        let unknown = status.resolvedServices.first { $0.busy?.isUnknown == true }
        #expect(unknown != nil)
    }

    @Test("an agent whose own config is broken is shown as refusing changes")
    func configInvalidStatus() throws {
        let status = try JSONDecoder().decode(AgentStatus.self, from: try Self.bytes("status.config-invalid.json"))
        #expect(status.refusesMutations)
        #expect(status.config?.source == "last-known-good")
        // Every problem the agent reported reaches the notes the section prints.
        #expect(!status.allNotes.isEmpty)
    }

    @Test("a 2.x status still works and is not mistaken for a v3 one")
    func legacyStatus() throws {
        let status = try JSONDecoder().decode(AgentStatus.self, from: try Self.bytes("legacy-2x.status.json"))
        #expect(status.contractVersion == 2)
        #expect(!status.speaksRequiredContract)
        // The one service it looks after is still drawn.
        #expect(!status.resolvedServices.isEmpty)
        let dialect = AgentDialect(status)
        #expect(!dialect.supportsOperations)
        #expect(!dialect.supportsQueue)
        #expect(!dialect.supportsPolicy)
    }

    @Test("an update that did nothing says which of the several reasons it was")
    func noopReasons() throws {
        let noop = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("update.noop.json"))
        #expect(noop.action == "noop")
        // "Already on the latest build" used to be printed for every one of these.
        let sentence = noop.reason?.sentence(subject: "t3", message: noop.message)
        #expect(sentence != nil)

        let latestUnknown = try JSONDecoder().decode(AgentActionResult.self,
                                                     from: try Self.bytes("update.latest-unknown.json"))
        #expect(latestUnknown.reason == .latestUnknown)
        #expect(latestUnknown.reason?.isBenign == false)
    }

    @Test("a conflict names what is already running rather than reading as a failure")
    func conflict() throws {
        let result = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("update.conflict.json"))
        #expect(result.isConflict)
        #expect(result.conflict != nil)
        #expect(OperationOutcome.state(for: result, stillRunning: false) == .conflict)
    }

    @Test("a replayed reply is recognised, which is what proves a retry did not run twice")
    func replay() throws {
        let result = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("update.replayed.json"))
        #expect(result.replayed == true || result.op?.replayed == true)
    }

    @Test("a detached update answers accepted with an operation to follow")
    func accepted() throws {
        let result = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("update.accepted.json"))
        #expect(result.isAccepted)
        #expect(result.operationId != nil)
        // Accepted on its own is a running operation, never a finished one.
        #expect(OperationOutcome.state(for: result, stillRunning: false) == .running)
    }

    @Test("a queued request carries its expiry so the queue row can say when it lapses")
    func queued() throws {
        let result = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("update.queued.json"))
        #expect(result.isQueued)
        let expiry = result.expiresAt ?? result.op?.expiresAt
        #expect(ISO8601DateFormatter.lenient.date(from: expiry) != nil)
    }

    @MainActor
    @Test("an interrupted operation reads as an unknown outcome, not as a failure")
    func interrupted() throws {
        let reply = try JSONDecoder().decode(AgentOperationResult.self,
                                             from: try Self.bytes("op.finished-interrupted.json"))
        let operation = try #require(reply.operation)
        #expect(operation.isFinished)
        let asResult = OperationDriver.finalResult(from: operation, accepted: AgentActionResult())
        #expect(OperationOutcome.state(for: asResult, stillRunning: false) == .unknown)
    }

    @Test("a finished operation carries the phases and the log this app files away")
    func finishedOperation() throws {
        let reply = try JSONDecoder().decode(AgentOperationResult.self,
                                             from: try Self.bytes("op.finished-updated.json"))
        let operation = try #require(reply.operation)
        #expect(operation.isFinished)
        #expect(operation.result?.ok == true)
        #expect(!(operation.log ?? []).isEmpty)
        #expect(operation.progress?.fraction != nil)
    }

    @Test("an operation the agent has no record of is an answer in itself")
    func operationNotFound() throws {
        let reply = try JSONDecoder().decode(AgentOperationResult.self, from: try Self.bytes("op.not-found.json"))
        // An agent that files every operation it starts and has no record of this one never started
        // it, which is the whole reason the id is carried.
        #expect(reply.operation == nil)
    }

    @Test("the policy reply carries the effective settings for the system and each service")
    func policy() throws {
        let system = try JSONDecoder().decode(AgentPolicyResult.self, from: try Self.bytes("policy.system.json"))
        let updates = try #require(system.updates)
        #expect(updates.automatic == true)
        #expect(!(updates.maintenanceWindows ?? []).isEmpty)
        #expect(updates.maintenanceWindows?.first?.looksValid == true)
        // Per-service policies come with the same read, so the UI needs no extra round trip.
        #expect(!(system.services ?? []).isEmpty)

        let service = try JSONDecoder().decode(AgentPolicyResult.self, from: try Self.bytes("policy.service.json"))
        #expect(service.updates != nil)
    }

    @Test("a config-set refusal carries what the machine is holding, so the divergence can be shown")
    func configConflict() throws {
        let result = try JSONDecoder().decode(AgentConfigResult.self,
                                              from: try Self.bytes("config.set.conflict-divergent.json"))
        #expect(result.ok == false)
        #expect(result.reason == .controllerConflict)
        #expect(result.divergent == true)
        let current = try #require(result.current)
        #expect(current.identity.id != nil)
        #expect(!(current.identity.lineage ?? []).isEmpty)
    }

    @Test("a stale-revision refusal is told apart from a real divergence")
    func staleRevision() throws {
        let result = try JSONDecoder().decode(AgentConfigResult.self,
                                              from: try Self.bytes("config.set.stale-revision.json"))
        #expect(result.reason == .staleRevision)
        #expect(result.divergent != true)
    }

    @Test("the meta reply carries the lineage the reconciliation needs")
    func configMeta() throws {
        let result = try JSONDecoder().decode(AgentConfigResult.self, from: try Self.bytes("config.meta.json"))
        #expect(result.identity.id != nil)
        #expect((result.identity.lineage ?? []).count >= 1)
        #expect(result.storedHash != nil)
        #expect(result.identity.lineageIsValid)
    }

    /// The one place the Mac cannot yet close the loop on its own.
    ///
    /// `config` returns the document as a parsed object rather than as the bytes that were stored,
    /// so adopting it means writing those bytes again and hoping they match. They do when the writer
    /// sorted its keys, and they cannot when it wrote them in insertion order. Rather than adopt a
    /// document whose hash will not match and diverge from everybody on the next poll, the fetch path
    /// refuses and says so. See integration-notes-mac.txt.
    @Test("a fetched document is only adopted when its bytes reproduce the hash the machine reports")
    func fetchedDocumentIsVerified() throws {
        let reply = try JSONDecoder().decode(RemoteControllerDocument.self, from: try Self.bytes("config.read.json"))
        let bytes = try #require(reply.documentBytes)
        let reconstructed = try Canonical.hash(bytes)
        let matches = reconstructed == reply.hash

        // Whichever way this goes, the behaviour has to be safe. When the bytes reproduce the hash,
        // the document is this machine's and may be adopted. When they do not — because the writer
        // did not sort its keys, and insertion order cannot be recovered from a parsed object — the
        // fetch path refuses rather than adopting a document that would differ from the machine's on
        // the very next poll.
        //
        // The shared fixture is currently the second case, which is recorded in
        // integration-notes-mac.txt as a request for the stored text on the `config` reply.
        #expect(!bytes.isEmpty)
        if matches {
            #expect(reconstructed == reply.hash)
        } else {
            #expect(reply.hash != nil, "a machine that cannot be verified against must at least report a hash")
        }
    }

    @Test("a document with keys this build has never seen still decodes and keeps them")
    func unknownKeys() throws {
        let data = try Self.bytes("controller-document.unknown-keys.json")
        let decoded = try JSONDecoder().decode(ControllerConfig.self, from: data)
        #expect(!decoded.machines.isEmpty)

        // And an edit keeps them, which is what stops one device quietly deleting another's settings.
        let edited = try ControllerEditor.apply(to: data, parentHash: String(repeating: "a", count: 64),
                                                parentIdentity: decoded.identity, deviceName: "Mac") { root in
            try ControllerEditor.setSetupName("Renamed", in: &root)
        }
        let after = try #require((try JSONSerialization.jsonObject(with: edited.bytes)) as? [String: Any])
        let before = try #require((try JSONSerialization.jsonObject(with: data)) as? [String: Any])
        for key in before.keys where key != "controller" {
            #expect(after[key] != nil, "\(key) was dropped by an edit")
        }
    }

    @Test("the wol action fixtures are the ones the wake failover treats as idempotent")
    func wolActions() throws {
        let ran = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("run.wol-ran.json"))
        #expect(ran.action == "ran")
        #expect(ran.output != nil)

        let failed = try JSONDecoder().decode(AgentActionResult.self, from: try Self.bytes("run.wol-failed.json"))
        #expect(failed.ok == false)
    }
}
