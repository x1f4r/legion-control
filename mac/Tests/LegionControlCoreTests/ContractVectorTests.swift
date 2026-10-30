import Foundation
import Testing
@testable import LegionControlCore

/// The shared conformance vectors, run through this app's own code.
///
/// Each group the index lists the Mac as a consumer of goes through exactly one adapter, which turns
/// a case's input into what this app does with it, and every case has to agree with the reference's
/// `expect`. The number of cases run has to be the number the index gives the Mac, so a group that
/// shrank, grew or was skipped fails here rather than passing quietly.
struct ContractVectorTests {
    static let consumer = "mac"

    /// The families this suite runs: all nine the contract has.
    static let families = ["hash", "identifiers", "outcome", "transport", "setup-acceptance", "launch", "setup-reconcile",
                           "setup-merge", "wake"]

    typealias Adapter = @Sendable (VectorJSON) throws -> VectorJSON

    struct BadInput: Error, CustomStringConvertible {
        var description: String
    }

    static func string(_ input: VectorJSON, _ key: String) throws -> String {
        guard let value = input[key]?.string else { throw BadInput(description: "\(key) is not a string") }
        return value
    }

    static func int(_ input: VectorJSON, _ key: String) throws -> Int {
        guard let value = input[key]?.int else { throw BadInput(description: "\(key) is not an integer") }
        return value
    }

    static func valid(_ check: @escaping @Sendable (String) -> Bool) -> Adapter {
        { input in ["valid": .bool(check(try string(input, "value")))] }
    }

    /// Every failure kind by its contract name.
    static let kinds: [String: AgentFailure.Kind] = Dictionary(uniqueKeysWithValues: [
        AgentFailure.Kind.noRoute, .launchFailed, .hostUnreachable, .connectionRefused, .authenticationFailed,
        .hostKeyChanged, .hostKeyUnknown, .timedOut(seconds: 0), .linkLost, .interpreterMissing, .agentMissing,
        .unreadableOutput, .agentFailed, .badRequest
    ].map { ($0.contractName, $0) })

    static func strings(_ input: VectorJSON?, _ key: String) throws -> [String] {
        guard let items = input?[key]?.array else { throw BadInput(description: "\(key) is not an array") }
        return try items.map { item in
            guard let value = item.string else { throw BadInput(description: "\(key) holds a non-string") }
            return value
        }
    }

    /// A time the vectors write as integer milliseconds.
    static func date(_ value: VectorJSON?) -> Date? {
        value?.number.map { Date(timeIntervalSince1970: $0 / 1000) }
    }

    /// The codes of a list of problems as the vectors list them: sorted, each once.
    static func codes(_ problems: [DocumentAcceptance.Problem]) -> VectorJSON {
        .array(Set(problems.map(\.code)).sorted().map(VectorJSON.string))
    }

    /// A case's input as the bytes a reply would have arrived as, for the decoders the app uses.
    static func decode<Value: Decodable>(_ type: Value.Type, _ input: VectorJSON) throws -> Value {
        try JSONDecoder().decode(type, from: JSONSerialization.data(withJSONObject: input.foundation, options: [.fragmentsAllowed]))
    }

    static func document(_ text: String) throws -> Any {
        try JSONSerialization.jsonObject(with: Data(text.utf8))
    }

