import Foundation
import Testing
@testable import LegionControlCore

/// What the app does with its models when the setup, the bindings or an action changes under them.
///
/// Nothing here dials anything: no viewer is on screen, so nothing polls, and the only action run
/// goes to a local agent command that does not exist.
@MainActor
struct AppModelTests {

    static func home() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-app-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    static func app(_ json: String, in home: URL, bindings: BindingsStore? = nil) throws -> AppModel {
        let file = home.appending(path: "config.json")
        try Data(json.utf8).write(to: file)
        return AppModel(
            config: ConfigStore(url: file),
            bindings: bindings ?? BindingsStore(url: home.appending(path: "absent-bindings.json")),
            operations: OperationStore(url: home.appending(path: "operations.json")),
            appUpdates: AppUpdateModel(preferences: FilePreferences(url: home.appending(path: "preferences.json")),
                                       installedVersion: "1.4.0"),
            remembered: .inMemory()
        )
    }

    static let twoMachines = """
    {
      "version": 1,
      "machines": [
        { "id": "pi", "name": "Pi",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.0.2.5", "user": "me" } ],
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
        { "id": "tower", "name": "Tower",
          "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.0.2.9", "user": "me" } ],
          "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] }
      ]
    }
    """

    /// A setup with an identity, and the divergence sheet as a peer holding a sibling of it would
    /// raise it: built from the document applied now.
    static func diverged(in home: URL) throws -> (AppModel, SetupDivergence) {
        let app = try Self.app(Self.twoMachines.replacingOccurrences(
            of: #""version": 1,"#,
            with: #""version": 1, "controller": { "id": "setup-home", "revision": 1, "lineage": [] },"#), in: home)
        let mine = try #require(app.config.document)
        let theirs = try ControllerEditor.apply(to: mine.bytes, parentHash: mine.hash, parentIdentity: mine.identity,
                                                deviceName: "Pixel") { root in
            try ControllerEditor.upsertMachine(["id": "pi", "name": "Kitchen Pi"], in: &root)
        }.bytes
        let differences = try SetupMerge.differences(base: mine.bytes, mine: mine.bytes, theirs: theirs)
        let divergence = SetupDivergence(machineId: "pi", machineName: "Pi", mine: mine, theirsBytes: theirs,
                                         theirsHash: try Canonical.hash(theirs),
                                         theirsIdentity: ControllerIdentity(block: try SetupJSON.parse(theirs)["controller"]),
                                         baseBytes: mine.bytes, differences: differences, isDifferentSetup: false,
                                         mergeBlocker: nil)
        app.divergence = divergence
        return (app, divergence)
    }

    @Test("merging, keeping mine and taking theirs never write over a setup saved while the sheet was open")
    func settlingIsACompareAndSwap() throws {
        for settle in [{ (app: AppModel, divergence: SetupDivergence) in app.resolveDivergence(divergence, choices: [:]) },
                       { app, divergence in app.keepMine(divergence) },
                       { app, divergence in app.takeTheirs(divergence) }] {
            let home = try Self.home()
            defer { try? FileManager.default.removeItem(at: home) }
            let (app, divergence) = try Self.diverged(in: home)
            // An edit lands while the sheet is up.
            #expect(app.editSetup("renamed") { root in
                try ControllerEditor.upsertMachine(["id": "tower", "name": "Big Tower"], in: &root)
            } == nil)
            let edited = try #require(app.config.document?.hash)

            #expect(settle(app, divergence) == "The setup here changed while this was open; comparing again.")
            #expect(app.divergence == nil)
            #expect(app.config.document?.hash == edited)
            #expect(app.config.config?.machine(id: "tower")?.name == "Big Tower")
        }
    }

    @Test("Merge and Keep mine give the reason the sheet showed for waiting, and write nothing")
    func blockedMergeSaysWhy() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let (app, diverged) = try Self.diverged(in: home)
        var divergence = diverged
        divergence.mergeBlocker = "The common ancestor kept on this device breaks a rule, so it cannot be used: a helper is missing."
        let applied = try #require(app.config.document?.hash)

