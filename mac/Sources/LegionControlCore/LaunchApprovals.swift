import Foundation
import Observation

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

/// A setup a person asked to take from a device's agent while this device applies none.
///
/// Enrolment is one of the four things that approve (section 9.9 of the contract), and the only
/// one that approves a whole document a peer wrote, so nothing is written until every launch value
/// in it has been on screen and the person has confirmed it.
struct LaunchEnrolment: Sendable {
    var bytes: Data
    /// Whose copy it is, for the summary's heading.
    var holder: String
    var identity: ControllerIdentity
    /// Every launch value in it.
    var projection: LaunchProjection
    /// The same document decoded, for the machine and system names the summary shows.
    var config: ControllerConfig?
}

/// The file `approved-launch.json`, beside the applied-setup state, and what it says.
///
/// Not part of the bindings, though it is just as private: a 1.3 build re-encodes the bindings
/// struct when it saves them and would drop it. So it has a file of its own, which an older build
/// never opens, and a downgrade followed by an upgrade finds it where it was.
///
/// It is never published, merged or replicated. Four things write it (section 9.9): a person
/// approving what a review showed, an edit made on this device for the values it changed,
/// enrolment after its summary was confirmed, and once, the first start of this build, which
/// records the document it already applies. Nothing that arrives by replication approves anything.
@MainActor
@Observable
final class LaunchApprovalStore {
    /// What the gate reads. Empty, approving nothing, while the file cannot be read.
    private(set) var record = LaunchRecord(object: [:])
    /// Why the record could not be read or written, or nil.
    private(set) var problem: String?
    /// True while the file on disk is not a record. It is never seeded over: everything waits for
    /// approval, and the first approval moves the file aside rather than writing over it.
    private(set) var isUnreadable = false

    let url: URL

    @ObservationIgnored private var lastSignature: String?
    /// The clock the name of a file moved aside is read from.
    @ObservationIgnored var now: () -> Date = { Date() }

    init(url: URL) {
        self.url = url
        read()
    }

    private var path: String { url.path(percentEncoded: false) }

    // MARK: - Reading