    /// Section 9.9 as the `launch/review` group reads it: the record is the case's `approvals`,
    /// otherwise the one seeded from the held document, otherwise one that approves nothing.
    static func launchReview(_ input: VectorJSON) throws -> VectorJSON {
        let heldText = input["heldText"]?.string
        let held = try heldText.map(document)
        let record: LaunchRecord
        if let approvals = input["approvals"], !approvals.isNull {
            guard let object = approvals.foundation as? [String: Any] else { throw BadInput(description: "approvals is not an object") }
            record = LaunchRecord(object: object)
        } else if let heldText, let held {
            record = LaunchValues.approvals(from: held, seededFrom: try Canonical.hash(Data(heldText.utf8)))
        } else {
            record = .nothingApproved
        }
        let projection = LaunchValues.projection(of: try document(try string(input, "offeredText")))
        // What a 3.1 agent answers a restricted push of this over the held copy: refused exactly when
        // a value would wait against the held document's own values, whatever this device approved.
        let refused = held.map { !LaunchValues.review(record: LaunchValues.approvals(from: $0, seededFrom: "none"),
                                                      projection: projection).isEmpty } ?? false
        return [
            "review": .array(LaunchValues.review(record: record, projection: projection).map { entry in
                ["path": .string(entry.path), "scope": .string(entry.scope.rawValue),
                 "kind": .string(entry.kind.rawValue), "approvable": .bool(entry.approvable)]
            }),
            "effective": .array(LaunchValues.effective(record: record, projection: projection).map { entry in
                ["path": .string(entry.location.path), "scope": .string(entry.location.scope.rawValue),
                 "state": .string(entry.state.rawValue),
                 "agent": entry.tuple.map { .array($0.agent.map(VectorJSON.string)) } ?? .null,
                 "grammar": VectorJSON(entry.tuple?.grammar), "text": VectorJSON(entry.text)]
            }),
            "restrictedRefused": .bool(refused),
        ]
    }

    /// Section 10.2 as the `wake/plans` group reads it: the shared document, this device's
    /// addresses and chosen site, each helper's last status read, and the holds for the target.
    static func wakePlan(_ input: VectorJSON) throws -> VectorJSON {
        let config = try JSONDecoder().decode(ControllerConfig.self, from: Data(try string(input, "documentText").utf8))
        let target = try #require(config.machine(id: try string(input, "target")))
        let here = try #require(input["here"])
        let placement = SiteAwareness.placement(sites: config.allSites,
                                                addresses: try strings(here, "addresses").compactMap(LocalAddress.init),
                                                confirmed: here["currentSite"]?.string)
        var helpers: [String: WakePlan.HelperState] = [:]
        for (id, read) in input["helpers"]?.object ?? [:] {
            guard let state = WakePlan.HelperState.State(rawValue: try string(read, "state")) else {
                throw BadInput(description: "unknown helper state")
            }
            helpers[id] = WakePlan.HelperState(state, actionKinds: read["actions"]?.object?.compactMapValues(\.string))
        }
        let holds = try (input["holds"]?.array ?? []).map { WakePlan.Hold(helper: try string($0, "helper"), action: try string($0, "action")) }
        let plan = WakePlan.plan(for: target, in: config, placement: placement, helpers: helpers, holds: holds)
        let steps: [VectorJSON] = plan.steps.map { step in
            switch step {
            case .direct(let broadcasts, let ports):
                return ["kind": "direct", "machine": nil, "action": nil, "readFirst": nil,
                        "broadcast": .array(broadcasts.map(VectorJSON.string)),
                        "ports": .array(ports.map { .number(Double($0)) }), "repeats": VectorJSON(WakeOnLAN.repeats)]
            case .helper(let helper, _, let readFirst):
                return ["kind": "helper", "machine": .string(helper.machine), "action": .string(helper.action),
                        "readFirst": .bool(readFirst), "broadcast": nil, "ports": nil, "repeats": nil]
            }
        }
        return [
            "matching": .array(plan.placement.matching.map(VectorJSON.string)),
            "effective": VectorJSON(plan.placement.effective),
            "steps": .array(steps),
            "skipped": .array(plan.skipped.map { skip in
                ["kind": .string(skip.kind.rawValue), "machine": VectorJSON(skip.machine), "action": VectorJSON(skip.action),
                 "reason": .string(skip.reason.rawValue)]
            }),
            "offerWakeFirst": .array(plan.offerWakeFirst.map(VectorJSON.string)),
        ]
    }

    static func verdict(_ verdict: AttemptClassifier.Verdict) -> VectorJSON {
        [
            "outcome": verdict.isReply ? "reply" : "failure",
            "kind": VectorJSON(verdict.kind?.contractName),
            "dispatch": .string(verdict.dispatch.rawValue),
            "routeLevel": .bool(verdict.routeLevel),
        ]
    }