        #expect(app.resolveDivergence(divergence, choices: [:]) == divergence.mergeBlocker)
        #expect(app.keepMine(divergence) == divergence.mergeBlocker)
        #expect(app.config.document?.hash == applied)
    }

    @Test("a merge over the document the sheet was built from is written, descending from both")
    func mergeLands() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let (app, divergence) = try Self.diverged(in: home)
        #expect(app.resolveDivergence(divergence, choices: [:]) == nil)
        #expect(app.divergence == nil)
        let merged = try #require(app.config.document)
        #expect(merged.identity.ancestors.prefix(2) == [divergence.mine.hash, divergence.theirsHash])
        #expect(app.config.config?.machine(id: "pi")?.name == "Kitchen Pi")
    }

    @Test("an edit to a machine's entry updates its model rather than replacing it")
    func editKeepsMachineModels() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(Self.twoMachines, in: home)
        let pi = try #require(app.machine(id: "pi"))
        let tower = try #require(app.machine(id: "tower"))

        let problem = app.editSetup("renamed the pi") { root in
            var machines = try #require(root["machines"] as? [[String: Any]])
            machines[0]["name"] = "Pi in the hall"
            machines[0]["endpoints"] = [["id": "lan", "kind": "lan", "host": "192.0.2.6", "user": "me"]]
            root["machines"] = machines
        }
        #expect(problem == nil)

        // The same objects: an action running in either one is still the one the window shows.
        #expect(app.machine(id: "pi") === pi)
        #expect(app.machine(id: "tower") === tower)
        #expect(pi.machine.name == "Pi in the hall")
        #expect(pi.machine.endpoints.first?.host == "192.0.2.6")
    }

    @Test("every machine model writes into the app's one remembered store")
    func machinesShareRememberedSettings() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(Self.twoMachines, in: home)

        #expect(app.machines.count == 2)
        #expect(app.machines.allSatisfy { $0.remembered === app.remembered })
    }

    @Test("a failure on this device reaches the panel's notice, as one on a machine does")
    func localFailureReachesThePanel() async throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let bindings = BindingsStore(url: home.appending(path: "bindings.json"))
        bindings.update {
            $0.deviceName = "Desk"
            $0.selfBinding = .init(machine: "tower", system: "mac")
            // Not there, so the agent is never spawned and the cycle fails before anything is sent.
            $0.localAgent = .init(argv: ["/nowhere/at/all/node", "/agent.mjs"])
        }
        let app = try Self.app("""
        { "version": 1, "machines": [ { "id": "tower", "name": "Tower",
          "systems": [ { "id": "mac", "platform": "mac", "agent": ["node", "/agent.mjs"] } ] } ] }
        """, in: home, bindings: bindings)
        let mac = try #require(app.mac)
        #expect(app.machines.isEmpty)
        #expect(app.lastActionOutcome == nil)

        mac.runCycle()
        let deadline = Date().addingTimeInterval(5)
        while mac.isWorking, Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }

        let outcome = try #require(app.lastActionOutcome)
        #expect(outcome == mac.lastActionOutcome)
        #expect(outcome.isError)
        #expect(outcome.text.contains("did not happen"))
    }

    @Test("picking a site over a private settings file this device refuses says why nothing changed")
    func refusedSiteIsSaid() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let url = home.appending(path: "bindings.json")
        let refused = Data(#"{"self":{"machine":"tower"},"localAgent":{"argv":["node","/a.mjs"],"shell":"cmd"}}"#.utf8)
        try refused.write(to: url)
        let app = try Self.app(Self.twoMachines, in: home, bindings: BindingsStore(url: url))

        // The file is never written over, and the picker used to snap back without a word.
        let problem = app.confirmSite("home")
        #expect(problem?.contains("Nothing was saved") == true)
        #expect(app.bindings.bindings.currentSite == nil)
        #expect(try Data(contentsOf: url) == refused)
    }

    @Test("opening the setup or the private settings goes through the one opener, which a test replaces")
    func editorOpenerIsInjected() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(Self.twoMachines, in: home)
        // Nothing reaches /usr/bin/open: the paths are only written down.
        var opened: [String] = []
        app.openInEditor = { opened.append($0) }

        app.openConfigInEditor()
        app.openBindingsInEditor()
        #expect(opened == [app.config.path, app.bindings.path])
    }

    @Test("going back and discarding the kept build are asked first, and the answer does it")
    func appActionsAreConfirmed() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(Self.twoMachines, in: home)
        let updates = app.appUpdates
        updates.bundleURL = home.appending(path: "Legion Control.app")
        updates.supportDirectory = home.appending(path: "support")
        updates.isIsolatedRun = false
        let previous = home.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "1.3.4")
        var handed: [URL] = []
        updates.performSwap = { staged, _, _, _, _ in handed.append(staged.bundle) }
        updates.quit = { }

        app.requestRollback()
        let rollBack = try #require(app.dialog)
        #expect(rollBack.title.contains("1.3.4"))
        #expect(handed.isEmpty)
        app.confirm(rollBack)
        #expect(handed == [previous])

        // A fresh model for the second question, as after a relaunch: the first one is waiting to quit.
        let next = try Self.app(Self.twoMachines, in: home)
        next.appUpdates.bundleURL = updates.bundleURL
        next.appUpdates.supportDirectory = updates.supportDirectory
        next.appUpdates.isIsolatedRun = false
        next.requestDiscardPreviousBuild()
        let discard = try #require(next.dialog)
        #expect(FileManager.default.fileExists(atPath: previous.path))
        next.confirm(discard)
        #expect(!FileManager.default.fileExists(atPath: previous.path))
        #expect(next.dialog == nil)

        // With nothing kept there is nothing to ask about.
        next.requestRollback()
        next.requestDiscardPreviousBuild()
        #expect(next.dialog == nil)
    }

    @Test("the app's own page has a stable sidebar identifier, like every other row")
    func appSidebarItem() {
        #expect(SidebarItem.app.id == "app")
        #expect(SidebarItem(id: "app") == .app)
    }

    @Test("every sidebar row carries its state, which VoiceOver reads as the row's value")
    func sidebarRowsSayTheirState() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let bindings = BindingsStore(url: home.appending(path: "bindings.json"))
        bindings.update {
            $0.deviceName = "Desk"
            $0.selfBinding = .init(machine: "tower", system: "mac")
            $0.localAgent = .init(argv: ["/nowhere/at/all/node", "/agent.mjs"])
        }
        let app = try Self.app("""
        { "version": 1, "machines": [
          { "id": "pi", "name": "Pi",
            "endpoints": [ { "id": "lan", "kind": "lan", "host": "192.0.2.5", "user": "me" } ],
            "systems": [ { "id": "linux", "platform": "linux", "agent": ["node", "/a.mjs"] } ] },
          { "id": "tower", "name": "Tower",
            "systems": [ { "id": "mac", "platform": "mac", "agent": ["node", "/agent.mjs"] } ] } ] }
        """, in: home, bindings: bindings)
        let pi = try #require(app.machine(id: "pi"))
        let mac = try #require(app.mac)

        // The sidebar used to be handed these and draw only the name, so a sidebar read aloud was a
        // list of names and nothing about any of them.
        #expect(app.sidebarItems == [.machine("pi"), .local, .setup, .app])
        #expect(app.sidebarItems.compactMap { app.sidebarRow(for: $0) } == [
            SidebarRow(symbol: pi.symbolName, title: "Pi", summary: pi.sidebarSummary),
            SidebarRow(symbol: "laptopcomputer", title: mac.name, summary: mac.sidebarSummary),
            SidebarRow(symbol: "list.bullet.rectangle", title: "Setup", summary: "no identity yet"),
            SidebarRow(symbol: "app", title: "Legion Control", summary: app.appUpdates.summary),
        ])
        #expect(app.sidebarItems.allSatisfy { app.sidebarRow(for: $0)?.summary.isEmpty == false })

        // A row whose machine or service has gone draws nothing rather than a name with no state.
        #expect(app.sidebarRow(for: .machine("gone")) == nil)
        #expect(app.sidebarRow(for: .service(machine: "pi", service: "gone")) == nil)
    }

    @Test("the setup row says what waits to be settled before which revision this device is on")
    func setupRowState() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let (app, _) = try Self.diverged(in: home)
        #expect(app.sidebarRow(for: .setup)?.summary == "needs a decision")
        app.divergence = nil
        #expect(app.sidebarRow(for: .setup)?.summary == "revision 1")

        // A record this device cannot read approves nothing, so every command of a setup that came
        // with an identity waits for a person. (One with none, written here as the first setup, is
        // this device's own edit, which approves what it holds and sets the unreadable record aside.)
        let other = try Self.home()
        defer { try? FileManager.default.removeItem(at: other) }
        let state = other.appending(path: "state")
        try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
        try Data("not a record".utf8).write(to: state.appending(path: "approved-launch.json"))
        let identified = Self.twoMachines.replacingOccurrences(
            of: #""version": 1,"#,
            with: #""version": 1, "controller": { "id": "setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846", "revision": 1, "lineage": [] },"#)
        let waiting = try Self.app(identified, in: other)
        #expect(!waiting.launchReview.isEmpty)
        #expect(waiting.sidebarRow(for: .setup)?.summary == "commands waiting for approval")
    }
}
