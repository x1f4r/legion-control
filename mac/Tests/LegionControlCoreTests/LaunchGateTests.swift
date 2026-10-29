import Foundation
import Testing
@testable import LegionControlCore

/// The executable setup boundary (section 9.9 of the contract): what this Mac runs is only ever
/// what it approved itself, however a document arrived.
///
/// The pure rules are held to the reference by the `launch` vectors in ContractVectorTests. These
/// drive the app around them: a real `AppModel` on a temporary home, documents written the way a
/// peer writes them, and a transport that records every ssh argv instead of running one, so the
/// command lines that would have left this Mac can be read back word for word.
@MainActor
struct LaunchGateTests {

    // MARK: - Harness

    /// Every ssh this app would have run, and what it would have sent on stdin. Answers every
    /// status as a contract 3 agent holding no setup, so the first read also publishes.
    final class Recorder: @unchecked Sendable {
        private let lock = NSLock()
        private var calls: [(arguments: [String], input: Data?)] = []
        let logDirectory = FakeSSH.logDirectory()

        static let status = """
        { "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux", "name": "Linux" },
          "agent": { "version": "3.1.0", "contract": 3 }, "services": [], "controller": {} }
        """

        func run(_ arguments: [String], input: Data?) -> CommandResult {
            lock.withLock { calls.append((arguments, input)) }
            let json = FakeSSH.line(arguments).contains("config set")
                ? #"{ "ok": true, "contract": 3, "action": "stored" }"#
                : Self.status
            return FakeSSH.result(out: json)
        }

        var lines: [String] { lock.withLock { calls.map { FakeSSH.line($0.arguments) } } }
        var destinations: [String] { lock.withLock { calls.compactMap { FakeSSH.destination($0.arguments) } } }
        /// The documents published with `config set`.
        var pushed: [Data] {
            lock.withLock { calls.filter { FakeSSH.line($0.arguments).contains("config set") }.compactMap(\.input) }
        }
    }

    static func home() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-launch-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    /// The app on `home`, with `document` written as its setup first when there is one.
    static func app(_ document: Data?, in home: URL, recorder: Recorder = Recorder(),
                    bindings: BindingsStore? = nil) throws -> AppModel {
        let file = home.appending(path: "config.json")
        if let document { try document.write(to: file) }
        return AppModel(
            config: ConfigStore(url: file),
            bindings: bindings ?? BindingsStore(url: home.appending(path: "absent-bindings.json")),
            operations: OperationStore(url: home.appending(path: "operations.json")),
            appUpdates: AppUpdateModel(preferences: FilePreferences(url: home.appending(path: "preferences.json")),
                                       installedVersion: "1.4.0"),
            remembered: .inMemory(),
            remoteAgent: { machine in
                var agent = RemoteAgent(machine: machine)
                agent.logDirectory = recorder.logDirectory
                agent.runner = { _, arguments, _, input in recorder.run(arguments, input: input) }
                return agent
            }
        )
    }

    static let setupId = "setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846"
    static let launcher = ["/usr/bin/node", "/home/me/.legion-control/bin/launcher.mjs"]
    static let windowsLauncher = ["C:\\Program Files\\nodejs\\node.exe", "D:\\lc\\bin\\launcher.mjs"]
    static let piLauncher = ["node", "/home/me/.legion-control/bin/launcher.mjs"]

    /// The setup this Mac already applies: a dual-boot tower with a custom Windows launcher, and a
    /// Pi reached with a restricted key.
    static func held(_ change: (inout [String: Any]) -> Void = { _ in }) throws -> Data {
        var root: [String: Any] = [
            "version": 1,
            "controller": ["id": setupId, "name": "Home setup", "revision": 7, "source": "desktop",
                           "device": "Desk tower", "lineage": [String]()],
            "machines": [
                ["id": "tower", "name": "Tower",
                 "endpoints": [["id": "lan", "kind": "lan", "host": "192.0.2.20", "user": "me"]],
                 "systems": [
                    ["id": "linux", "name": "Linux", "platform": "linux", "shell": "posix", "agent": launcher],
                    ["id": "windows", "name": "Windows 11", "platform": "windows", "shell": "cmd", "agent": windowsLauncher],
                 ]],
                ["id": "pi", "name": "Beacon",
                 "endpoints": [["id": "lan", "kind": "lan", "host": "192.0.2.40", "user": "me"]],
                 "systems": [["id": "linux", "name": "Raspberry Pi OS", "platform": "linux", "restricted": true,
                              "agent": piLauncher]]],
            ],
        ]
        change(&root)
        return try JSONText.canonicalDocument(root)
    }

    /// A child of `parent` as another device writes it: the next revision, with the parent's hash
    /// in its lineage, so this Mac takes it as a fast-forward.
    static func child(of parent: Data, _ change: (inout [String: Any]) -> Void) throws -> Data {
        let identity = try JSONDecoder().decode(ControllerConfig.self, from: parent).identity
        return try ControllerEditor.apply(to: parent, parentHash: Canonical.hash(parent), parentIdentity: identity,
                                          deviceName: "Desk tower") { change(&$0) }.bytes
    }

    /// Change one system the way a peer may, with nothing checked.
    static func system(_ machine: String, _ system: String, in root: inout [String: Any],
                       _ change: (inout [String: Any]) -> Void) {
        self.machine(machine, in: &root) { entry in
            var systems = entry["systems"] as? [[String: Any]] ?? []
            guard let index = systems.firstIndex(where: { $0["id"] as? String == system }) else { return }
            change(&systems[index])
            entry["systems"] = systems
        }
    }