    static let adapters: [String: Adapter] = [
        // Section 1, rule 5: the identifier grammars, whole input, ASCII only.
        "identifiers/token": valid(AgentToken.isValid),
        "identifiers/configuredId": valid(AgentToken.isValidID),
        "identifiers/setupId": valid(AgentToken.isValidSetupID),
        "identifiers/opId": valid(AgentToken.isValidOperationId),
        "identifiers/duration": valid(AgentToken.isValidDuration),
        "identifiers/clockTime": valid(AgentToken.isValidClockTime),
        "identifiers/sha256": valid(AgentToken.isValidHash),

        // Section 5.6: the reducer itself, and a record or a summary read through the decoder the
        // operation lists and the reconciliation use.
        "outcome/reduce": { input in
            ["outcome": .string(Outcome.reduce(try decode(Outcome.Evidence.self, input)).rawValue)]
        },
        "outcome/records": { input in
            let value = try #require(input["value"])
            return ["outcome": .string(Outcome.of(try decode(AgentOperation.self, value)).rawValue)]
        },

        // Section 15.1.
        "transport/grammars": { input in
            let resolved = CommandGrammar.resolve(
                platform: input["platform"]?.string.flatMap(Platform.init(rawValue:)),
                shell: input["shell"]?.string,
                restricted: input["restricted"]?.bool,
                restrictedSession: input["restrictedSession"]?.bool ?? false,
                agentLength: try int(input, "agentLength")
            )
            switch resolved {
            case .success(let grammar): return ["grammar": .string(grammar.rawValue), "refusal": nil]
            case .failure(let refusal): return ["grammar": nil, "refusal": .string(refusal.code)]
            }
        },
        // Section 15.2.
        "transport/commandLines": { input in
            guard let grammar = CommandGrammar(rawValue: try string(input, "grammar")) else {
                throw BadInput(description: "unknown grammar")
            }
            let argv = try (input["argv"]?.array ?? []).map { item in
                guard let value = item.string else { throw BadInput(description: "argv holds a non-string") }
                return value
            }
            do throws(CommandLineRefusal) {
                return ["line": .string(try grammar.serialize(argv)), "refusal": nil, "index": nil]
            } catch {
                return ["line": nil, "refusal": .string(error.code), "index": VectorJSON(error.index)]
            }
        },
        // Section 15.4.
        "transport/classifications": { input in
            let exitCode = try input["exitCode"].map { value -> Int32? in
                if value.isNull { return nil }
                guard let code = value.int else { throw BadInput(description: "exitCode is not an integer") }
                return Int32(code)
            } ?? nil
            let evidence = AttemptEvidence(launchFailed: input["launchFailed"]?.bool ?? false,
                                           timedOut: input["timedOut"]?.bool ?? false,
                                           exitCode: exitCode,
                                           stdout: try string(input, "stdout"),
                                           stderr: try string(input, "stderr"),
                                           sshLog: try string(input, "sshLog"))
            return verdict(AttemptClassifier.classify(evidence))
        },
        // Section 15.5: the walk, run through the same step RemoteAgent takes.
        "transport/walks": { input in
            let safety: CommandSafety = try string(input, "safety") == "mutation" ? .mutation : .readOnly
            let results = try (input["attempts"]?.array ?? []).map { attempt -> (route: String, verdict: AttemptClassifier.Verdict) in
                guard let result = attempt["result"], let dispatch = Dispatch(rawValue: try string(result, "dispatch")) else {
                    throw BadInput(description: "an attempt has no result")
                }
                let kind = try result["kind"]?.string.map { name in
                    guard let kind = kinds[name] else { throw BadInput(description: "unknown kind \(name)") }
                    return kind
                }
                let reply = try string(result, "outcome") == "reply"
                return (try string(attempt, "route"),
                        AttemptClassifier.Verdict(kind: reply ? nil : kind, dispatch: dispatch,
                                                  routeLevel: result["routeLevel"]?.bool ?? false))
            }
            let walk = AttemptWalk.simulate(safety: safety, results: results)
            return ["sent": .array(walk.sent.map { .number(Double($0)) }), "ending": .string(walk.ending.rawValue)]
        },
        "transport/statusBudgets": { input in
            let budget = StatusBudget.next(remainingMs: try int(input, "remainingMs"), attemptsMade: try int(input, "attemptsMade"))
            return ["attempt": .bool(budget != nil), "budgetMs": VectorJSON(budget)]
        },
        // Section 15.6.
        "transport/hosts": { input in
            let form = Addresses.hostForm(try string(input, "value"))
            return ["valid": .bool(form != nil), "form": VectorJSON(form?.rawValue)]
        },
        "transport/users": valid(Addresses.isUser),
        "transport/ports": { input in
            // The document's decoder refuses a port that is not a JSON integer before it is ever a
            // number here, so a string or a fraction is not a port.
            ["valid": .bool(input["value"]?.int.map(Addresses.isPort) ?? false)]
        },

        // Section 12: canonical bytes, their hash, and what the agent refuses to store.
        "hash/vectors": { input in
            guard let bytes = Data(base64Encoded: try string(input, "inputBase64")) else { throw BadInput(description: "bad base64") }
            let canonical = try? Canonical.bytes(bytes)
            let rejection = Canonical.rejection(bytes)
            return [
                "canonicalBase64": VectorJSON(canonical?.base64EncodedString()),
                "sha256": VectorJSON(canonical.map(Canonical.sha256)),
                "accepted": .bool(rejection == nil),
                "reject": VectorJSON(rejection?.rawValue),
            ]
        },

        // Section 9.7.
        "setup-acceptance/documents": { input in
            guard let intake = DocumentAcceptance.Intake(rawValue: try string(input, "intake")) else {
                throw BadInput(description: "unknown intake")
            }
            let verdict = DocumentAcceptance.check(Data(try string(input, "text").utf8), intake: intake)
            return ["valid": .bool(verdict.valid), "errors": codes(verdict.errors), "warnings": codes(verdict.warnings),
                    "assignsIdentity": .bool(verdict.assignsIdentity), "keep": .bool(verdict.keep)]
        },

        // Section 9.5.
        "setup-reconcile/decide": { input in
            let local = try input["local"].flatMap { local -> SetupSync.Local? in
                if local.isNull { return nil }
                return SetupSync.Local(hash: try string(local, "hash"), id: local["id"]?.string, lineage: try strings(local, "lineage"))
            }
            let status = input["status"].flatMap { $0.isNull ? nil : $0 }
            let remote = status?["controller"].flatMap { controller -> ControllerCopy? in
                controller.isNull ? nil : ControllerCopy(hash: controller["hash"]?.string, id: controller["id"]?.string)
            }
            var metaByHash: [String: SetupSync.Meta] = [:]
            for (hash, meta) in input["metaByHash"]?.object ?? [:] {
                metaByHash[hash] = SetupSync.Meta(id: meta["id"]?.string, lineage: try strings(meta, "lineage"))
            }
            let verdict = SetupSync.decide(local: local, contract: status?["contract"]?.int, remote: remote, metaByHash: metaByHash)
            return ["decision": .string(verdict.decision.rawValue), "reason": VectorJSON(verdict.reason), "expect": VectorJSON(verdict.expect)]
        },
        "setup-reconcile/admit": { input in
            let expected = try #require(input["expected"])
            let fetched = try #require(input["fetched"])
            let admission = SetupSync.admit(
                expected: SetupSync.Expected(localHash: try string(expected, "localHash"), localId: try string(expected, "localId"),
                                             remoteHash: try string(expected, "remoteHash")),
                currentLocalHash: input["currentLocalHash"]?.string,
                fetched: SetupSync.Fetched(hash: fetched["hash"]?.string, replyHash: try string(fetched, "replyHash"),
                                           id: fetched["id"]?.string, lineage: try strings(fetched, "lineage")),
                accepted: input["accepted"]?.bool ?? false
            )
            return ["admission": .string(admission.rawValue)]
        },
        "setup-reconcile/gate": { input in
            guard let decision = SetupSync.Decision(rawValue: try string(input, "decision")), let state = input["state"] else {
                throw BadInput(description: "unknown decision or no state")
            }
            var peer = SetupSync.PeerState()
            peer.inFlight = state["inFlight"]?.bool ?? false
            peer.failedAt = (state["failedAt"]?.object ?? [:]).compactMapValues(date)
            peer.held = (state["held"]?.object ?? [:]).compactMapValues(\.string)
            peer.localInvalid = state["localInvalid"]?.bool ?? false
            peer.localMutation = state["localMutation"]?.bool ?? false
            peer.runningOperations = state["runningOperations"]?.int ?? 0
            peer.lockedAt = date(state["lockedAt"])
            peer.statusAt = date(state["statusAt"])
            peer.settled = Set(try strings(state, "settled"))
            let gate = SetupSync.gate(decision: decision, localHash: try string(input, "localHash"),
                                      remoteHash: input["remoteHash"]?.string, agentVersion: input["agentVersion"]?.string,
                                      now: try #require(date(input["now"])), state: peer)
            return ["action": .string(gate.action.rawValue), "reason": VectorJSON(gate.reason)]
        },
        "setup-reconcile/pushReply": { input in
            // The reply as the transport hands it over: decoded, or nothing when it would not decode.
            let reply = try input["reply"].flatMap { reply -> AgentConfigResult? in
                if reply.isNull { return nil }
                let data = try JSONSerialization.data(withJSONObject: reply.foundation)
                return try? JSONDecoder().decode(AgentConfigResult.self, from: data)
            }
            let effect = SetupSync.afterPush(reply, localHash: try string(input, "localHash"))
            let metaHash: String? = if case .fileMeta(let hash, _) = effect { hash } else { nil }
            return ["effect": .string(effect.name), "metaHash": VectorJSON(metaHash)]
        },

        // Section 9.8.
        "setup-merge/merges": { input in
            let writer = try #require(input["writer"])
            var choices: [String: SetupMerge.Choice] = [:]
            for (entry, side) in input["choices"]?.object ?? [:] {
                guard let choice = side.string.flatMap(SetupMerge.Choice.init(rawValue:)) else { throw BadInput(description: "bad choice") }
                choices[entry] = choice
            }
            let outcome = try SetupMerge.merge(
                base: input["baseText"]?.string.map { Data($0.utf8) },
                mine: Data(try string(input, "mineText").utf8),
                theirs: Data(try string(input, "theirsText").utf8),
                mineHash: try string(input, "mineHash"),
                theirsHash: try string(input, "theirsHash"),
                writer: SetupMerge.Writer(source: try string(writer, "source"), device: try string(writer, "device"),
                                          now: try string(writer, "now")),
                choices: choices
            )
            return [
                "differences": .array(outcome.differences.map { ["entry": .string($0.entry), "kind": .string($0.kind.rawValue)] }),
                "resultText": VectorJSON(outcome.result.map { String(decoding: $0, as: UTF8.self) }),
                "error": VectorJSON(outcome.error),
            ]
        },
        "setup-merge/bases": { input in
            let kept = (input["kept"]?.object ?? [:]).compactMapValues { $0.string.map { Data($0.utf8) } }
            let base = SetupMerge.commonBase(mineHash: try string(input, "mineHash"), theirsHash: try string(input, "theirsHash"),
                                             mineLineage: try strings(input, "mineLineage"),
                                             theirsLineage: try strings(input, "theirsLineage")) { kept[$0] }
            return ["base": VectorJSON(base)]
        },
        "setup-merge/numbers": { input in
            let a = SetupJSON.decimalKey(try string(input, "a"))
            let b = SetupJSON.decimalKey(try string(input, "b"))
            guard a != nil, b != nil else { throw BadInput(description: "not a number lexeme") }
            return ["equal": .bool(a == b)]
        },

        // Sections 10.1 and 10.2.
        "wake/prefixes": { input in
            let prefix = LanPrefix(try string(input, "prefix"))
            return ["valid": .bool(prefix != nil),
                    "contains": .array(try strings(input, "addresses").map { text in
                        .bool(LanPrefix.ipv4(text).map { prefix?.contains($0) ?? false } ?? false)
                    })]
        },
        "wake/broadcasts": { input in
            ["valid": .bool(LanPrefix.ipv4(try string(input, "text")) != nil)]
        },
        "wake/plans": wakePlan,
        "wake/readiness": { input in
            ["pollStartsMs": .array(WakePlan.readinessSchedule(sentAnything: input["sentAnything"]?.bool ?? false)
                .map { .number(Double($0)) })]
        },

        // Section 9.9.
        "launch/review": launchReview,
        "launch/migration": { input in
            ["migration": .bool(LaunchValues.isLauncherMigration(approved: LaunchTuple(json: input["approved"]?.foundation),
                                                                 offered: LaunchTuple(json: input["offered"]?.foundation)))]
        },
    ]

    @Test("every case of a family the Mac consumes agrees with the reference", arguments: families)
    func family(_ name: String) throws {
        let index = try ContractCorpus.vectorIndex()
        #expect(index.contract == 3)
        let entry = try #require(index.families.first { $0.family == name }, "the index has no family \(name)")
        let file = try ContractCorpus.family(name)

        for group in entry.groups {
            guard let expected = group.consumers[Self.consumer] else { continue }
            let key = "\(name)/\(group.group)"
            let adapter = try #require(Self.adapters[key], "no adapter for \(key)")
            // The hash file keeps the flat layout it always had: every vector is for every
            // consumer, is its own input, and carries the four fields to reproduce beside it.
            let flat = entry.layout == "flat"
            let cases = try #require(flat ? file["vectors"]?.array : file["groups"]?[group.group]?.array,
                                     "\(key) has no cases in its file")
            var ran = 0
            for vector in cases where flat || vector["consumers"]?.array?.contains(.string(Self.consumer)) == true {
                let label = "\(key)/\(vector["name"]?.string ?? "?")"
                let input = try #require(flat ? vector : vector["input"], "\(label) has no input")
                let expect = try #require(flat ? Self.flatExpect(vector) : vector["expect"], "\(label) has no expect")
                do {
                    let differences = expect.differences(from: try adapter(input))
                    #expect(differences.isEmpty, "\(label): \(differences.joined(separator: "; "))")
                } catch {
                    Issue.record("\(label): \(error)")
                }
                ran += 1
            }
            #expect(ran == expected, "\(key): ran \(ran) cases, the index gives the Mac \(expected)")
        }
    }

