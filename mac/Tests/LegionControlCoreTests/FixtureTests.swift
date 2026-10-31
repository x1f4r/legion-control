import Foundation
import Testing
@testable import LegionControlCore

/// Decoding the fixtures every client shares.
///
/// The point is not that Swift can parse JSON. It is that this app reads the same meaning out of the
/// same bytes as the agent, the phone and the desktop client, and that a reply it has never seen
/// before degrades into "not known" rather than into a decode failure that blanks a whole machine.
struct FixtureTests {

    /// One entry of `contract/fixtures/index.json`.
    struct Entry: Decodable, Sendable {
        var file: String
        var kind: String
        var expect: String
        var command: String?
        var variant: String?
        var schema: String
        var ok: Bool?
        var exitCode: Int?
        var contract: Int?
        var action: String?
        var reasonCode: String?
        var outcome: String?
        var tags: [String]?
    }

    struct Index: Decodable {
        var contract: Int
        var fixtures: [Entry]
    }

    /// The index is loaded rather than the directory globbed (section 14.1): it is itself a `.json`
    /// file in that directory, it names which schema each payload belongs to, and a fixture it does
    /// not list is not one a test should be reading.
    static func index() throws -> Index {
        try JSONDecoder().decode(Index.self, from: ContractCorpus.data("fixtures/index.json"))
    }

    static func bytes(_ file: String) throws -> Data {
        try ContractCorpus.data("fixtures/\(file)")
    }

    // MARK: - Every entry of the index

    /// What this app read out of one reply: the envelope every reply is checked on, and the fields
    /// its schema's projection names (section 14.1 and S6.7), as the decoded model has them.
    struct Reading: Sendable {
        var ok: Bool?
        var action: String?
        var reasonCode: String?
        var outcome: Outcome?
        /// Whether the client's contract gate takes it for contract 3, for a status.
        var speaksContract3: Bool?
        var projection: VectorJSON = .null
    }

    typealias Handler = @Sendable (_ data: Data, _ entry: Entry) throws -> Reading

    static func envelope(_ reply: AgentActionResult) -> Reading {
        Reading(ok: reply.ok, action: reply.action, reasonCode: reply.reasonCode, outcome: Outcome.of(reply))
    }

    static func reduced(outcome: String?, ok: Bool?, action: String? = nil, reasonCode: String?) -> Outcome {
        Outcome.reduce(Outcome.Evidence(outcome: outcome, state: nil, ok: ok, action: action, reasonCode: reasonCode))
    }

    static func strings(_ values: [String?]) -> VectorJSON { .array(values.map(VectorJSON.init)) }

    /// A reply the app reads through the envelope every mutation goes through.
    static let mutation: Handler = { data, _ in envelope(try AgentReplyDecoder.decode(AgentActionResult.self, from: data)) }

    static let status: Handler = { data, entry in
        let status = try AgentReplyDecoder.decode(AgentStatus.self, from: data)
        var reading = Reading(ok: status.ok, reasonCode: status.reasonCode, speaksContract3: status.speaksRequiredContract)
        if entry.schema == "status.schema.json" {
            reading.projection = [
                "services": strings(status.resolvedServices.map(\.id)),
                "controllerHash": VectorJSON(status.controller?.hash),
                "running": strings(status.runningOperations.map(\.id)),
                "processUnknown": .array(status.resolvedServices.map { .bool($0.process?.isUnknown ?? false) }),
            ]
        }
        return reading
    }

    static let logs: Handler = { data, _ in
        let logs = try AgentReplyDecoder.decode(AgentLogsResult.self, from: data)
        return Reading(ok: logs.ok, reasonCode: logs.reasonCode, projection: ["lines": strings((logs.lines ?? []).map(\.line))])
    }

