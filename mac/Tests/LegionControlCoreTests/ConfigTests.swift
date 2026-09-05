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

    // MARK: - Validation

    @Test("a helper that does not exist, is the machine itself, or forms a loop is refused")
    func helperValidation() throws {
        func problem(_ json: String) -> String? {
            guard let data = json.data(using: .utf8),
                  let decoded = try? JSONDecoder().decode(ControllerConfig.self, from: data) else {
                return "did not decode"
            }
            do {
                _ = try decoded.validated()
                return nil
            } catch let failure as ConfigProblem {
                return failure.message
            } catch {
                return error.localizedDescription
            }
        }

        #expect(problem(Self.twoMachines) == nil)

        let missing = Self.twoMachines.replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"ghost\"")
        #expect(problem(missing)?.contains("not a machine in this setup") == true)

        let itself = Self.twoMachines.replacingOccurrences(of: "\"machine\": \"pi\"", with: "\"machine\": \"tower\"")
        // A machine that is asleep cannot wake itself, so this is always a mistake.
        #expect(problem(itself)?.contains("its own wake helper") == true)

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
        #expect(problem(cycle)?.contains("loop") == true)

        // The compatibility key has to name the same machine as the first of the list: a document
        // where they disagree wakes different things depending on which app is open.
        let mismatch = Self.twoMachines.replacingOccurrences(
            of: "\"helper\": { \"machine\": \"pi\", \"action\": \"wake-tower\" }",
            with: "\"helper\": { \"machine\": \"ghost\", \"action\": \"wake-tower\" }")
        #expect(problem(mismatch)?.contains("compatibility") == true)
    }

    @Test("a machine naming a site that does not exist is refused")
    func siteValidation() throws {
        let json = Self.twoMachines.replacingOccurrences(of: "\"site\": \"home\"", with: "\"site\": \"nowhere\"")
        let data = try #require(json.data(using: .utf8))
        let decoded = try JSONDecoder().decode(ControllerConfig.self, from: data)
        #expect(throws: ConfigProblem.self) { _ = try decoded.validated() }
    }

    @Test("a lineage of the wrong shape is refused rather than half-trusted")
    func lineageValidation() throws {
        let json = """
        { "version": 1, "controller": { "id": "s", "revision": 2, "lineage": ["short"] },
          "machines": [ { "id": "a", "endpoints": [{"id":"e","host":"h"}],
            "systems": [{"id":"l","platform":"linux","agent":["node","/a"]}] } ] }
        """
        let decoded = try JSONDecoder().decode(ControllerConfig.self, from: Data(json.utf8))
        #expect(throws: ConfigProblem.self) { _ = try decoded.validated() }
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
        let config = try JSONDecoder().decode(ControllerConfig.self, from: Data(json.utf8)).validated()
        let warnings = config.warnings()
        // A helper at another site is legitimate through a router, so it is said rather than refused.
        #expect(warnings.contains { $0.contains("different sites") })
        // And a machine nothing can reach is worth knowing about while you are editing, not the
        // first time you need it.
        #expect(warnings.contains { $0.contains("only be woken by a device that is already on its network") })
        #expect(warnings.contains { $0.contains("always on") })
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
        #expect(store.adopt(adopted.bytes, describedAs: "gave it an identity") == nil)
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

    @Test("changing the bound local argv replaces the transport and uses this device's name")
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
        #expect(app.mac !== before)
        #expect(app.mac?.boundAgentArgv == ["/bin/echo", "second"])
    }
}
