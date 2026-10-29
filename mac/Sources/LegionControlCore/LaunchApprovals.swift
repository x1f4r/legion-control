import Foundation

/// This device's record of the launch values it approved (section 11 of the contract).
///
/// Kept as the JSON object it was read as, so keys a newer build wrote survive this one writing it,
/// and read leniently, the way the reference reads one: a value of the wrong shape approves nothing,
/// but it is still something recorded at that path, which is what makes a review entry `changed`
/// rather than `new`.
///
/// `@unchecked Sendable` for the reason `JSONAny` is: the object only ever holds JSON values, and
/// the compiler cannot see that through `Any`.
struct LaunchRecord: @unchecked Sendable {
    private(set) var object: [String: Any]

    init(object: [String: Any]) {
        self.object = object
    }

    /// What a device that has approved nothing writes, so that no later adoption ever counts as the
    /// seed.
    static var nothingApproved: LaunchRecord { LaunchRecord(object: ["schema": 1, "seededFrom": "none"]) }

    /// The hash of the document this record was seeded from, or "none".
    var seededFrom: String? { object["seededFrom"] as? String }

    /// The file's bytes: sorted keys, two-space indent, one trailing newline.
    func data() throws -> Data {
        Data((try JSONText.stringify(object) + "\n").utf8)
    }

    // MARK: - Reading

    /// Every approved value, wherever it sits: a value approved anywhere is accepted anywhere.
    struct Approved {
        var tuples: [LaunchTuple] = []
        var identityFiles: [String] = []
        var repo: String?
        var localAgent: String?
    }

    var approved: Approved {
        var approved = Approved()
        for case let machine as [String: Any] in ((object["machines"] as? [String: Any]) ?? [:]).values {
            for value in ((machine["systems"] as? [String: Any]) ?? [:]).values {
                if let tuple = LaunchTuple(json: value) { approved.tuples.append(tuple) }
            }
            if let path = machine["identityFile"] as? String { approved.identityFiles.append(path) }
        }
        approved.repo = object["githubRepo"] as? String
        approved.localAgent = (object["local"] as? [String: Any])?["agent"] as? String
        return approved
    }

    /// Whatever the record holds at one location, of any shape, or nil when it holds nothing there.
    func recorded(at location: LaunchLocation) -> Any? {
        let machines = object["machines"] as? [String: Any]
        switch location {
        case .system(let machine, let system):
            return (((machines?[machine] as? [String: Any])?["systems"]) as? [String: Any])?[system]
        case .identityFile(let machine):
            return (machines?[machine] as? [String: Any])?["identityFile"]
        case .repo:
            return object["githubRepo"]
        case .local:
            return object["local"]
        }
    }

    /// What the record holds at one location, when it is a value of the right shape.
    func value(at location: LaunchLocation) -> LaunchValue? {
        let recorded = recorded(at: location)
        switch location.scope {
        case .system: return LaunchTuple(json: recorded).map(LaunchValue.tuple)
        case .identityFile, .repo: return (recorded as? String).map(LaunchValue.text)
        case .local: return (recorded as? [String: Any]).map { .local(agent: $0["agent"] as? String) }
        }
    }

    /// The recorded local agent, and whether it could run: an agent that is null means the default
    /// path, and one that is missing or not a string means nothing runnable was recorded.
    struct RecordedLocal {
        var agent: String?
        var isRunnable: Bool
    }

    var recordedLocal: RecordedLocal? {
        guard let local = object["local"] as? [String: Any] else { return nil }
        switch local["agent"] {
        case let agent as String: return RecordedLocal(agent: agent, isRunnable: LaunchValues.isLaunchText(agent))
        case is NSNull: return RecordedLocal(agent: nil, isRunnable: true)
        default: return RecordedLocal(agent: nil, isRunnable: false)
        }
    }

    // MARK: - Writing

    /// Record one value at its location, keeping every other key at every level.
    ///
    /// Ids are nested keys, never joined into one, because an id may contain a slash.
    mutating func set(_ value: LaunchValue, at location: LaunchLocation) {
        if object["schema"] == nil { object["schema"] = 1 }
        switch (location, value) {
        case (.system(let machineId, let systemId), .tuple(let tuple)):
            updateMachine(machineId) { machine in
                var systems = (machine["systems"] as? [String: Any]) ?? [:]
                var entry = (systems[systemId] as? [String: Any]) ?? [:]
                entry["agent"] = tuple.agent
                entry["grammar"] = tuple.grammar
                systems[systemId] = entry
                machine["systems"] = systems
            }
        case (.identityFile(let machineId), .text(let path)):
            updateMachine(machineId) { $0["identityFile"] = path }
        case (.repo, .text(let repo)):
            object["githubRepo"] = repo
        case (.local, .local(let agent)):
            var local = (object["local"] as? [String: Any]) ?? [:]
            local["agent"] = agent ?? NSNull()
            object["local"] = local
        default:
            // A value of another scope has no place here, and nothing is recorded for it.
            break
        }
    }

    private mutating func updateMachine(_ id: String, _ change: (inout [String: Any]) -> Void) {
        var machines = (object["machines"] as? [String: Any]) ?? [:]
        var machine = (machines[id] as? [String: Any]) ?? [:]
        change(&machine)
        machines[id] = machine
        object["machines"] = machines
    }
}

/// One value to record at one location.
struct LaunchRecording: Sendable, Equatable {
    var location: LaunchLocation
    var value: LaunchValue
}