    /// The decoder this app uses for each schema. A refusal in the error shape is read by the
    /// decoder of the command it answers, because that is the one that meets it.
    static let handlers: [String: Handler] = [
        "status.schema.json": status,
        "legacy-2x-status.schema.json": status,
        "busy.schema.json": { data, _ in
            let busy = try AgentReplyDecoder.decode(AgentBusyResult.self, from: data)
            let records: [VectorJSON] = busy.services.map { services in
                services.map { ["id": .string($0.id), "monitored": VectorJSON($0.monitored), "evidence": VectorJSON($0.evidence)] }
            } ?? [["id": nil, "monitored": VectorJSON(busy.monitored), "evidence": VectorJSON(busy.evidence)]]
            return Reading(ok: busy.ok, reasonCode: busy.reasonCode, projection: .array(records))
        },
        "update.schema.json": mutation,
        "restart.schema.json": mutation,
        "boot.schema.json": mutation,
        "sleep.schema.json": mutation,
        "run.schema.json": mutation,
        "cancel.schema.json": mutation,
        "self-update.schema.json": mutation,
        "legacy-2x-mutation.schema.json": mutation,
        // This app sends `version` and reads only the envelope of it; it never sends `help`, and
        // reads a `bundle` as the text it saves. All three answer in the envelope every reply has.
        "version.schema.json": mutation,
        "help.schema.json": mutation,
        "bundle.schema.json": { data, entry in
            _ = try AgentReplyDecoder.decode(RawJSON.self, from: data)
            return try mutation(data, entry)
        },
        "cycle.schema.json": { data, entry in
            let reply = try AgentReplyDecoder.decode(AgentActionResult.self, from: data)
            var reading = envelope(reply)
            reading.projection = .array((reply.children ?? []).map { ["service": VectorJSON($0.service), "opId": VectorJSON($0.opId)] })
            return reading
        },
        "op.schema.json": { data, _ in
            let reply = try AgentReplyDecoder.decode(AgentOperationResult.self, from: data)
            let projection: VectorJSON = reply.isUnknownOperation
                ? ["noRecord": true]
                : ["id": VectorJSON(reply.operation?.id), "state": VectorJSON(reply.operation?.state)]
            return Reading(ok: reply.ok, reasonCode: reply.reasonCode, projection: projection)
        },
        "history.schema.json": { data, _ in
            let history = try AgentReplyDecoder.decode(AgentHistoryResult.self, from: data)
            return Reading(ok: history.ok, reasonCode: history.reasonCode, projection: strings(history.records.map(\.id)))
        },
        "logs.schema.json": logs,
        "doctor.schema.json": { data, _ in
            let doctor = try AgentReplyDecoder.decode(AgentDoctorResult.self, from: data)
            return Reading(ok: doctor.ok, reasonCode: doctor.reasonCode, projection: [
                "checks": strings((doctor.checks ?? []).map(\.rawId)),
                "counts": .object((doctor.counts ?? [:]).mapValues { .number(Double($0)) }),
            ])
        },
        "config-read.schema.json": { data, _ in
            let read = try AgentReplyDecoder.decode(RemoteControllerDocument.self, from: data)
            // What is adopted is the agent's own text, and its hash is the one the reply states.
            if let raw = read.raw {
                #expect(read.documentBytes == Data(raw.utf8))
                #expect(try read.documentBytes.map(Canonical.hash) == read.hash)
            }
            return Reading(ok: read.ok, reasonCode: read.reasonCode, projection: ["hash": VectorJSON(read.hash)])
        },
        "legacy-2x-config.schema.json": { data, _ in
            let read = try AgentReplyDecoder.decode(RemoteControllerDocument.self, from: data)
            return Reading(ok: read.ok, reasonCode: read.reasonCode)
        },
        "config-meta.schema.json": { data, _ in
            let meta = try AgentReplyDecoder.decode(AgentConfigResult.self, from: data)
            return Reading(ok: meta.ok, action: meta.action, reasonCode: meta.reasonCode,
                           projection: ["hash": VectorJSON(meta.meta?.hash), "lineage": strings(meta.meta?.lineage ?? [])])
        },
        "config-set.schema.json": { data, _ in
            let set = try AgentReplyDecoder.decode(AgentConfigResult.self, from: data)
            return Reading(ok: set.ok, action: set.action, reasonCode: set.reasonCode,
                           outcome: Outcome.of(set),
                           projection: ["hash": VectorJSON(set.hash), "current": VectorJSON(set.current?.hash),
                                        "launchChanges": strings(set.launchChanges ?? []),
                                        "problems": strings((set.problems ?? []).map(\.path))])
        },
        "policy.schema.json": { data, entry in
            // The plain switch is read as the mutation it is; a policy read or patch as a policy.
            if entry.command == "auto-update" { return try mutation(data, entry) }
            let policy = try AgentReplyDecoder.decode(AgentPolicyResult.self, from: data)
            return Reading(ok: policy.ok, reasonCode: policy.reasonCode, outcome: Outcome.of(policy))
        },
        "service-config.schema.json": { data, entry in
            let reply = try AgentReplyDecoder.decode(ServiceConfigReply.self, from: data)
            if entry.command == "service-config get", reply.ok == true {
                #expect(reply.document != nil, "\(entry.file)")
                #expect(reply.hash != nil, "\(entry.file)")
            }
            return Reading(ok: reply.ok, reasonCode: reply.reasonCode,
                           outcome: reduced(outcome: reply.outcome, ok: reply.ok, reasonCode: reply.reasonCode))
        },
    ]