    static func machine(_ id: String, in root: inout [String: Any], _ change: (inout [String: Any]) -> Void) {
        var machines = root["machines"] as? [[String: Any]] ?? []
        guard let index = machines.firstIndex(where: { $0["id"] as? String == id }) else { return }
        change(&machines[index])
        root["machines"] = machines
    }

    static func eventually(timeout: TimeInterval = 3, _ condition: () -> Bool) async {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition(), Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }
    }

    static func file(_ url: URL) -> Data? { try? Data(contentsOf: url) }

    // MARK: - Upgrading

    @Test("the first start records the setup it already runs, beside the applied state, and asks nothing")
    func upgradeSeed() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let document = try Self.held { root in
            Self.machine("tower", in: &root) { $0["ssh"] = ["host": "desk-tower.local", "identityFile": "~/.ssh/id_tower"] }
            root["appUpdates"] = ["githubRepo": "me/legion-fork"]
            root["local"] = ["enabled": true, "name": "This Mac", "agent": "/Users/me/lc/agent/src/index.mjs"]
        }
        let app = try Self.app(document, in: home)

        let url = home.appending(path: "state/approved-launch.json")
        #expect(app.config.approvalsURL == url)
        let record = try #require(Self.file(url))
        #expect(app.approvals.record.seededFrom == app.config.hash)
        #expect(app.launchReview.isEmpty)
        #expect(!app.isReviewingLaunch)

        // Everything it drove before it drives now, exactly as the document says.
        for machine in app.machines {
            let written = try #require(app.config.config?.machine(id: machine.id))
            #expect(machine.machine.systems.map(\.agent) == written.systems.map(\.agent))
            #expect(machine.machine.systems.allSatisfy { $0.launchState == .approved })
        }
        #expect(app.effectiveSetup?.machine(id: "tower")?.ssh?.identityFile == "~/.ssh/id_tower")
        #expect(app.appUpdates.repository == "me/legion-fork")
        #expect(app.mac?.config.agent == "/Users/me/lc/agent/src/index.mjs")

        // A second start finds the record and leaves it alone.
        let again = try Self.app(nil, in: home)
        #expect(Self.file(url) == record)
        #expect(again.launchReview.isEmpty)
    }

    @Test("an unreadable record approves nothing, is never seeded over, and is moved aside by the next approval")
    func unreadableRecord() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let state = home.appending(path: "state")
        try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
        let url = state.appending(path: "approved-launch.json")
        try Data("not a record".utf8).write(to: url)

        let app = try Self.app(try Self.held(), in: home)
        #expect(app.approvals.isUnreadable)
        #expect(app.approvals.problem?.contains("could not be read") == true)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[linux]", "machines[tower].systems[windows]",
                                                 "machines[pi].systems[linux]"])
        #expect(app.machines.flatMap(\.machine.systems).allSatisfy { $0.launchState == .notDriven && $0.agent.isEmpty })
        #expect(Self.file(url) == Data("not a record".utf8))

        app.approveLaunch([try #require(app.launchReview.first)])
        let kept = try FileManager.default.contentsOfDirectory(atPath: state.path(percentEncoded: false))
            .filter { $0.hasPrefix("approved-launch.json.corrupt-") }
        #expect(kept.count == 1)
        #expect(kept.first.flatMap { Self.file(state.appending(path: $0)) } == Data("not a record".utf8))
        #expect(!app.approvals.isUnreadable)
        #expect(app.approvals.problem == nil)
        #expect(app.machine(id: "tower")?.machine.system(id: "linux")?.agent == Self.launcher)
        #expect(app.launchReview.count == 2)
    }

    @Test("deleting the record while the app runs records the document applied now")
    func deletedRecordIsSeededAgain() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let store = LaunchApprovalStore(url: home.appending(path: "approved-launch.json"))
        let held = try Self.held()
        store.seedIfMissing(from: held)
        #expect(!store.reloadIfChanged(seedingFrom: held))

        let next = try Self.child(of: held) { Self.system("tower", "linux", in: &$0) { $0["agent"] = ["/opt/node", "/opt/lc/bin/launcher.mjs"] } }
        try FileManager.default.removeItem(at: store.url)
        #expect(store.reloadIfChanged(seedingFrom: next))
        #expect(store.record.seededFrom == (try Canonical.hash(next)))
        #expect(LaunchValues.review(record: store.record, projection: LaunchValues.projection(of: next)).isEmpty)
    }

    // MARK: - What a peer may and may not change

    @Test("a rename and a new route from a peer apply on their own, and the new route is dialled")
    func displayAndRoutingApply() async throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let recorder = Recorder()
        let held = try Self.held()
        let app = try Self.app(held, in: home, recorder: recorder)

        let next = try Self.child(of: held) { root in
            Self.machine("tower", in: &root) { tower in
                tower["name"] = "Desk tower"
                tower["alwaysOn"] = true
                tower["endpoints"] = [["id": "lan", "kind": "lan", "host": "192.0.2.21", "user": "me", "port": 2222],
                                      ["id": "remote", "kind": "remote", "host": "tower.example.net", "user": "me"]]
            }
        }
        #expect(app.config.adopt(next, describedAs: "adopted from Beacon") == nil)
        #expect(app.launchReview.isEmpty)
        #expect(!app.isReviewingLaunch)

        let tower = try #require(app.machine(id: "tower"))
        #expect(tower.machine.name == "Desk tower")
        #expect(app.effectiveSetup?.machine(id: "tower")?.systems.map(\.agent)
                == app.config.config?.machine(id: "tower")?.systems.map(\.agent))
        await tower.refresh(userInitiated: false)
        #expect(recorder.destinations.first == "me@tower.example.net")
        #expect(recorder.lines.first?.hasPrefix(try CommandGrammar.posix.serialize(Self.launcher) + " ") == true)
    }

    /// A peer's descendant changes one command. It is adopted and published on, and nothing that
    /// leaves this Mac carries it.
    private func runsOnlyTheApproved(machine machineId: String, grammar: CommandGrammar, approved: [String],
                                     forbidden: String, change: (inout [String: Any]) -> Void) async throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let recorder = Recorder()
        let held = try Self.held()
        let app = try Self.app(held, in: home, recorder: recorder)

        let next = try Self.child(of: held) { Self.system(machineId, "linux", in: &$0, change) }
        #expect(app.config.adopt(next, describedAs: "adopted from Tower") == nil)
        #expect(app.config.hash == (try Canonical.hash(next)))
        #expect(app.launchReview.map(\.path) == ["machines[\(machineId)].systems[linux]"])
        #expect(app.isReviewingLaunch)

        let model = try #require(app.machine(id: machineId))
        let system = try #require(model.machine.system(id: "linux"))
        #expect(system.agent == approved)
        #expect(system.grammar == .success(grammar))
        #expect(system.launchState == .approvedWithChangeWaiting)

        await model.refresh(userInitiated: false)
        await Self.eventually { !recorder.pushed.isEmpty }
        let prefix = try grammar.serialize(approved) + " "
        #expect(recorder.lines.count >= 2)
        for line in recorder.lines {
            #expect(line.hasPrefix(prefix), "\(line)")
            #expect(!line.contains(forbidden), "\(line)")
        }
        // Replicating is not running: a machine that is behind still gets the whole document.
        #expect(recorder.pushed == [try Canonical.bytes(next)])
    }

    @Test("a peer's new command is adopted and published, and every line sent starts with the approved one")
    func maliciousDescendant() async throws {
        try await runsOnlyTheApproved(machine: "tower", grammar: .posix, approved: Self.launcher, forbidden: "touch") {
            $0["agent"] = ["sh", "-c", "touch /tmp/x"]
        }
    }

    @Test("a verb appended to a restricted key's launcher is never sent, and the dispatcher's grammar stays")
    func verbInjection() async throws {
        try await runsOnlyTheApproved(machine: "pi", grammar: .dispatch, approved: Self.piLauncher, forbidden: "self-update") {
            $0["agent"] = Self.piLauncher + ["self-update", "--rollback"]
        }
    }

    @Test("a restricted key stays in the dispatcher's grammar whatever a peer says about it")
    func restrictedReceivingKey() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held()
        let app = try Self.app(held, in: home)

        let next = try Self.child(of: held) { root in
            Self.system("pi", "linux", in: &root) { $0["restricted"] = false; $0["shell"] = "powershell" }
        }
        #expect(app.config.adopt(next, describedAs: "adopted from Tower") == nil)
        #expect(app.launchReview.map(\.path) == ["machines[pi].systems[linux]"])
        let system = try #require(app.machine(id: "pi")?.machine.system(id: "linux"))
        #expect(system.grammar == .success(.dispatch))
        #expect(system.isRestricted)
        #expect(system.shell == "powershell")
        #expect(try RemoteCommand(system: system, arguments: ["status"]).commandLine()
                == "node /home/me/.legion-control/bin/launcher.mjs status")
    }

    @Test("an approved custom launcher that does not change asks nothing across revisions")
    func unchangedCustomLauncher() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        var document = try Self.held()
        let app = try Self.app(document, in: home)
        for name in ["Tower", "Desk tower", "Tower upstairs"] {
            document = try Self.child(of: document) { root in Self.machine("tower", in: &root) { $0["name"] = name } }
            #expect(app.config.adopt(document, describedAs: "adopted") == nil)
            #expect(app.launchReview.isEmpty)
            let windows = try #require(app.machine(id: "tower")?.machine.system(id: "windows"))
            #expect(windows.agent == Self.windowsLauncher)
            #expect(windows.grammar == .success(.cmd))
            #expect(windows.launchState == .approved)
        }
    }

    @Test("a system the setup names no command for is a warning and is never tried, not a refused document")
    func systemWithoutCommand() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let document = try Self.held { root in
            Self.system("tower", "windows", in: &root) { $0["agent"] = [String]() }
            Self.system("tower", "linux", in: &root) { $0.removeValue(forKey: "agent") }
        }
        let app = try Self.app(document, in: home)
        #expect(app.config.problem == nil)
        #expect(app.setupWarnings.filter { $0.contains("has no agent command, so it is not driven") }.count == 2)
        let tower = try #require(app.effectiveSetup?.machine(id: "tower"))
        #expect(tower.systems.allSatisfy { $0.launchState == .notDriven })
        #expect(RemoteAgent.attempts(machine: tower, preferredSystem: nil, preferredRoute: nil).isEmpty)
        #expect(app.launchReview.isEmpty)
    }

    @Test("an update repository or local agent a peer names waits, and the approved one stays in use")
    func repositoryAndLocalAgent() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held { root in
            root["appUpdates"] = ["githubRepo": "me/legion-fork"]
            root["local"] = ["enabled": true, "name": "This Mac", "agent": "/Users/me/lc/agent/src/index.mjs"]
        }
        let app = try Self.app(held, in: home)
        let local = try #require(app.mac)

        let next = try Self.child(of: held) { root in
            root["appUpdates"] = ["githubRepo": "someone/elsewhere"]
            root["local"] = ["enabled": true, "name": "This Mac", "agent": "/Users/me/elsewhere/index.mjs"]
        }
        #expect(app.config.adopt(next, describedAs: "adopted") == nil)
        #expect(app.launchReview.map(\.path) == ["appUpdates.githubRepo", "local"])
        #expect(app.appUpdates.repository == "me/legion-fork")
        #expect(app.mac === local)
        #expect(app.mac?.config.agent == "/Users/me/lc/agent/src/index.mjs")
    }

    @Test("with nothing approved, a peer's repository gives way to the built-in one and its local agent to none")
    func nothingToFallBackTo() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held()
        let app = try Self.app(held, in: home)
        let next = try Self.child(of: held) { root in
            root["appUpdates"] = ["githubRepo": "someone/elsewhere"]
            root["local"] = ["enabled": true, "name": "This Mac", "agent": "/Users/me/elsewhere/index.mjs"]
        }
        #expect(app.config.adopt(next, describedAs: "adopted") == nil)
        #expect(app.appUpdates.repository == ControllerConfig.AppUpdatesConfig.defaultRepo)
        #expect(app.mac == nil)
        #expect(app.launchReview.map(\.kind) == [.new, .new])
    }

    // MARK: - Enrolment

    @Test("enrolment shows every launch value, cancel applies nothing, and confirm approves all of it")
    func firstEnrolment() async throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let recorder = Recorder()
        let app = try Self.app(nil, in: home, recorder: recorder)
        let file = home.appending(path: "config.json")
        #expect(app.approvals.record.seededFrom == "none")
        #expect(Self.file(app.config.approvalsURL) != nil)

        let document = try Self.held { root in
            Self.machine("tower", in: &root) { $0["ssh"] = ["host": "desk-tower.local", "identityFile": "~/.ssh/id_tower"] }
            root["appUpdates"] = ["githubRepo": "me/legion-fork"]
        }
        let identity = try JSONDecoder().decode(ControllerConfig.self, from: document).identity
        #expect(app.takeSetup(document, identity: identity, from: "Tower") == .awaitingConfirmation)
        let enrolment = try #require(app.enrolment)
        #expect(enrolment.projection.machines.map(\.id) == ["tower", "pi"])
        #expect(enrolment.projection.machines.flatMap(\.systems).compactMap(\.tuple?.agent)
                == [Self.launcher, Self.windowsLauncher, Self.piLauncher])
        #expect(enrolment.projection.machines.first?.identityFile == "~/.ssh/id_tower")
        #expect(enrolment.projection.repo == "me/legion-fork")
        #expect(Self.file(file) == nil)

        app.cancelEnrolment()
        #expect(app.enrolment == nil)
        #expect(Self.file(file) == nil)
        #expect(app.config.config == nil)

        _ = app.takeSetup(document, identity: identity, from: "Tower")
        #expect(app.confirmEnrolment() == nil)
        #expect(app.enrolment == nil)
        #expect(app.config.hash == (try Canonical.hash(document)))
        #expect(app.launchReview.isEmpty)
        #expect(app.machines.flatMap(\.machine.systems).allSatisfy { $0.launchState == .approved })

        // What a peer changes afterwards waits, and a machine it adds is not driven at all.
        let next = try Self.child(of: document) { root in
            Self.system("tower", "linux", in: &root) { $0["agent"] = ["/usr/bin/node", "/srv/lc/bin/launcher.mjs"] }
            var machines = root["machines"] as? [[String: Any]] ?? []
            machines.append(["id": "nas", "name": "NAS",
                             "endpoints": [["id": "lan", "kind": "lan", "host": "192.0.2.60", "user": "me"]],
                             "systems": [["id": "linux", "platform": "linux", "agent": ["/usr/bin/node", "/srv/nas/bin/launcher.mjs"]]]])
            root["machines"] = machines
        }
        #expect(app.config.adopt(next, describedAs: "adopted from Tower") == nil)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[linux]", "machines[nas].systems[linux]"])
        let nas = try #require(app.machine(id: "nas"))
        #expect(nas.machine.system(id: "linux")?.launchState == .notDriven)
        await nas.refresh(userInitiated: false)
        #expect(!recorder.destinations.contains { $0.contains("192.0.2.60") })
    }

    @Test("the enrolment summary marks each value confirming cannot approve, and exactly those come back for review")
    func enrolmentMarksWhatItCannotApprove() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(nil, in: home)
        let hidden = ["/usr/bin/node", "/home/me/.legion-control/bin/\u{202E}sjm.rehcnual"]
        let document = try Self.held { root in
            Self.system("tower", "linux", in: &root) { $0["agent"] = hidden }
            // A restricted key with three words, which the dispatcher refuses.
            Self.system("pi", "linux", in: &root) { $0["agent"] = Self.piLauncher + ["--verbose"] }
        }
        let identity = try JSONDecoder().decode(ControllerConfig.self, from: document).identity
        #expect(app.takeSetup(document, identity: identity, from: "Tower") == .awaitingConfirmation)
        let marked = try #require(app.enrolment).unapprovable
        #expect(marked.map(\.path) == ["machines[tower].systems[linux]", "machines[pi].systems[linux]"])
        #expect(LaunchText.whyNotEnrolled(marked[0]).hasPrefix("It holds a control, invisible or direction-changing character"))
        #expect(LaunchText.whyNotEnrolled(marked[1]).hasPrefix("This command is never sent anywhere."))
        #expect(marked.allSatisfy { LaunchText.whyNotEnrolled($0).hasSuffix("Confirming does not approve it, so this device does not run it.") })

        #expect(app.confirmEnrolment() == nil)
        #expect(app.launchReview.map(\.path) == marked.map(\.path))
        #expect(app.launchReview.allSatisfy { !$0.approvable && $0.approved == nil })
        let tower = try #require(app.machine(id: "tower")?.machine)
        #expect(tower.system(id: "windows")?.launchState == .approved)
        #expect(tower.system(id: "linux")?.launchState == .notDriven)

        // The editor reads the document as written, where that system is still marked restricted
        // although no grammar can be worked out for it.
        let pi = try #require(app.config.config?.machine(id: "pi")?.system(id: "linux"))
        #expect(pi.restricted == true)
        #expect(!pi.isRestricted)
        #expect(pi.grammar == .failure(CommandLineRefusal("restricted-agent-shape")))
    }

    @Test("a setup taken from an agent with none applied waits for confirmation; with one applied it approves nothing")
    func takingAnAgentsSetup() async throws {
        let runner = MacModelTests.FakeRunner()
        runner.answer("status", json: MacModelTests.status)
        let document = try Self.held()
        let reply: [String: Any] = ["ok": true, "contract": 3, "raw": String(decoding: document, as: UTF8.self),
                                    "hash": try Canonical.hash(document)]
        runner.answer("config", json: String(decoding: try JSONSerialization.data(withJSONObject: reply), as: UTF8.self))
        let (model, _, directory) = try MacModelTests.model(runner)
        defer { try? FileManager.default.removeItem(at: directory) }
        var taken: (bytes: Data, holder: String)?
        model.takeSetup = { bytes, _, holder in
            taken = (bytes, holder)
            return .awaitingConfirmation
        }

        await model.refresh()
        model.adoptRemoteSetup()
        await Self.eventually { model.note?.contains("confirm") == true }
        #expect(taken?.bytes == document)
        #expect(taken?.holder == "the local agent")
        #expect(model.note == "The setup from the local agent is waiting for you to confirm what it runs. Nothing has been applied yet.")
        #expect(!model.noteIsError)

        // With a setup applied the same request is an adoption of a different one.
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(try Self.held(), in: home)
        let record = Self.file(app.config.approvalsURL)
        let other = try Self.held { root in
            root["controller"] = ["id": "setup-0b1c2d3e-0000-4000-8000-000000000001", "name": "Other", "revision": 1,
                                  "lineage": [String]()]
            Self.system("tower", "linux", in: &root) { $0["agent"] = ["/usr/bin/node", "/srv/other/bin/launcher.mjs"] }
        }
        let identity = try JSONDecoder().decode(ControllerConfig.self, from: other).identity
        #expect(app.takeSetup(other, identity: identity, from: "Tower") == .adopted)
        #expect(app.enrolment == nil)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[linux]"])
        #expect(Self.file(app.config.approvalsURL) == record)
    }

    // MARK: - What approves

    @Test("an authorised launcher migration is recorded before the file is written and taken by a peer without asking")
    func launcherMigration() throws {
        let entry = ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"]
        let held = try Self.held { Self.system("tower", "linux", in: &$0) { $0["agent"] = entry } }

        // Device A presses "Update command".
        let homeA = try Self.home()
        defer { try? FileManager.default.removeItem(at: homeA) }
        let a = try Self.app(held, in: homeA)
        let fileA = homeA.appending(path: "config.json")
        var fileWhenRecorded: Data?
        let record = a.config.onLocalEdit
        a.config.onLocalEdit = { before, after in
            fileWhenRecorded = Self.file(fileA)
            record?(before, after)
        }
        a.machine(id: "tower")?.migrateAgentCommand?("linux", Self.launcher)
        #expect(fileWhenRecorded == held)
        #expect(a.approvals.record.value(at: .system(machine: "tower", system: "linux"))
                == .tuple(LaunchTuple(agent: Self.launcher, grammar: "posix")))
        #expect(a.launchReview.isEmpty)
        let migrated = try #require(Self.file(fileA))
        #expect(migrated != held)

        // Peer B adopts it, asks nothing, and records the new launcher where it now runs.
        let homeB = try Self.home()
        defer { try? FileManager.default.removeItem(at: homeB) }
        let b = try Self.app(held, in: homeB)
        #expect(b.config.adopt(migrated, describedAs: "adopted from A") == nil)
        #expect(b.launchReview.isEmpty)
        #expect(b.machine(id: "tower")?.machine.system(id: "linux")?.agent == Self.launcher)
        #expect(b.approvals.record.value(at: .system(machine: "tower", system: "linux"))
                == .tuple(LaunchTuple(agent: Self.launcher, grammar: "posix")))

        // The same move with a new interpreter, or into another grammar, is reviewed.
        for change in [{ (system: inout [String: Any]) in system["agent"] = ["/opt/node", Self.launcher[1]] },
                       { (system: inout [String: Any]) in system["agent"] = Self.launcher; system["shell"] = "powershell" }] {
            let home = try Self.home()
            defer { try? FileManager.default.removeItem(at: home) }
            let peer = try Self.app(held, in: home)
            let next = try Self.child(of: held) { Self.system("tower", "linux", in: &$0, change) }
            #expect(peer.config.adopt(next, describedAs: "adopted") == nil)
            #expect(peer.launchReview.map(\.path) == ["machines[tower].systems[linux]"])
            #expect(peer.machine(id: "tower")?.machine.system(id: "linux")?.agent == entry)
        }
    }

    @Test("a Mac bound to itself migrates its own command in its private settings, approving nothing")
    func selfBoundMigration() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let bindings = BindingsStore(url: home.appending(path: "bindings.json"))
        bindings.update {
            $0.deviceName = "Desk"
            $0.selfBinding = .init(machine: "tower", system: "linux")
            $0.localAgent = .init(argv: ["/usr/bin/node", "/Users/me/.legion-control/agent/src/index.mjs"])
        }
        let app = try Self.app(try Self.held(), in: home, bindings: bindings)
        let file = Self.file(home.appending(path: "config.json"))
        let record = Self.file(app.config.approvalsURL)
        let mac = try #require(app.mac)

        mac.migrateAgentCommand?(["/usr/bin/node", "/Users/me/.legion-control/bin/launcher.mjs"])
        #expect(bindings.bindings.localAgent?.argv == ["/usr/bin/node", "/Users/me/.legion-control/bin/launcher.mjs"])
        #expect(Self.file(home.appending(path: "config.json")) == file)
        #expect(Self.file(app.config.approvalsURL) == record)
    }

    @Test("an edit here approves only what it changed, and never a command that was waiting")
    func localEdits() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(try Self.held(), in: home)
        let spare = ["/usr/bin/node", "/opt/lc/bin/launcher.mjs"]

        #expect(app.editSetup("added a system") { root in
            try ControllerEditor.upsertSystem(["id": "spare", "name": "Spare", "platform": "linux", "agent": spare],
                                              onMachine: "tower", in: &root)
        } == nil)
        #expect(app.launchReview.isEmpty)
        #expect(app.approvals.record.value(at: .system(machine: "tower", system: "spare"))
                == .tuple(LaunchTuple(agent: spare, grammar: "posix")))

        let evil = ["sh", "-c", "touch /tmp/x"]
        let next = try Self.child(of: try #require(app.config.rawBytes)) { Self.system("tower", "linux", in: &$0) { $0["agent"] = evil } }
        #expect(app.config.adopt(next, describedAs: "adopted") == nil)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[linux]"])

        // One field of the waiting system changes here; the rest of it is still not this device's.
        #expect(app.editSetup("changed the shell") { Self.system("tower", "linux", in: &$0) { $0["shell"] = "powershell" } } == nil)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[linux]"])
        #expect(!app.approvals.record.approved.tuples.contains { $0.agent == evil })
        #expect(app.machine(id: "tower")?.machine.system(id: "linux")?.agent == Self.launcher)
    }

    @Test("a hand edit that keeps the controller block approves what it changed; a write with a new block approves nothing")
    func handEdits() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held()
        let app = try Self.app(held, in: home)
        let file = home.appending(path: "config.json")
        let edited = ["/usr/bin/node", "/opt/hand/bin/launcher.mjs"]

        // The same controller block, so this is an edit made on this device.
        var root = try #require(try JSONSerialization.jsonObject(with: held) as? [String: Any])
        Self.system("tower", "linux", in: &root) { $0["agent"] = edited }
        try JSONText.canonicalDocument(root).write(to: file)
        app.config.reloadIfChanged()
        #expect(app.config.config?.identity.revisionNumber == 8)
        #expect(app.launchReview.isEmpty)
        #expect(app.machine(id: "tower")?.machine.system(id: "linux")?.agent == edited)

        // A file written with a controller block of its own is a revision from somewhere else.
        let written = try Self.child(of: try #require(app.config.rawBytes)) {
            Self.system("tower", "linux", in: &$0) { $0["agent"] = ["/usr/bin/node", "/opt/written/bin/launcher.mjs"] }
        }
        try written.write(to: file)
        app.config.reloadIfChanged()
        #expect(app.config.hash == (try Canonical.hash(written)))
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[linux]"])
        #expect(app.machine(id: "tower")?.machine.system(id: "linux")?.agent == edited)
    }

    @Test("the example this app writes when asked runs without asking")
    func exampleIsApproved() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(nil, in: home)
        #expect(app.writeExampleConfig() == nil)
        #expect(app.launchReview.isEmpty)
        #expect(!app.machines.isEmpty)
        #expect(app.machines.flatMap(\.machine.systems).allSatisfy { $0.launchState == .approved && !$0.agent.isEmpty })
    }

    @Test("restoring, taking theirs, merging, keeping mine and adopting another setup approve nothing")
    func nothingElseApproves() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(try Self.held(), in: home)
        let recordURL = app.config.approvalsURL
        let record = Self.file(recordURL)
        func current() throws -> Data { try #require(app.config.rawBytes) }
        func divergence(theirs: Data) throws -> SetupDivergence {
            let mine = try #require(app.config.document)
            return SetupDivergence(machineId: "tower", machineName: "Tower", mine: mine, theirsBytes: theirs,
                                   theirsIdentity: try JSONDecoder().decode(ControllerConfig.self, from: theirs).identity,
                                   baseBytes: mine.bytes,
                                   differences: try SetupMerge.differences(mine: mine.bytes, theirs: theirs, base: mine.bytes),
                                   isDifferentSetup: false)
        }

        // Restore a kept revision that names a command nobody here approved.
        let kept = try Self.child(of: try current()) { Self.system("tower", "windows", in: &$0) { $0["agent"] = ["C:\\Other\\node.exe", "D:\\lc\\bin\\launcher.mjs"] } }
        let hash = try #require(app.config.keepRevision(try Canonical.bytes(kept)))
        let revision = try #require(app.config.storedRevisions().first { $0.hash == hash })
        #expect(app.config.restore(revision, deviceName: "Desk") == nil)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[windows]"])

        // Take theirs.
        let theirs = try Self.child(of: try current()) { Self.system("pi", "linux", in: &$0) { $0["agent"] = ["node", "/srv/pi/bin/launcher.mjs"] } }
        #expect(app.takeTheirs(try divergence(theirs: theirs)) == nil)
        #expect(app.launchReview.map(\.path) == ["machines[tower].systems[windows]", "machines[pi].systems[linux]"])

        // Merge, and keep mine.
        let merged = try Self.child(of: try current()) { Self.system("tower", "linux", in: &$0) { $0["agent"] = ["/usr/bin/node", "/srv/merge/bin/launcher.mjs"] } }
        #expect(app.resolveDivergence(try divergence(theirs: merged), choices: [:]) == nil)
        #expect(app.launchReview.count == 3)
        let mine = try Self.child(of: try current()) { Self.machine("tower", in: &$0) { $0["name"] = "Elsewhere" } }
        #expect(app.keepMine(try divergence(theirs: mine)) == nil)
        #expect(app.launchReview.count == 3)

        #expect(Self.file(recordURL) == record)
    }

    @Test("Revert puts the approved value back as an ordinary edit that replicates")
    func revert() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held()
        let app = try Self.app(held, in: home)

        let next = try Self.child(of: held) { Self.system("tower", "linux", in: &$0) { $0["agent"] = ["sh", "-c", "id"] } }
        #expect(app.config.adopt(next, describedAs: "adopted") == nil)
        let entry = try #require(app.launchReview.first)
        #expect(entry.approved == .tuple(LaunchTuple(agent: Self.launcher, grammar: "posix")))
        #expect(app.revertLaunch(entry) == nil)
        #expect(app.launchReview.isEmpty)
        #expect(app.config.config?.machine(id: "tower")?.system(id: "linux")?.agent == Self.launcher)
        #expect(app.config.config?.identity.revisionNumber == 9)
        #expect(app.config.config?.identity.lineage?.first == (try Canonical.hash(next)))

        // A grammar is put back by moving the fields only as far as it takes.
        let unrestricted = try Self.child(of: try #require(app.config.rawBytes)) { root in
            Self.system("pi", "linux", in: &root) { $0["restricted"] = false; $0["shell"] = "powershell" }
        }
        #expect(app.config.adopt(unrestricted, describedAs: "adopted") == nil)
        let restricted = try #require(app.launchReview.first)
        #expect(app.revertLaunch(restricted) == nil)
        #expect(app.launchReview.isEmpty)
        #expect(app.config.config?.machine(id: "pi")?.system(id: "linux")?.grammar == .success(.dispatch))
    }

    @Test("approving records exactly what the review showed, and a newer value waits on its own")
    func approvesWhatWasShown() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held()
        let app = try Self.app(held, in: home)
        let shownArgv = ["/usr/bin/node", "/srv/a/bin/launcher.mjs"]
        let first = try Self.child(of: held) { Self.system("tower", "linux", in: &$0) { $0["agent"] = shownArgv } }
        #expect(app.config.adopt(first, describedAs: "adopted") == nil)
        let shown = app.launchReview

        let second = try Self.child(of: first) { Self.system("tower", "linux", in: &$0) { $0["agent"] = ["/usr/bin/node", "/srv/b/bin/launcher.mjs"] } }
        #expect(app.config.adopt(second, describedAs: "adopted") == nil)
        app.approveLaunch(shown)

        #expect(app.approvals.record.value(at: .system(machine: "tower", system: "linux"))
                == .tuple(LaunchTuple(agent: shownArgv, grammar: "posix")))
        let waiting = try #require(app.launchReview.first)
        #expect(app.launchReview.count == 1)
        #expect(waiting.kind == .changed)
        #expect(waiting.offered == .tuple(LaunchTuple(agent: ["/usr/bin/node", "/srv/b/bin/launcher.mjs"], grammar: "posix")))
        let system = try #require(app.machine(id: "tower")?.machine.system(id: "linux"))
        #expect(system.agent == shownArgv)
        #expect(system.launchState == .approvedWithChangeWaiting)
    }

    @Test("the review comes up by itself once per document, and putting it off keeps the notice")
    func reviewIsPutOffPerDocument() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let held = try Self.held()
        let app = try Self.app(held, in: home)
        let first = try Self.child(of: held) { Self.system("tower", "linux", in: &$0) { $0["agent"] = ["sh", "-c", "id"] } }
        #expect(app.config.adopt(first, describedAs: "adopted") == nil)
        #expect(app.isReviewingLaunch)

        app.deferLaunchReview(app.config.hash)
        #expect(!app.isReviewingLaunch)
        app.bindings.update { $0.deviceName = "Desk" }
        #expect(!app.isReviewingLaunch)
        #expect(!app.launchReview.isEmpty)

        let second = try Self.child(of: first) { Self.machine("pi", in: &$0) { $0["name"] = "Pi" } }
        #expect(app.config.adopt(second, describedAs: "adopted") == nil)
        #expect(app.isReviewingLaunch)
    }

    // MARK: - What runs

    @Test("serialisation reads only the gate's argv and grammar, and a restricted session only ever moves it to the dispatcher")
    func serialisationReadsTheGate() throws {
        var system = SystemConfig(id: "linux", platform: .linux, agent: ["node", "/a.mjs"], shell: "powershell", restricted: false)
        system.grammar = .success(.posix)
        #expect(try RemoteCommand(system: system, arguments: ["status"]).commandLine() == "node /a.mjs status")
        #expect(RemoteCommand(system: system, arguments: ["status"], restrictedSession: true).grammar == .success(.dispatch))
        system.agent = ["node", "/a.mjs", "extra"]
        #expect(RemoteCommand(system: system, arguments: [], restrictedSession: true).grammar
                == .failure(CommandLineRefusal("restricted-agent-shape")))
    }

    @Test("a system read from a document is not driven until the gate says so, so only the gated model reaches ssh")
    func decodedSystemsAreNotDriven() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let app = try Self.app(try Self.held(), in: home)

        let applied = try #require(app.config.config?.machine(id: "tower"))
        #expect(applied.systems.allSatisfy { $0.launchState == .notDriven })
        #expect(RemoteAgent.attempts(machine: applied, preferredSystem: nil, preferredRoute: nil).isEmpty)

        let gated = try #require(app.machine(id: "tower")?.machine)
        #expect(gated.systems.allSatisfy { $0.launchState == .approved })
        #expect(RemoteAgent.attempts(machine: gated, preferredSystem: nil, preferredRoute: nil).count == 2)
    }

    @Test("the machine section says which system waits, and names one the typed reading does not have")
    func machineSectionLines() {
        var changed = SystemConfig(id: "linux", name: "Linux", platform: .linux, agent: Self.launcher)
        changed.launchState = .approvedWithChangeWaiting
        var waiting = SystemConfig(id: "windows", name: "Windows 11", platform: .windows, agent: [])
        waiting.launchState = .notDriven
        let machine = Machine(id: "tower", name: "Tower",
                              endpoints: [Endpoint(id: "lan", kind: "lan", host: "192.0.2.20", user: "me")],
                              systems: [changed, waiting])
        func line(_ location: LaunchLocation) -> String {
            MachineSection.launchLine(LaunchReviewEntry(location: location, kind: .changed, approvable: true,
                                                        approved: nil, offered: .text("~/.ssh/id_tower")),
                                      on: machine)
        }

        #expect(line(.system(machine: "tower", system: "linux")) == "Linux: Runs the approved command. A changed one is waiting for approval.")
        #expect(line(.system(machine: "tower", system: "windows")) == "Windows 11: Not driven from this device until its command is approved.")
        #expect(line(.system(machine: "tower", system: "spare\u{202E}")) == "The command for spare\\u{202E} is waiting for approval. Nothing is sent to it from this device.")
        #expect(line(.identityFile(machine: "tower")) == "Its identity file is waiting for approval. This device offers the one it approved, or none.")
    }

    @Test("a system this device has not approved is never bootstrapped over a 2.x agent")
    func notDrivenIsNeverBootstrapped() async throws {
        let runner = MachineModelTests.FakeRunner()
        runner.answer("status", json: #"{ "ok": true, "agentVersion": "2.1.0", "os": "windows" }"#)
        var waiting = SystemConfig(id: "windows", name: "Windows", platform: .windows, agent: [])
        waiting.launchState = .notDriven
        let machine = Machine(id: "tower", name: "Tower",
                              endpoints: [Endpoint(id: "lan", kind: "lan", host: "192.0.2.20", user: "me")],
                              systems: [SystemConfig(id: "linux", name: "Linux", platform: .linux, agent: ["node", "/a.mjs"]), waiting])
        var agent = RemoteAgent(machine: machine)
        agent.logDirectory = FakeSSH.logDirectory()
        agent.runner = { _, arguments, _, _ in runner.run(arguments) }
        let model = MachineModel(machine: machine, agent: agent)

        await model.refresh(userInitiated: false)
        #expect(model.commandableSystem?.id == "windows")
        #expect(model.legacyInstallRefusal == "Windows is not driven from this device until its command is approved.")
    }

    // MARK: - The record and the editors

    @Test("the record nests ids, keeps keys it does not know, and an edit records only what it can approve")
    func recordShape() throws {
        let home = try Self.home()
        defer { try? FileManager.default.removeItem(at: home) }
        let url = home.appending(path: "approved-launch.json")
        try Data(#"{ "schema": 1, "seededFrom": "none", "note": "kept", "machines": { "a/b": { "extra": 1 } } }"#.utf8).write(to: url)
        let store = LaunchApprovalStore(url: url)

        let document = try JSONText.canonicalDocument([
            "machines": [["id": "a/b", "systems": [
                ["id": "x.y", "agent": ["/usr/bin/node", "/opt/lc/bin/launcher.mjs"]],
                ["id": "hidden", "agent": ["/usr/bin/node", "/opt/lc/bin/\u{202E}sjm.rehcnual"]],
            ]]],
        ])
        store.recordLocalEdit(before: nil, after: document)

        let bytes = try #require(Self.file(url))
        let written = try #require(try JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        #expect(written["note"] as? String == "kept")
        let machine = try #require((written["machines"] as? [String: Any])?["a/b"] as? [String: Any])
        #expect(machine["extra"] as? Int == 1)
        let systems = try #require(machine["systems"] as? [String: Any])
        #expect(Set(systems.keys) == ["x.y"])
        #expect((systems["x.y"] as? [String: Any])?["grammar"] as? String == "posix")
    }

    @Test("an editor never saves an empty argv, nor one holding a control or direction-changing character")
    func editorsRefuseUnrunnableCommands() throws {
        var root = try #require(try JSONSerialization.jsonObject(with: try Self.held()) as? [String: Any])
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertSystem(["id": "spare", "agent": [String]()], onMachine: "tower", in: &root)
        }
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertSystem(["id": "spare", "agent": ["node", "/a\u{202E}b.mjs"]], onMachine: "tower", in: &root)
        }
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertSystem(["id": "spare", "name": "Spare"], onMachine: "tower", in: &root)
        }
        #expect(throws: ControllerEditor.EditFailure.self) {
            try ControllerEditor.upsertMachine(["id": "nas", "systems": [["id": "linux", "agent": ["node", "/a\u{0007}.mjs"]]]], in: &root)
        }
        #expect(CommandArguments.parse("[]") == nil)
        #expect(CommandArguments.parse(#"["node", "/a\u202Eb.mjs"]"#) == nil)
        #expect(CommandArguments.parse(#"["node", "/a\u200Bb.mjs"]"#) == nil)
        #expect(CommandArguments.parse(#"["node", "/Users/My Name/b.mjs"]"#) == ["node", "/Users/My Name/b.mjs"])
    }
}
