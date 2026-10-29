import Foundation
import Testing
@testable import LegionControlCore

/// Reading, validating and editing the shared setup.
@MainActor
struct ConfigTests {

    static func temporaryHome() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-config-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    static func store(_ json: String) throws -> (ConfigStore, URL) {
        let home = try temporaryHome()
        let file = home.appending(path: "config.json")
        try Data(json.utf8).write(to: file)
        return (ConfigStore(url: file), home)
    }

    static let twoMachines = """
    {
      "version": 1,
      "sites": [ { "id": "home", "name": "Home", "lanPrefixes": ["10.0.0."], "broadcast": ["10.0.0.255"] } ],
      "machines": [
        { "id": "pi", "name": "Pi", "site": "home", "alwaysOn": true,
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5" } ],
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "tower", "name": "Tower", "site": "home",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.9" } ],
          "wake": { "mac": "AA:BB:CC:DD:EE:FF", "broadcast": ["10.0.0.255"],
                    "helper": { "machine": "pi", "action": "wake-tower" },
                    "helpers": [ { "machine": "pi", "action": "wake-tower" } ] },
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] }
      ]
    }
    """

    // MARK: - Decoding

    @Test("sites, helpers and the always-on hint are read")
    func decodesTopology() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }
        let config = try #require(store.config)

        #expect(config.allSites.count == 1)
        #expect(config.site(id: "home")?.lanPrefixes == ["10.0.0."])
        let tower = try #require(config.machine(id: "tower"))
        #expect(tower.site == "home")
        #expect(tower.wake?.orderedHelpers.map(\.machine) == ["pi"])
        #expect(config.machine(id: "pi")?.alwaysOn == true)
    }

    @Test("the singular helper is read when there is no list, for a document a 1.2 client wrote")
    func singularHelperAlias() throws {
        let json = Self.twoMachines.replacingOccurrences(
            of: """
            ,
                        "helpers": [ { "machine": "pi", "action": "wake-tower" } ]
            """, with: "")
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }
        let tower = try #require(store.config?.machine(id: "tower"))
        #expect(tower.wake?.orderedHelpers.map(\.action) == ["wake-tower"])
    }

    // MARK: - Acceptance

    /// The error codes section 9.7 gives a document under peer intake, sorted.
    static func errorCodes(_ json: String) -> [String] {
        Set(DocumentAcceptance.check(Data(json.utf8), intake: .peer).errors.map(\.code)).sorted()
    }

    @Test("a helper that does not exist, is the machine itself, or forms a loop is refused")
    func helperValidation() throws {
        #expect(Self.errorCodes(Self.twoMachines).isEmpty)

        let missing = Self.twoMachines.replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"ghost\"")
        #expect(Self.errorCodes(missing) == ["helper-unknown"])

        let itself = Self.twoMachines.replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"tower\"")
        // A machine that is asleep cannot wake itself, so this is always a mistake.
        #expect(Self.errorCodes(itself) == ["helper-self"])

        let cycle = """
        { "version": 1, "machines": [
          { "id": "a", "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}],
            "endpoints": [{"id":"e","host":"h"}],
            "wake": { "mac": "AA:BB:CC:DD:EE:FF", "helpers": [{ "machine": "b", "action": "w" }] } },
          { "id": "b", "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}],
            "endpoints": [{"id":"e2","host":"h2"}],
            "wake": { "mac": "AA:BB:CC:DD:EE:00", "helpers": [{ "machine": "a", "action": "w" }] } }
        ] }
        """
        #expect(Self.errorCodes(cycle) == ["helper-cycle"])

        // The compatibility key has to name the same machine and action as the first of the list: a
        // document where they disagree wakes different things depending on which app is open.
        let mismatch = Self.twoMachines.replacingOccurrences(
            of: "\"helper\": { \"machine\": \"pi\", \"action\": \"wake-tower\" }",
            with: "\"helper\": { \"machine\": \"pi\", \"action\": \"wake-other\" }")
        #expect(Self.errorCodes(mismatch) == ["helper-alias"])

        // A helper action is a configured id, which has no colon, as the agent's own grammar says.
        let colon = Self.twoMachines.replacingOccurrences(of: "wake-tower", with: "wake:tower")
        #expect(Self.errorCodes(colon) == ["schema"])
    }

    @Test("a site is only checked against a list that exists")
    func siteValidation() throws {
        let nowhere = Self.twoMachines.replacingOccurrences(of: "\"site\": \"home\"", with: "\"site\": \"nowhere\"")
        #expect(Self.errorCodes(nowhere) == ["unknown-site"])
        // A setup without sites places nobody, so naming one is not wrong yet.
        let unplaced = nowhere.replacingOccurrences(
            of: "\"sites\": [ { \"id\": \"home\", \"name\": \"Home\", \"lanPrefixes\": [\"10.0.0.\"], \"broadcast\": [\"10.0.0.255\"] } ],", with: "")
        #expect(Self.errorCodes(unplaced).isEmpty)
    }

    @Test("a lineage of the wrong shape is refused rather than half-trusted")
    func lineageValidation() throws {
        let json = """
        { "version": 1, "controller": { "id": "s", "revision": 2, "lineage": ["short"] },
          "machines": [ { "id": "a", "endpoints": [{"id":"e","host":"h"}],
            "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        #expect(Self.errorCodes(json) == ["schema"])

        // Swift's own idea of a hex digit takes the fullwidth letters too; the contract's does not.
        let fullwidth = String(repeating: "a", count: 63) + "\u{FF41}"
        #expect(Self.errorCodes(json.replacingOccurrences(of: "short", with: fullwidth)) == ["schema"])
        #expect(Self.errorCodes(json.replacingOccurrences(of: "short", with: String(repeating: "a", count: 64))).isEmpty)
    }

    @Test("what this app cannot use is accepted with a warning, and only a schema or rule break refuses")
    func partialSetupsAreAccepted() throws {
        // A machine with no systems yet, an endpoint with no id and no user, a wake block with no
        // hardware address: every one of them is a document another peer may well have written.
        let partial = """
        { "version": 1, "machines": [
          { "id": "new" },
          { "id": "tower", "endpoints": [{"host":"192.0.2.10"}], "wake": { "helpers": [{ "machine": "new", "action": "w" }] },
            "systems": [{"id":"l","platform":"linux"}] } ] }
        """
        let verdict = DocumentAcceptance.check(Data(partial.utf8), intake: .peer)
        #expect(verdict.valid)
        #expect(Set(verdict.warnings.map(\.code)) == ["machine-no-systems", "machine-no-endpoints", "system-no-agent",
                                                     "endpoint-no-id", "endpoint-no-user", "wake-no-mac",
                                                     "helper-not-always-on"])
    }

    @Test("a document typed here gets an identity before it is checked; one read from a machine does not")
    func intakes() throws {
        let anonymous = """
        { "version": 1, "controller": { "name": "Home" }, "machines": [] }
        """
        let local = DocumentAcceptance.check(Data(anonymous.utf8), intake: .local)
        #expect(local.valid && local.assignsIdentity)
        let peer = DocumentAcceptance.check(Data(anonymous.utf8), intake: .peer)
        #expect(!peer.valid && !peer.keep)
        // The document this device already applies is never refused while section 12 accepts it.
        let applied = DocumentAcceptance.check(Data(anonymous.utf8), intake: .applied)
        #expect(!applied.valid && applied.keep)
        #expect(!DocumentAcceptance.check(Data("{ not json".utf8), intake: .applied).keep)
    }

    @Test("a broken applied setup is kept, without the machine it breaks, and is not published")
    func brokenAppliedSetupIsKept() throws {
        let broken = Self.twoMachines.replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"ghost\"")
        let (store, home) = try Self.store(broken)
        defer { try? FileManager.default.removeItem(at: home) }

        // The file is applied: it is what this device runs on, errors and all.
        #expect(store.problem == nil)
        #expect(store.errors.map(\.code) == ["helper-unknown"])
        // The machine the error is inside is not driven, and the other one still is.
        #expect(store.config?.machines.map(\.id) == ["pi"])
        // And it decides and merges as it is, but is never published.
        let document = try #require(store.document)
        #expect(!document.errors.isEmpty)
        let gate = SetupSync.gate(decision: .push, localHash: document.hash, remoteHash: nil, agentVersion: nil, now: Date(),
                                  state: SetupSync.PeerState(localInvalid: !document.errors.isEmpty))
        #expect(gate == SetupSync.Gate(.hold, reason: "local-invalid"))
    }

    @Test("a setup that already breaks a rule can still be edited, and fixed, but not broken further")
    func editingABrokenSetup() throws {
        let broken = Self.twoMachines.replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"ghost\"")
        let (store, home) = try Self.store(broken)
        defer { try? FileManager.default.removeItem(at: home) }
        #expect(store.errors.map(\.code) == ["helper-unknown"])

        // Renaming the very machine the error is in, or moving the other one up the list, is not a
        // new error.
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "tower", "name": "Big Tower"], in: &root)
        } == nil)
        #expect(store.applyEdit(describedAs: "reordered", deviceName: "MacBook") { root in
            root["machines"] = Array(((root["machines"] as? [[String: Any]]) ?? []).reversed())
        } == nil)
        // A second one is.
        #expect(store.applyEdit(describedAs: "broke more", deviceName: "MacBook") { root in
            try ControllerEditor.setSite("nowhere", onMachine: "pi", in: &root)
        } != nil)
        // And the edit that fixes it is saved, and the setup is shared again.
        #expect(store.applyEdit(describedAs: "fixed", deviceName: "MacBook") { root in
            try ControllerEditor.setWakeHelpers([WakeHelper(machine: "pi", action: "wake-tower")], onMachine: "tower", in: &root)
        } == nil)
        #expect(store.errors.isEmpty)
        #expect(store.document?.errors.isEmpty == true)
    }

    @Test("a system no grammar can be written for is a warning that says why")
    func refusedGrammarIsAWarning() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "tower", "endpoints": [{"id":"e","host":"192.0.2.10"}],
          "systems": [{"id":"l","platform":"linux","restricted":true,"agent":["/bin/sh","-c","legionctl"]}] } ] }
        """
        let config = try ControllerConfig.accepted(Data(json.utf8))
        #expect(config.commandWarnings().contains { $0.contains("is never sent anything") && $0.contains("the interpreter and the agent's entry") })
    }

    @Test("an address outside the dial grammar is a warning that names it")
    func undialableAddressIsAWarning() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "tower", "name": "Tower",
          "endpoints": [{"id":"lan","host":"desk-tower.local","user":"me"}, {"id":"odd","host":"tower.example.","user":"me"}],
          "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let warnings = DocumentAcceptance.check(Data(json.utf8), intake: .peer).warnings
        #expect(warnings.map(\.code) == ["endpoint-undialable"])
        #expect(warnings.first?.path == "machines[0].endpoints[1].host")
        #expect(warnings.first?.message == "Tower lists \"tower.example.\", which this app never dials, scans or pins: it is not a name or address it accepts.")
    }

    @Test("a wake path that cannot work is a warning, not a refusal")
    func warnings() throws {
        let json = """
        { "version": 1,
          "sites": [ { "id": "home", "name": "Home" }, { "id": "away", "name": "Away" } ],
          "machines": [
            { "id": "pi", "site": "away", "endpoints": [{"id":"e","host":"h"}],
              "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] },
            { "id": "tower", "site": "home", "endpoints": [{"id":"e2","host":"h2"}],
              "wake": { "mac": "AA:BB:CC:DD:EE:FF", "helpers": [{ "machine": "pi", "action": "w" }] },
              "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] },
            { "id": "orphan", "endpoints": [{"id":"e3","host":"h3"}],
              "wake": { "mac": "AA:BB:CC:DD:EE:01" },
              "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] }
          ] }
        """
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }
        let warnings = store.warnings.map(\.message)
        // A helper at another site is legitimate through a router, so it is said rather than refused.
        #expect(warnings.contains { $0.contains("different sites") })
        // And a machine nothing can reach is worth knowing about while you are editing, not the
        // first time you need it.
        #expect(warnings.contains { $0.contains("only be woken by a device that is already on its network") })
        #expect(warnings.contains { $0.contains("always on") })
    }

    @Test("a maintenance window end is HH:MM in ASCII digits, nothing that only parses like one")
    func clockTimes() {
        #expect(MaintenanceWindow(from: "02:00", to: "04:00").looksValid)
        #expect(!MaintenanceWindow(from: "+1:00", to: "04:00").looksValid)
        #expect(!MaintenanceWindow(from: "\u{0660}\u{0662}:00", to: "04:00").looksValid)
        #expect(!MaintenanceWindow(from: "24:00", to: "04:00").looksValid)
    }

    // MARK: - Editing

    @Test("an edit bumps the revision and records what it came from")
    func editStampsLineage() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }

        let before = try #require(store.document)
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Raspberry Pi"], in: &root)
        } == nil)

        let after = try #require(store.document)
        #expect(after.identity.revisionNumber == before.identity.revisionNumber + 1)
        #expect(after.identity.ancestors.first == before.hash)
        #expect(after.identity.device == "MacBook")
        #expect(after.identity.source == "mac")
        #expect(store.config?.machine(id: "pi")?.name == "Raspberry Pi")

        // The bytes it replaced are kept, which is what a merge needs and what an undo needs.
        #expect(store.revisionBytes(hash: before.hash) != nil)
    }

    @Test("an edit that would produce an invalid document changes nothing on disk")
    func invalidEditIsRefused() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }
        let before = try #require(store.document)

        let problem = store.applyEdit(describedAs: "broke it", deviceName: "MacBook") { root in
            try ControllerEditor.removeMachine(id: "pi", in: &root)   // tower's helper points at it
        }
        #expect(problem != nil)
        #expect(store.document?.hash == before.hash)
    }

    @Test("keys this build has never heard of survive an edit")
    func unknownKeysSurviveAnEdit() throws {
        let json = """
        { "version": 1, "futureTopLevel": { "a": 1 },
          "machines": [ { "id": "pi", "futureMachineKey": "kept",
            "endpoints": [{"id":"e","host":"h"}],
            "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }

        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Pi"], in: &root)
        } == nil)

        let raw = try #require(store.rawBytes)
        let object = try #require((try JSONSerialization.jsonObject(with: raw)) as? [String: Any])
        #expect(object["futureTopLevel"] != nil)
        let machines = try #require(object["machines"] as? [[String: Any]])
        #expect(machines.first?["futureMachineKey"] as? String == "kept")
    }

    @Test("an edit writes back untouched what it did not change: number spelling, member order, unknown controller keys")
    func editKeepsSpellingAndOrder() throws {
        // The Mac used to sort every key, write 22.0 as 22, and replace the controller block with its
        // own, so a rename here changed a peer's spelling everywhere and dropped what it had added (C6).
        let json = """
        {
          "version": 1,
          "controller": { "id": "setup-home", "revision": 3, "policy": { "keep": true }, "lineage": [] },
          "machines": [
            { "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}], "id": "pi", "weight": 22.0,
              "endpoints": [{"id":"e","host":"h","port":22.0}] },
            { "id": "tower", "name": "Tower", "endpoints": [{"id":"e","host":"h2"}],
              "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] }
          ]
        }
        """
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }

        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "tower", "name": "Big Tower"], in: &root)
        } == nil)

        let text = String(decoding: try #require(store.rawBytes), as: UTF8.self)
        #expect(text.contains(#""weight": 22.0"#))
        #expect(text.contains(#""port": 22.0"#))
        let tree = try SetupJSON.parse(try #require(store.rawBytes))
        #expect(tree.members?.map(\.name) == ["version", "controller", "machines"])
        #expect(tree["machines"]?.items?.first?.members?.map(\.name) == ["systems", "id", "weight", "endpoints"])
        #expect(tree["machines"]?.items?.last?["name"] == .string("Big Tower"))
        let block = try #require(tree["controller"])
        #expect(block["policy"]?["keep"] == .bool(true))
        #expect(block["revision"] == .number("4"))
        #expect(block.members?.map(\.name) == ["id", "revision", "policy", "lineage", "updatedAt", "source", "device"])
    }

    @Test("an edit elsewhere keeps numbers no double holds exactly as they were written")
    func editKeepsNumbersADoubleCannotHold() throws {
        // Every number went through a double and was written out again from it, so an edit anywhere
        // turned 12345678901234567890 into 1.2345678901234567e+19 and 0.10000000000000000001 into
        // 0.1, values nobody changed, and a document holding 1e400 could not be edited at all.
        let json = """
        {
          "version": 1,
          "futureTopLevel": { "big": 12345678901234567890, "fine": 0.10000000000000000001, "huge": 1e400 },
          "machines": [
            { "id": "pi", "name": "Pi", "futureMachineKey": [1e400, 0.10000000000000000001, 12345678901234567890],
              "endpoints": [{"id":"e","host":"h"}],
              "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] },
            { "id": "tower", "name": "Tower", "endpoints": [{"id":"e","host":"h2"}],
              "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] }
          ]
        }
        """
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }
        func occurrences(_ lexeme: String) throws -> Int {
            String(decoding: try #require(store.rawBytes), as: UTF8.self).components(separatedBy: lexeme).count - 1
        }

        // An edit to another machine, and one to the machine that holds some of them.
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "tower", "name": "Big Tower"], in: &root)
        } == nil)
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Kitchen Pi"], in: &root)
        } == nil)
        #expect(store.config?.machine(id: "pi")?.name == "Kitchen Pi")
        for lexeme in ["12345678901234567890", "0.10000000000000000001", "1e400"] {
            #expect(try occurrences(lexeme) == 2, "\(lexeme) was not written back as it was")
        }
        let tree = try SetupJSON.parse(try #require(store.rawBytes))
        #expect(tree["futureTopLevel"]?["huge"] == .number("1e400"))
        #expect(tree["machines"]?.items?.first?["futureMachineKey"]?.items
                == [.number("1e400"), .number("0.10000000000000000001"), .number("12345678901234567890")])

        // A hand edit is recorded as a revision through the same path, and keeps them as well.
        let text = String(decoding: try #require(store.rawBytes), as: UTF8.self)
        Thread.sleep(forTimeInterval: 0.01)
        try Data(text.replacingOccurrences(of: "\"Big Tower\"", with: "\"Hand Tower\"").utf8).write(to: store.url)
        store.reloadIfChanged()
        #expect(store.hasPendingExternalEdit)
        #expect(store.reconcileExternalEdit(deviceName: "MacBook") == nil)
        #expect(store.config?.machine(id: "tower")?.name == "Hand Tower")
        for lexeme in ["12345678901234567890", "0.10000000000000000001", "1e400"] {
            #expect(try occurrences(lexeme) == 2, "\(lexeme) was not kept by the hand edit")
        }

        // A number the change itself brings is written the way JavaScript writes it, and one no JSON
        // can hold is still refused.
        #expect(store.applyEdit(describedAs: "added", deviceName: "MacBook") { root in
            root["futureAdded"] = 0.25
        } == nil)
        #expect(try SetupJSON.parse(try #require(store.rawBytes))["futureAdded"] == .number("0.25"))
        #expect(store.applyEdit(describedAs: "added", deviceName: "MacBook") { root in
            root["futureInfinite"] = Double.infinity
        } != nil)
        #expect(try occurrences("1e400") == 2)
    }

    @Test("renaming the setup keeps the new name")
    func renameKeepsTheName() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }
        #expect(store.applyEdit(describedAs: "named", deviceName: "MacBook") { root in
            try ControllerEditor.setSetupName("Home", in: &root)
        } == nil)
        #expect(store.applyEdit(describedAs: "renamed", deviceName: "MacBook") { root in
            try ControllerEditor.setSetupName("Cabin", in: &root)
        } == nil)
        #expect(store.config?.identity.name == "Cabin")
    }

    // MARK: - Adopting

    /// A store applying a setup with an identity, and a child of it as another device wrote it.
    static func storeWithChild() throws -> (ConfigStore, URL, child: Data) {
        let (store, home) = try Self.store(Self.twoMachines)
        #expect(store.applyEdit(describedAs: "identity", deviceName: "MacBook") { _ in } == nil)
        let raw = try #require(store.rawBytes)
        let child = try ControllerEditor.apply(to: raw, parentHash: try Canonical.hash(raw),
                                               parentIdentity: try #require(store.config).identity, deviceName: "Pixel") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Kitchen Pi"], in: &root)
        }.bytes
        return (store, home, child)
    }

    @Test("an adoption is written over the head it was decided against, and over nothing else")
    func adoptionIsACompareAndSwap() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)

        // A person saves an edit between the decision and the write (M16).
        #expect(store.applyEdit(describedAs: "edit", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "tower", "name": "Big Tower"], in: &root)
        } == nil)
        let edited = try #require(store.document?.hash)
        #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .headMoved(current: edited))
        #expect(store.document?.hash == edited)
        #expect(store.config?.machine(id: "tower")?.name == "Big Tower")

        // Against the head as it is now, the same bytes are written exactly as they came.
        #expect(store.adopt(child, describedAs: "adopted", replacing: edited) == .adopted(hash: try Canonical.hash(child)))
        #expect(store.rawBytes == child)
    }

    @Test("an edit saved by another program is noticed at the moment of writing")
    func adoptionRereadsTheFile() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)

        // A revision written straight to the file, which the watcher has not reported yet.
        let raw = try #require(store.rawBytes)
        let other = try ControllerEditor.apply(to: raw, parentHash: head, parentIdentity: try #require(store.config).identity,
                                               deviceName: "Editor") { root in
            try ControllerEditor.upsertMachine(["id": "tower", "name": "Hand Tower"], in: &root)
        }.bytes
        Thread.sleep(forTimeInterval: 0.01)
        try other.write(to: store.url)
        #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .headMoved(current: try Canonical.hash(other)))
        #expect(store.rawBytes == other)
    }

    @Test("a hand edit not yet recorded as a revision always reads as a moved head")
    func pendingHandEditIsAMovedHead() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)

        // The same controller block, so this is an edit waiting to be given a revision.
        let raw = String(decoding: try #require(store.rawBytes), as: UTF8.self)
        Thread.sleep(forTimeInterval: 0.01)
        try Data(raw.replacingOccurrences(of: "\"Tower\"", with: "\"Hand Tower\"").utf8).write(to: store.url)
        store.reloadIfChanged()
        #expect(store.hasPendingExternalEdit)
        guard case .headMoved = store.adopt(child, describedAs: "adopted", replacing: head) else {
            Issue.record("a pending hand edit was overwritten")
            return
        }
        #expect(store.config?.machine(id: "tower")?.name == "Hand Tower")
    }

    @Test("two adoptions decided against one head cannot both land")
    func secondAdoptionFindsTheFirst() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)
        let raw = try #require(store.rawBytes)
        let sibling = try ControllerEditor.apply(to: raw, parentHash: head, parentIdentity: try #require(store.config).identity,
                                                 deviceName: "Desk tower") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Hall Pi"], in: &root)
        }.bytes

        #expect(store.adopt(child, describedAs: "first", replacing: head) == .adopted(hash: try Canonical.hash(child)))
        #expect(store.adopt(sibling, describedAs: "second", replacing: head) == .headMoved(current: try Canonical.hash(child)))
        #expect(store.rawBytes == child)
    }

    @Test("an adoption is refused for bytes section 12 or 9.7 refuses")
    func adoptionChecksTheBytes() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)
        let broken = Data(String(decoding: child, as: UTF8.self).replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"ghost\"").utf8)
        guard case .refused(let why) = store.adopt(broken, describedAs: "adopted", replacing: head) else {
            Issue.record("a document with an unknown helper was adopted")
            return
        }
        #expect(why.contains("ghost"))
        guard case .refused = store.adopt(Data([0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF]) + child, describedAs: "adopted", replacing: head) else {
            Issue.record("a document behind two byte order marks was adopted")
            return
        }
        #expect(store.document?.hash == head)
    }

    @Test("a half-saved file is a moved head, and nothing is adopted over it")
    func adoptionKeepsAHalfSavedFile() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)
        let raw = try #require(store.rawBytes)

        // M16: the file fails to parse, so the app runs on the document before it, and that one is
        // still the head the adoption was decided against. What storage holds is not.
        for unfinished in [Data(raw.dropLast(3)) + Data(",\n".utf8), Data([0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF]) + raw] {
            Thread.sleep(forTimeInterval: 0.01)
            try unfinished.write(to: store.url)
            store.reloadIfChanged()
            #expect(store.problem != nil)
            #expect(store.config?.machine(id: "pi")?.name == "Pi")
            #expect(store.document == nil)

            #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .headMoved(current: nil))
            #expect(try Data(contentsOf: store.url) == unfinished)
            // With nothing expected, which is how enrolment and giving a setup an identity come, the
            // file is refused outright rather than replaced.
            guard case .refused(let why) = store.enrol(child, describedAs: "enrolled") else {
                Issue.record("a file that is not a document was enrolled over")
                return
            }
            #expect(why.contains("not a setup document"))
            #expect(try Data(contentsOf: store.url) == unfinished)
        }

        // Saved whole again, it is the head once more, and the adoption goes ahead.
        Thread.sleep(forTimeInterval: 0.01)
        try raw.write(to: store.url)
        #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .adopted(hash: try Canonical.hash(child)))
        #expect(store.rawBytes == child)
    }

    @Test("a file section 12 accepts but that is no setup this app can read is not adopted over either")
    func adoptionKeepsAFileThatIsNoSetup() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)
        let unfinished = Data(#"{ "version": 1.5, "machines": [] }"#.utf8)
        #expect(Canonical.rejection(unfinished) == nil)
        Thread.sleep(forTimeInterval: 0.01)
        try unfinished.write(to: store.url)

        // Not reloaded first: the adoption reads the file itself.
        #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .headMoved(current: nil))
        #expect(store.problem != nil)
        #expect(try Data(contentsOf: store.url) == unfinished)
    }

    @Test("a save the poll cannot see is loaded by the adoption that finds it")
    func adoptionLoadsASaveThePollMissed() throws {
        let (store, home, child) = try Self.storeWithChild()
        defer { try? FileManager.default.removeItem(at: home) }
        let head = try #require(store.document?.hash)
        let raw = try #require(store.rawBytes)

        // The poll compares the modification time and the size. Each save below is pinned to a time
        // and keeps the size, so the poll cannot tell it from the file before.
        func save(_ data: Data, at seconds: TimeInterval) throws {
            try data.write(to: store.url)
            try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: seconds)],
                                                  ofItemAtPath: store.path)
        }
        try save(raw, at: 1_700_000_000)
        store.reloadIfChanged()
        let signature = ConfigStore.signature(of: store.url)

        // A hand edit of the same length. Nothing is written over it, and what is applied here is
        // the edit, not the head it replaced: deciding again from that head would only fetch the
        // same bytes and find the head moved once more, round after round.
        let edit = Data(String(decoding: raw, as: UTF8.self).replacingOccurrences(of: "\"Tower\"", with: "\"Tawer\"").utf8)
        try #require(edit.count == raw.count && edit != raw)
        try save(edit, at: 1_700_000_000)
        try #require(ConfigStore.signature(of: store.url) == signature)
        store.reloadIfChanged()
        #expect(store.document?.hash == head)

        #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .headMoved(current: try Canonical.hash(edit)))
        #expect(try Data(contentsOf: store.url) == edit)
        #expect(store.document?.hash != head)
        #expect(store.hasPendingExternalEdit)
        #expect(store.config?.machine(id: "tower")?.name == "Tawer")

        // And back: a half-saved file saved whole again in the same second and to the same size is
        // the head once more, and the adoption goes ahead rather than finding a file that is gone.
        let unfinished = Data(String(decoding: raw, as: UTF8.self).replacingOccurrences(of: "\"Tower\"", with: "\"Tower,").utf8)
        try #require(unfinished.count == raw.count)
        try save(unfinished, at: 1_700_000_100)
        store.reloadIfChanged()
        #expect(store.hasBrokenFile)
        let brokenSignature = ConfigStore.signature(of: store.url)
        try save(raw, at: 1_700_000_100)
        try #require(ConfigStore.signature(of: store.url) == brokenSignature)
        #expect(store.adopt(child, describedAs: "adopted", replacing: head) == .adopted(hash: try Canonical.hash(child)))
        #expect(store.rawBytes == child)
    }

    @Test("setting helpers writes the compatibility alias too")
    func helpersWriteTheAlias() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }

        #expect(store.applyEdit(describedAs: "helpers", deviceName: "MacBook") { root in
            try ControllerEditor.setWakeHelpers(
                [WakeHelper(machine: "pi", action: "wake-tower"),
                 WakeHelper(machine: "pi", action: "wake-tower-2")],
                onMachine: "tower", in: &root
            )
        } == nil)

        let raw = try #require(store.rawBytes)
        let object = try #require((try JSONSerialization.jsonObject(with: raw)) as? [String: Any])
        let machines = try #require(object["machines"] as? [[String: Any]])
        let tower = try #require(machines.first { $0["id"] as? String == "tower" })
        let wake = try #require(tower["wake"] as? [String: Any])
        // A 1.2 client reads only the singular key, so it keeps working.
        let singular = try #require(wake["helper"] as? [String: Any])
        #expect(singular["action"] as? String == "wake-tower")
        #expect((wake["helpers"] as? [[String: Any]])?.count == 2)
    }

    @Test("an id outside the agent's grammar is refused by the editor")
    func editorValidatesIds() throws {
        var root: [String: Any] = ["version": 1, "machines": []]
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertMachine(["id": "has space"], in: &root)
        }
    }

    @Test("an address the app would never dial is refused by the editor, naming the part that fails")
    func editorRefusesUndialableAddresses() throws {
        // C-2: the schema lets these through and the app only warns about one a peer wrote, but an
        // editor is where a person types it. Saved, it would be skipped on every call.
        var root: [String: Any] = ["version": 1, "machines": [[
            "id": "tower", "endpoints": [["id": "lan", "host": "192.0.2.10", "user": "me"]],
            "systems": [["id": "linux", "platform": "linux", "agent": ["node", "/a"]]]
        ] as [String: Any]]]
        let refusals: [([String: Any], String)] = [
            (["id": "bad", "host": "victim.example,*"], "the host \"victim.example,*\""),
            (["id": "bad", "host": "-oProxyCommand=x"], "the host \"-oProxyCommand=x\""),
            (["id": "bad", "host": "192.0.2.11", "user": "me$(id)"], "the user \"me$(id)\""),
            (["id": "bad", "host": "192.0.2.11", "port": 70000], "the port 70000"),
            // A merge that leaves an old host with a new, bad port is one bad address too.
            (["id": "lan", "port": 0], "the port 0")
        ]
        for (endpoint, part) in refusals {
            do {
                try ControllerEditor.upsertEndpoint(endpoint, onMachine: "tower", in: &root)
                Issue.record("saved \(endpoint)")
            } catch let failure as ControllerEditor.EditFailure {
                #expect(failure.message.contains(part), "\(failure.message)")
            }
        }
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertMachine(["id": "nas", "ssh": ["host": "nas.example,*"]], in: &root)
        }
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertMachine(["id": "nas", "endpoints": [["id": "e", "host": "nas.example", "port": 0]]], in: &root)
        }
        // Nothing refused was written, and a good address still is.
        let tower = try #require((root["machines"] as? [[String: Any]])?.first)
        #expect((tower["endpoints"] as? [[String: Any]])?.count == 1)
        #expect((tower["endpoints"] as? [[String: Any]])?.first?["port"] == nil)
        #expect((root["machines"] as? [[String: Any]])?.count == 1)
        try ControllerEditor.upsertEndpoint(["id": "remote", "host": "desk-tower.local", "port": 2222], onMachine: "tower", in: &root)
        try ControllerEditor.upsertMachine(["id": "nas", "endpoints": [["id": "e", "host": "2001:db8::5", "user": "me"]]], in: &root)
    }

    @Test("a Windows system the editor writes always names its shell, and keeps one it had")
    func editorWritesWindowsShell() throws {
        var root: [String: Any] = ["version": 1, "machines": [[
            "id": "tower",
            "systems": [["id": "windows", "platform": "windows", "agent": ["node", "C:\\lc\\agent\\src\\index.mjs"]],
                        ["id": "linux", "platform": "linux", "agent": ["node", "/a"]],
                        ["id": "pwsh", "platform": "windows", "shell": "powershell", "agent": ["node", "C:\\a.mjs"]]]
        ] as [String: Any]]]
        // The launcher migration changes only the agent, and the system it touches gains cmd, the
        // shell every 1.4 client already writes its lines in.
        try ControllerEditor.upsertSystem(["id": "windows", "agent": ["node", "C:\\lc\\bin\\launcher.mjs"]],
                                          onMachine: "tower", in: &root)
        try ControllerEditor.upsertSystem(["id": "linux", "agent": ["node", "/b"]], onMachine: "tower", in: &root)
        try ControllerEditor.upsertSystem(["id": "pwsh", "agent": ["node", "C:\\b.mjs"]], onMachine: "tower", in: &root)
        let systems = try #require(((root["machines"] as? [[String: Any]])?.first)?["systems"] as? [[String: Any]])
        #expect(systems[0]["shell"] as? String == "cmd")
        #expect(systems[1]["shell"] == nil)
        #expect(systems[2]["shell"] as? String == "powershell")
    }

    @Test("a document with no identity gets one only when it is adopted, not on every load")
    func adoptingGivesAnIdentity() throws {
        let json = """
        { "version": 1, "machines": [ { "id": "pi", "endpoints": [{"id":"e","host":"h"}],
          "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }

        // An id invented on every load would make two devices reading the same file believe they
        // were on different setups.
        #expect(store.config?.identity.id == nil)

        let raw = try #require(store.rawBytes)
        let adopted = try ControllerEditor.adopting(raw, deviceName: "MacBook")
        #expect(adopted.identity.id?.hasPrefix("setup-") == true)
        #expect(adopted.identity.revisionNumber == 1)
        #expect(adopted.identity.ancestors.isEmpty)
        #expect(store.adopt(adopted.bytes, describedAs: "gave it an identity", replacing: store.document?.hash)
                == .adopted(hash: try Canonical.hash(adopted.bytes)))
        #expect(store.config?.identity.id == adopted.identity.id)
    }

    @Test("restoring a kept revision counts forward rather than winding the number back")
    func restoreIsAnOrdinaryEdit() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }

        _ = store.applyEdit(describedAs: "first", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "One"], in: &root)
        }
        let first = try #require(store.document)
        _ = store.applyEdit(describedAs: "second", deviceName: "MacBook") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Two"], in: &root)
        }
        let second = try #require(store.document)

        let revision = try #require(store.storedRevisions().first { $0.hash == first.hash })
        #expect(store.restore(revision, deviceName: "MacBook") == nil)

        let restored = try #require(store.document)
        #expect(store.config?.machine(id: "pi")?.name == "One")
        // A document that wound the counter back would be one every machine refuses, and rightly:
        // it descends from nothing they hold.
        #expect(restored.identity.revisionNumber == second.identity.revisionNumber + 1)
        #expect(restored.identity.ancestors.first == second.hash)
    }

    @Test("moving private settings out of the shared document is an explicit edit")
    func stripPrivateKeys() throws {
        let json = """
        { "version": 1, "local": { "enabled": true, "name": "This Mac", "agent": "~/a.mjs" },
          "machines": [ { "id": "pi", "ssh": { "host": "pi", "identityFile": "/Users/someone/.ssh/id" },
            "endpoints": [{"id":"e","host":"h"}],
            "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let (store, home) = try Self.store(json)
        defer { try? FileManager.default.removeItem(at: home) }
        // Read, but never stripped on its own: removing it changes the bytes and therefore the hash,
        // and doing that quietly on one device looks like a divergence to every other.
        #expect(store.config?.machine(id: "pi")?.ssh?.identityFile != nil)

        #expect(store.applyEdit(describedAs: "moved private settings out", deviceName: "MacBook") { root in
            try ControllerEditor.stripPrivateKeys(in: &root)
        } == nil)
        #expect(store.config?.machine(id: "pi")?.ssh?.identityFile == nil)
        #expect(store.config?.local == nil)
        // The host is kept: it is the address, not a secret.
        #expect(store.config?.machine(id: "pi")?.ssh?.host == "pi")
    }

    @Test("a file edited into something unreadable leaves the previous config in force")
    func brokenFileKeepsThePreviousConfig() throws {
        let (store, home) = try Self.store(Self.twoMachines)
        defer { try? FileManager.default.removeItem(at: home) }
        #expect(store.config != nil)

        try Data("{ not json".utf8).write(to: store.url)
        store.reloadIfChanged()
        #expect(store.problem != nil)
        // A half-saved file is the normal state of a file being edited.
        #expect(store.config?.machines.count == 2)
    }

    @Test("a hand edit made while the app is closed becomes a child revision on restart")
    func offlineHandEditGetsLineage() throws {
        let home = try Self.temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let file = home.appending(path: "config.json")
        let original = """
        { "version": 1,
          "controller": { "id": "setup-home", "revision": 5, "source": "phone",
                          "device": "Pixel", "lineage": [] },
          "machines": [ { "id": "pi", "name": "Before", "futureKey": { "kept": true },
            "endpoints": [{"id":"lan","host":"10.0.0.5"}],
            "systems": [{"id":"linux","agent":["node","/agent/index.mjs"]}] } ] }
        """
        try Data(original.utf8).write(to: file)

        var first: ConfigStore? = ConfigStore(url: file)
        let parentHash = try #require(first?.document?.hash)
        #expect(first?.revisionBytes(hash: parentHash) != nil)
        first = nil

        let editedByHand = original.replacingOccurrences(of: "\"Before\"", with: "\"After\"")
        try Data(editedByHand.utf8).write(to: file)

        let reopened = ConfigStore(url: file)
        #expect(reopened.hasPendingExternalEdit)
        #expect(reopened.document == nil)
        #expect(reopened.reconcileExternalEdit(deviceName: "MacBook") == nil)

        let applied = try #require(reopened.document)
        #expect(applied.identity.revisionNumber == 6)
        #expect(applied.identity.ancestors.first == parentHash)
        #expect(applied.identity.source == "mac")
        #expect(applied.identity.device == "MacBook")
        #expect(reopened.config?.machine(id: "pi")?.name == "After")
        let root = try #require((try JSONSerialization.jsonObject(with: applied.bytes)) as? [String: Any])
        let machines = try #require(root["machines"] as? [[String: Any]])
        let future = try #require(machines.first?["futureKey"] as? [String: Any])
        #expect(future["kept"] as? Bool == true)
        #expect(!reopened.hasPendingExternalEdit)
    }

    // MARK: - Private bindings

    @Test("bindings are read, written and kept out of the shared document")
    func bindingsRoundTrip() throws {
        let home = try Self.temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let store = BindingsStore(url: home.appending(path: "bindings.json"))

        #expect(store.bindings == .empty)
        #expect(!store.isPresent)
        #expect(store.update {
            $0.deviceName = "Tower"
            $0.selfBinding = Bindings.SelfBinding(machine: "tower", system: "windows")
            $0.localAgent = Bindings.LocalAgentBinding(argv: ["/usr/bin/node", "/agent/index.mjs"])
            $0.identityFile = "~/.ssh/legion"
            $0.currentSite = "home"
        } == nil)

        let reopened = BindingsStore(url: store.url)
        #expect(reopened.isPresent)
        #expect(reopened.bindings.deviceName == "Tower")
        #expect(reopened.bindings.isSelf("tower"))
        #expect(reopened.bindings.canControlSelfLocally)
        #expect(reopened.bindings.currentSite == "home")

        // The file is JSON a person can read, and carries no key material.
        let text = try String(contentsOf: store.url, encoding: .utf8)
        #expect(text.contains("\"self\""))
        #expect(!text.contains("PRIVATE KEY"))
    }

    @Test("a bindings file that asks for a shell is refused, and the settings from before it stay")
    func bindingsWithAShellAreRefused() throws {
        let home = try Self.temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let store = BindingsStore(url: home.appending(path: "bindings.json"))
        #expect(store.update { $0.deviceName = "Desk tower"; $0.selfBinding = .init(machine: "tower") } == nil)

        // A local agent is spawned as an argv; a shell beside it has nothing on its far side, and
        // the decoder used to drop the key and run the argv as though it were not there.
        try Data(#"{"deviceName":"Changed","self":{"machine":"tower"},"localAgent":{"argv":["node","/a.mjs"],"shell":"cmd"}}"#.utf8)
            .write(to: store.url)
        store.reloadIfChanged()
        #expect(store.bindings.deviceName == "Desk tower")
        #expect(store.problem?.contains("localAgent.shell is not a setting") == true)

        #expect(Bindings.problems(in: Data(#"{"localAgent":{"argv":[]}}"#.utf8)).count == 1)
        #expect(Bindings.problems(in: Data(#"{"self":{"system":"linux"}}"#.utf8)).count == 1)
        #expect(Bindings.problems(in: Data("[]".utf8)).count == 1)
        // Keys this build does not know are left alone, as they are everywhere else.
        #expect(Bindings.problems(in: Data(#"{"knownHostsFile":"~/kh","machines":{"pi":{"sshAlias":"pi","port":2222}}}"#.utf8)).isEmpty)
    }

    @Test("private bindings retire the deprecated shared local block on this device")
    func bindingsReplaceSharedLocal() throws {
        let home = try Self.temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let configFile = home.appending(path: "config.json")
        try Data("""
        { "version": 1, "local": { "enabled": true, "name": "Old Mac", "agent": "/tmp/agent.mjs" },
          "machines": [] }
        """.utf8).write(to: configFile)

        let withoutBindings = AppModel(
            config: ConfigStore(url: configFile),
            bindings: BindingsStore(url: home.appending(path: "absent-bindings.json")),
            operations: OperationStore(url: home.appending(path: "old-operations.json"))
        )
        #expect(withoutBindings.mac != nil)

        let bindingStore = BindingsStore(url: home.appending(path: "bindings.json"))
        #expect(bindingStore.update { $0.deviceName = "Current device" } == nil)
        let withBindings = AppModel(
            config: ConfigStore(url: configFile),
            bindings: bindingStore,
            operations: OperationStore(url: home.appending(path: "operations.json"))
        )
        #expect(withBindings.mac == nil)
    }

    @Test("the key for a machine is its own, then this device's, then the deprecated shared one")
    func identityPrecedence() {
        var bindings = Bindings()
        #expect(bindings.identityFile(forMachine: "pi", sharedFallback: "/shared") == "/shared")
        bindings.identityFile = "/device"
        #expect(bindings.identityFile(forMachine: "pi", sharedFallback: "/shared") == "/device")
        bindings.machines = ["pi": Bindings.MachineBinding(identityFile: "/pi", sshAlias: nil, port: nil, user: nil)]
        #expect(bindings.identityFile(forMachine: "pi", sharedFallback: "/shared") == "/pi")
    }

    @Test("isolated preferences stay in their own state file")
    func filePreferencesRoundTrip() throws {
        let home = try Self.temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let url = home.appending(path: "state/preferences.json")
        let preferences = FilePreferences(url: url)

        preferences.set(Data([0, 1, 2, 255]), forKey: "data")
        preferences.set("window {42}", forKey: "string")
        preferences.set(false, forKey: "bool")

        let reopened = FilePreferences(url: url)
        #expect(reopened.data(forKey: "data") == Data([0, 1, 2, 255]))
        #expect(reopened.string(forKey: "string") == "window {42}")
        #expect(reopened.bool(forKey: "bool") == false)

        reopened.removeObject(forKey: "string")
        #expect(FilePreferences(url: url).string(forKey: "string") == nil)
        #expect(FilePreferences(url: url).bool(forKey: "bool") == false)
    }

    @Test("changing the bound local argv replaces the transport in place and uses this device's name")
    func boundTransportTracksEdits() throws {
        let home = try Self.temporaryHome()
        defer { try? FileManager.default.removeItem(at: home) }
        let configFile = home.appending(path: "config.json")
        try Data("""
        {"version":1,"machines":[{"id":"tower","name":"Shared name","systems":[{"id":"mac","agent":["node","/agent.mjs"]}]}]}
        """.utf8).write(to: configFile)
        let bindings = BindingsStore(url: home.appending(path: "bindings.json"))
        bindings.update {
            $0.deviceName = "My local name"
            $0.selfBinding = .init(machine: "tower", system: "mac")
            $0.localAgent = .init(argv: ["/bin/echo", "first"])
        }
        let app = AppModel(config: ConfigStore(url: configFile), bindings: bindings,
                           operations: OperationStore(url: home.appending(path: "operations.json")))
        let before = try #require(app.mac)
        #expect(before.name == "My local name")
        #expect(app.machines.isEmpty)
        bindings.update { $0.localAgent = .init(argv: ["/bin/echo", "second"]) }
        // The same model, so nothing in flight on this device is left behind in one the app no
        // longer shows; only the command it spawns its agent with has changed.
        #expect(app.mac === before)
        #expect(app.mac?.boundAgentArgv == ["/bin/echo", "second"])
    }
}