    /// The decoder of the command an error-shaped refusal answers.
    static let commandSchemas: [String: String] = [
        "status": "status.schema.json", "update": "update.schema.json", "boot": "boot.schema.json",
        "run": "run.schema.json", "cancel": "cancel.schema.json", "logs": "logs.schema.json",
    ]

    /// The same projection as each handler's, read straight off the JSON.
    static func rawProjection(_ schema: String, _ raw: VectorJSON) -> VectorJSON {
        func ids(_ list: VectorJSON?, _ key: String = "id") -> VectorJSON { .array((list?.array ?? []).map { $0[key] ?? nil }) }
        switch schema {
        case "status.schema.json":
            let services = raw["services"]?.array ?? []
            return [
                "services": ids(raw["services"]),
                "controllerHash": raw["controller"]?["hash"] ?? nil,
                "running": ids(raw["operations"]?["running"]),
                "processUnknown": .array(services.map { .bool(($0["process"]?["error"]).map { !$0.isNull } ?? false) }),
            ]
        case "busy.schema.json":
            if let services = raw["services"]?.array {
                return .array(services.map { ["id": $0["id"] ?? nil, "monitored": $0["monitored"] ?? nil, "evidence": $0["evidence"] ?? nil] })
            }
            return [["id": nil, "monitored": raw["monitored"] ?? nil, "evidence": raw["evidence"] ?? nil]]
        case "cycle.schema.json":
            return .array((raw["children"]?.array ?? []).map { ["service": $0["service"] ?? nil, "opId": $0["opId"] ?? nil] })
        case "op.schema.json":
            guard let op = raw["op"], !op.isNull else { return ["noRecord": true] }
            return ["id": op["id"] ?? nil, "state": op["state"] ?? nil]
        case "history.schema.json":
            return ids(raw["operations"])
        case "logs.schema.json":
            return ["lines": ids(raw["lines"], "line")]
        case "doctor.schema.json":
            return ["checks": ids(raw["checks"]), "counts": raw["counts"] ?? [:]]
        case "config-read.schema.json":
            return ["hash": raw["hash"] ?? nil]
        case "config-meta.schema.json":
            return ["hash": raw["meta"]?["hash"] ?? nil, "lineage": raw["meta"]?["lineage"] ?? []]
        case "config-set.schema.json":
            return ["hash": raw["hash"] ?? nil, "current": raw["current"]?["hash"] ?? nil,
                    "launchChanges": raw["launchChanges"] ?? [], "problems": ids(raw["problems"], "path")]
        default:
            return .null
        }
    }

    /// Every `reasonCode` anywhere in a reply: the envelope's, a record's, a result's, a child's.
    static func reasonCodes(in value: VectorJSON) -> [String] {
        switch value {
        case .object(let members):
            return members.flatMap { key, member in
                key == "reasonCode" && member.string != nil ? [member.string!] : reasonCodes(in: member)
            }
        case .array(let items):
            return items.flatMap(reasonCodes)
        default:
            return []
        }
    }