    /// What a flat vector expects: the four fields the reference reproduces.
    static func flatExpect(_ vector: VectorJSON) -> VectorJSON? {
        let keys = ["canonicalBase64", "sha256", "accepted", "reject"]
        guard keys.allSatisfy({ vector[$0] != nil }) else { return nil }
        return .object(Dictionary(uniqueKeysWithValues: keys.map { ($0, vector[$0]!) }))
    }

    @Test("every group the Mac consumes in these families has exactly one adapter")
    func everyConsumedGroupHasOneAdapter() throws {
        let index = try ContractCorpus.vectorIndex()
        let consumed = Set(index.families.filter { Self.families.contains($0.family) }.flatMap { family in
            family.groups.filter { $0.consumers[Self.consumer] != nil }.map { "\(family.family)/\($0.group)" }
        })
        #expect(Set(Self.families).isSubset(of: index.families.map(\.family)))
        #expect(Set(Self.adapters.keys) == consumed)
    }

    @Test("the comparison is the index's: key order free, strings by code point, Text fields parsed")
    func comparison() {
        let expected: VectorJSON = ["a": 1, "b": ["x", nil, true], "docText": #"{"p":1,"q":[2]}"#]
        #expect(expected.differences(from: ["b": ["x", nil, true], "a": 1, "docText": #"{"q":[2],"p":1}"#]).isEmpty)
        // In a document, a number's spelling is part of what is compared: `22.0` is not `22` there.
        #expect(!VectorJSON.object(["docText": #"{"p":22.0}"#]).differences(from: ["docText": #"{"p":22}"#]).isEmpty)
        // A composed and a decomposed é are one string to Swift and two to the contract.
        #expect(!VectorJSON.string("\u{E9}").differences(from: .string("e\u{301}")).isEmpty)
        #expect(!expected.differences(from: ["a": 1, "b": ["x", nil, false], "docText": #"{"p":1,"q":[2]}"#]).isEmpty)
        #expect(!expected.differences(from: ["a": 1, "b": ["x", nil, true]]).isEmpty)
        #expect(!VectorJSON.bool(true).differences(from: .number(1)).isEmpty)
    }
}