    private func read() {
        lastSignature = ConfigStore.signature(of: url)
        guard let data = try? Data(contentsOf: url) else {
            record = LaunchRecord(object: [:])
            isUnreadable = false
            problem = nil
            return
        }
        guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
            record = LaunchRecord(object: [:])
            isUnreadable = true
            problem = "The record of the commands this device approved (\(url.lastPathComponent)) could not be read, so every command from the shared setup waits for approval. Approving one starts a new record and keeps the unreadable file beside it."
            return
        }
        record = LaunchRecord(object: object)
        isUnreadable = false
        problem = nil
    }

    /// Re-read the file when it changed under this device, which is checked before every gate
    /// evaluation. A record that has gone is seeded again from the document applied now: deleting
    /// it is how a person resets what this device approved. True when anything was re-read.
    @discardableResult
    func reloadIfChanged(seedingFrom document: Data?) -> Bool {
        let signature = ConfigStore.signature(of: url)
        guard signature != lastSignature else { return false }
        if signature == nil {
            seedIfMissing(from: document)
        } else {
            read()
        }
        return true
    }

    // MARK: - What approves

    /// The first start of a build with this record: every launch value of the document it already
    /// applies is recorded, so an upgrade drives exactly what it drove before and asks nothing. With
    /// no document the record says so, and no later adoption counts as a seed. Nothing happens when
    /// there is a file, readable or not.
    func seedIfMissing(from document: Data?) {
        guard !FileManager.default.fileExists(atPath: path) else { return }
        var seeded = LaunchRecord.nothingApproved
        if let document, let hash = try? Canonical.hash(document),
           let parsed = try? JSONSerialization.jsonObject(with: Canonical.bytes(document)), parsed is [String: Any] {
            seeded = LaunchValues.approvals(from: parsed, seededFrom: hash)
        }
        isUnreadable = false
        save(seeded)
    }

    /// An edit made on this device approves each launch value it changed, at its new value.
    ///
    /// Called before the setup file is written, so the edit's own values never flash up for review.
    /// `before` is the document the edit was made to, nil when there was none, which approves every
    /// launch value of `after`.
    ///
    /// A value that was waiting for approval before the edit is not approved by it. An edit to one
    /// field of a system whose command is waiting would otherwise approve whatever else the shared
    /// setup says about that system along with it; that value is approved on the review, where it
    /// is shown whole. Nor is a value recorded that could not be approved at all.
    func recordLocalEdit(before: Data?, after: Data) {
        let changes = Self.changes(from: LaunchValues.projection(of: before), to: LaunchValues.projection(of: after),
                                   record: record)
        guard !changes.isEmpty else { return }
        update { record in for change in changes { record.set(change.value, at: change.location) } }
    }

    /// Exactly the values a review showed, at their paths. A value the document has moved on to
    /// since then is a new entry of its own, and is not approved by this.
    func approve(_ entries: [LaunchReviewEntry]) {
        let approvable = entries.filter(\.approvable)
        guard !approvable.isEmpty else { return }
        update { record in for entry in approvable { record.set(entry.offered, at: entry.location) } }
    }

    /// Values accepted by reuse or by migration at a location whose record holds something else.
    /// Recording them there keeps the fallback for that location current, and a value overwritten
    /// this way leaves the approved set: the only pruning the record ever has.
    func recordAccepted(_ recordings: [LaunchRecording]) {
        let changed = recordings.filter { record.value(at: $0.location) != $0.value }
        guard !changed.isEmpty else { return }
        update { record in for change in changed { record.set(change.value, at: change.location) } }
    }

    /// What `recordLocalEdit` records.
    static func changes(from before: LaunchProjection, to after: LaunchProjection, record: LaunchRecord) -> [LaunchRecording] {
        let waiting = Set(LaunchValues.review(record: record, projection: before).map(\.location))
        var previous: [LaunchLocation: LaunchValue] = [:]
        for machine in before.machines {
            for system in machine.systems {
                if let tuple = system.tuple { previous[.system(machine: machine.id, system: system.id)] = .tuple(tuple) }
            }
            if let path = machine.identityFile { previous[.identityFile(machine: machine.id)] = .text(path) }
        }
        if let repo = before.repo { previous[.repo] = .text(repo) }
        if let agent = before.local?.agent { previous[.local] = .local(agent: agent) }

        var changes: [LaunchRecording] = []
        func consider(_ location: LaunchLocation, _ value: LaunchValue, approvable: Bool) {
            guard previous[location] != value, !waiting.contains(location), approvable else { return }
            changes.append(LaunchRecording(location: location, value: value))
        }
        for machine in after.machines {
            for system in machine.systems {
                guard let tuple = system.tuple else { continue }
                consider(.system(machine: machine.id, system: system.id), .tuple(tuple), approvable: LaunchValues.isRunnable(tuple))
            }
            if let path = machine.identityFile {
                consider(.identityFile(machine: machine.id), .text(path), approvable: LaunchValues.isLaunchText(path))
            }
        }
        if let repo = after.repo { consider(.repo, .text(repo), approvable: LaunchValues.isRepo(repo)) }
        // A local agent with no path of its own runs the default one, which is accepted as it is.
        if let agent = after.local?.agent { consider(.local, .local(agent: agent), approvable: LaunchValues.isLaunchText(agent)) }
        return changes
    }

    // MARK: - Writing

    /// Apply one change and write the record. An unreadable file is moved aside first, to
    /// `approved-launch.json.corrupt-<unix time>`, and the change starts a record of its own.
    private func update(_ change: (inout LaunchRecord) -> Void) {
        var next = isUnreadable ? LaunchRecord.nothingApproved : record
        change(&next)
        if isUnreadable, FileManager.default.fileExists(atPath: path) {
            let aside = url.deletingLastPathComponent()
                .appending(path: "\(url.lastPathComponent).corrupt-\(Int(now().timeIntervalSince1970))")
            do {
                try FileManager.default.moveItem(at: url, to: aside)
            } catch {
                problem = "The unreadable record of approved commands could not be moved aside, so nothing was approved. \(error.localizedDescription)"
                return
            }
        }
        isUnreadable = false
        save(next)
    }

    /// The record takes effect in this run whether or not the file could be written. One that could
    /// not be saved lasts until the app quits, and says so.
    private func save(_ next: LaunchRecord) {
        record = next
        do {
            try AtomicFile.write(next.data(), to: url)
            problem = nil
        } catch let failure as AtomicFile.WriteFailure {
            problem = "The approved commands could not be saved, so they last only until Legion Control quits. \(failure.message)"
        } catch {
            problem = "The approved commands could not be saved, so they last only until Legion Control quits. \(error.localizedDescription)"
        }
        lastSignature = ConfigStore.signature(of: url)
    }
}
