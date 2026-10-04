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
}