    @MainActor
    @Test("every entry of the fixture index is handled, in order, by the decoder this app reads it with")
    func everyIndexEntryIsHandled() throws {
        let index = try Self.index()
        #expect(index.contract == 3)
        #expect(index.fixtures.count >= 110)

        var handled = 0
        for entry in index.fixtures {
            let data = try Self.bytes(entry.file)
            let label = entry.file
            switch (entry.kind, entry.schema) {
            case ("document", "controller-document.schema.json"):
                // Section 9.7 under peer intake, which is how a document read from a machine is
                // taken. Android once refused the minimal document while its own test decoded the
                // fixtures as something else and passed (C3).
                let verdict = DocumentAcceptance.check(data, intake: .peer)
                #expect(verdict.valid == (entry.expect == "valid"), "\(label): \(verdict.errors.map(\.message))")
                if entry.expect == "valid" {
                    #expect((try? JSONDecoder().decode(ControllerConfig.self, from: data)) != nil, "\(label) did not decode")
                }
            case ("document", "bindings.schema.json"):
                let problems = Bindings.problems(in: data)
                #expect(problems.isEmpty == (entry.expect == "valid"), "\(label): \(problems)")
                if entry.expect == "valid" {
                    // And a valid file loads through the store this app keeps it in.
                    let directory = FileManager.default.temporaryDirectory.appending(path: "legion-bindings-\(UUID().uuidString)")
                    defer { try? FileManager.default.removeItem(at: directory) }
                    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
                    let url = directory.appending(path: "bindings.json")
                    try data.write(to: url)
                    let store = BindingsStore(url: url)
                    #expect(store.problem == nil, "\(label): \(store.problem ?? "")")
                    #expect(store.isPresent)
                }
            case ("reply", let schema):
                let handler = schema == "error.schema.json"
                    ? entry.command.flatMap { Self.commandSchemas[$0] }.flatMap { Self.handlers[$0] }
                    : Self.handlers[schema]
                guard let handler else {
                    Issue.record("no handler for kind=\(entry.kind) schema=\(schema) (\(label))")
                    continue
                }
                let reading = try handler(data, entry)
                #expect(reading.ok == entry.ok, "\(label): ok")
                #expect(reading.action == entry.action, "\(label): action")
                #expect(reading.reasonCode == entry.reasonCode, "\(label): reasonCode")
                if let outcome = entry.outcome {
                    #expect(reading.outcome?.rawValue == outcome, "\(label): outcome")
                }
                if schema == "status.schema.json" || schema == "legacy-2x-status.schema.json" {
                    #expect(reading.speaksContract3 == (entry.contract == 3), "\(label): contract gate")
                }

                // Every code in the file is one this build renders by code rather than falling
                // back to the message.
                let raw = try VectorJSON.parse(data)
                for code in Self.reasonCodes(in: raw) {
                    if case .unrecognised? = AgentReason(code: code) {
                        Issue.record("\(label): the reason code \(code) is not one this build knows")
                    }
                }

                // What this app's own reading of the attempt makes of the file as stdout, with the
                // exit code rule 7 gives (section 15.4): a reply, and the same one.
                let text = String(decoding: data, as: UTF8.self)
                let verdict = AttemptClassifier.classify(AttemptEvidence(
                    launchFailed: false, timedOut: false, exitCode: entry.exitCode.map(Int32.init),
                    stdout: text, stderr: "", sshLog: ""))
                #expect(verdict.isReply, "\(label): exit \(entry.exitCode.map(String.init) ?? "none") was not read as a reply")
                if verdict.isReply, let json = AttemptClassifier.jsonObjectSlice(text) {
                    let again = try handler(json, entry)
                    #expect(again.ok == reading.ok, "\(label): the classified reply reads differently")
                    #expect(again.action == reading.action, "\(label): the classified reply reads differently")
                    #expect(again.reasonCode == reading.reasonCode, "\(label): the classified reply reads differently")
                }

                // The schema's own fields, read off the model and off the JSON, agree.
                if schema != "error.schema.json" {
                    let differences = Self.rawProjection(schema, raw).differences(from: reading.projection)
                    #expect(differences.isEmpty, "\(label): \(differences.joined(separator: "; "))")
                }
            default:
                Issue.record("no handler for kind=\(entry.kind) schema=\(entry.schema) (\(label))")
                continue
            }
            handled += 1
        }
        #expect(handled == index.fixtures.count)
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
        // The fixtures are written from agent/package.json, the version this app bundles.
        #expect(status.version == bundledAgentVersion)
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

    @Test("a fetched document is the agent's own text, and an agent without it is rebuilt in the order it sent")
    func fetchedDocumentIsVerified() throws {
        // Agent 3.1 sends the stored text, and that is what is hashed and adopted.
        let current = try AgentReplyDecoder.decode(RemoteControllerDocument.self, from: try Self.bytes("config.read.json"))
        let raw = try #require(current.documentBytes)
        #expect(raw == Data(try #require(current.raw).utf8))
        #expect(try Canonical.hash(raw) == current.hash)

        // Agent 3.0 sends only the parsed object. Written out again in the order its members arrived,
        // it reproduces the stored bytes; sorted, as this app once wrote it, it did not (C1).
        let older = try AgentReplyDecoder.decode(RemoteControllerDocument.self, from: try Self.bytes("config.read-without-raw.json"))
        #expect(older.raw == nil)
        let rebuilt = try #require(older.documentBytes)
        #expect(try Canonical.hash(rebuilt) == older.hash)
        #expect(rebuilt == raw)

        // Without the reply's own text there is no order to rebuild in, and nothing to adopt.
        let decodedOnly = try JSONDecoder().decode(RemoteControllerDocument.self, from: try Self.bytes("config.read-without-raw.json"))
        #expect(decodedOnly.documentBytes == nil)

        // A machine holding nothing sends no document and no hash.
        let empty = try AgentReplyDecoder.decode(RemoteControllerDocument.self, from: try Self.bytes("config.read-empty.json"))
        #expect(empty.documentBytes == nil)
        #expect(empty.hash == nil)
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
