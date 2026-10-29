import Foundation

/// Launch values and local approval: section 9.9 of the contract, as `contract/tools/launch.mjs`
/// has it.
///
/// The shared setup carries the argv this Mac runs over ssh with its own key, and lineage only
/// shows that a document claims an ancestor, not who wrote it. Before this, any key that could run
/// `config set`, a restricted one included, could publish a descendant whose `agent` was
/// `sh -c …`, and this Mac adopted it and ran it on every machine with its own unrestricted key.
/// The document still replicates and applies unchanged. What changes is that this device runs only
/// the launch values it approved itself, and everything else waits for a person here.
///
/// This file is the pure part every client agrees on, held to the reference case by case by
/// `contract/vectors/launch.json`: which values are launch values and how they are read, which
/// offered values are accepted without asking, which wait and whether they could be approved at
/// all, and what runs meanwhile. The record of what this device approved is LaunchApprovals.swift,
/// and LaunchGate.swift writes the outcome into the typed model every transport reads.
///
/// Everything is read from the parsed JSON, never from `ControllerConfig`. The typed model fills in
/// defaults, so a projection read through it would disagree with what the next device reads from
/// the same bytes. Strings compare by code point: Swift's `==` takes a composed and a decomposed
/// `é` for one string, and to a file system that does not normalise they are two files.
enum LaunchValues {
    /// The repository this app updates from when the setup names none.
    static let defaultRepo = ControllerConfig.AppUpdatesConfig.defaultRepo

    // MARK: - Reading a document

    /// The launch values of a parsed document (section 9.9, "Reading them").
    ///
    /// Machines and systems are walked in order. A non-object, an item without a string id, and a
    /// repeated id after its first occurrence are skipped, which is what the next device does with
    /// the same bytes whatever its own decoder makes of them.
    static func projection(of document: Any?) -> LaunchProjection {
        let root = document as? [String: Any]
        var projection = LaunchProjection()
        var seenMachines = Set<[Unicode.Scalar]>()
        for case let machine as [String: Any] in (root?["machines"] as? [Any]) ?? [] {
            guard let id = machine["id"] as? String, seenMachines.insert(Array(id.unicodeScalars)).inserted else { continue }
            var systems: [LaunchProjection.System] = []
            var seenSystems = Set<[Unicode.Scalar]>()
            for case let system as [String: Any] in (machine["systems"] as? [Any]) ?? [] {
                guard let systemId = system["id"] as? String,
                      seenSystems.insert(Array(systemId.unicodeScalars)).inserted else { continue }
                systems.append(.init(id: systemId, tuple: tuple(of: system)))
            }
            let identityFile = (machine["ssh"] as? [String: Any])?["identityFile"] as? String
            projection.machines.append(.init(id: id, systems: systems,
                                             identityFile: identityFile?.isEmpty == false ? identityFile : nil))
        }
        if let repo = (root?["appUpdates"] as? [String: Any])?["githubRepo"] as? String,
           !repo.isEmpty, !same(repo, defaultRepo) {
            projection.repo = repo
        }
        if let local = root?["local"] as? [String: Any], !isJSONFalse(local["enabled"]) {
            let agent = local["agent"] as? String
            projection.local = .init(agent: agent?.isEmpty == false ? agent : nil)
        }
        return projection
    }

    /// The same, from a document's bytes. Bytes that are not a JSON object hold no launch values.
    static func projection(of document: Data?) -> LaunchProjection {
        projection(of: document.flatMap { try? JSONSerialization.jsonObject(with: $0) })
    }

    /// A system's tuple, or nil when it is inert because nothing runs on it: `agent` absent, not an
    /// array, empty, or holding anything but strings.
    ///
    /// The grammar, not the raw platform, shell and restricted fields, is what decides how the line
    /// is quoted. Recording the grammar means an editor that writes `"shell": "cmd"` where it was
    /// absent on Windows changes nothing that runs and asks nobody anything.
    static func tuple(of system: [String: Any]) -> LaunchTuple? {
        guard let items = system["agent"] as? [Any], !items.isEmpty else { return nil }
        let agent = items.compactMap { $0 as? String }
        guard agent.count == items.count else { return nil }
        // Section 15.1 reads the three fields as the document has them, with restrictedSession
        // false. A shell that is absent or null is the platform's default, and any other value that
        // is not one of the three spellings, a number included, is a shell nobody knows.
        let shell: String?
        switch system["shell"] {
        case nil, is NSNull: shell = nil
        case let text as String: shell = text
        default: shell = ""
        }
        let platform: Platform? = (system["platform"] as? String).flatMap { same($0, "windows") ? .windows : nil }
        let resolved = CommandGrammar.resolve(platform: platform, shell: shell,
                                              restricted: isJSONTrue(system["restricted"]) ? true : nil,
                                              restrictedSession: false, agentLength: agent.count)
        switch resolved {
        case .success(let grammar): return LaunchTuple(agent: agent, grammar: grammar.rawValue)
        case .failure(let refusal): return LaunchTuple(agent: agent, grammar: "refused:" + refusal.code)
        }
    }

    // MARK: - What can be approved at all

    /// Whether an argv item, an identityFile or local.agent can be shown to a person and approved.
    ///
    /// Control characters, C1 controls, zero-width and bidirectional formatting characters, word
    /// joiners and the byte order mark can each make the text a person approves look different from
    /// the text that runs, so a value holding one can only be reverted or left.
    static func isLaunchText(_ text: String) -> Bool {
        let scalars = text.unicodeScalars
        if scalars.contains(where: { $0 == "\n" || $0 == "\r" }) { return false }
        guard (1...4096).contains(text.utf8.count) else { return false }
        return !scalars.contains(where: isHidden)
    }

    /// A repository this app could update from: GitHub's own owner/name.
    static func isRepo(_ value: String) -> Bool { SetupGuard.isRepository(value) }

    /// A tuple that can run: every argv item shown faithfully, and a grammar section 15.2 can write.
    static func isRunnable(_ tuple: LaunchTuple) -> Bool {
        !tuple.agent.isEmpty && tuple.agent.allSatisfy(isLaunchText) && tuple.commandGrammar != nil
    }

    /// The scalars a launch value may not hold, the reference's LAUNCH_TEXT class.
    static func isHidden(_ scalar: Unicode.Scalar) -> Bool {
        switch scalar.value {
        case 0x00...0x1F, 0x7F...0x9F, 0x200B...0x200F, 0x202A...0x202E, 0x2060...0x2064, 0x2066...0x2069, 0xFEFF:
            true
        default:
            false
        }
    }

    /// A value as a sheet can show it without being made to say something else: every scalar
    /// `isLaunchText` refuses is written out as its code point.
    static func visible(_ text: String) -> String {
        var shown = ""
        for scalar in text.unicodeScalars {
            if isHidden(scalar) { shown += String(format: "\\u{%04X}", scalar.value) } else { shown.unicodeScalars.append(scalar) }
        }
        return shown
    }

    // MARK: - The launcher migration

    /// Whether `offered` is the launcher migration of `approved`: the same grammar, the same argv
    /// except one item, which moves from `<base>/agent/src/index.mjs` to `<base>/bin/launcher.mjs`
    /// under the same non-empty base with no `..` segment.
    ///
    /// Compared after replacing `\` with `/`, because the desktop writes the new path with forward
    /// slashes into a Windows argv. The 3.1 upgrade migrates every system this way, and asking every
    /// peer to review a file the agent itself installs under a base already approved would add
    /// prompts and no protection. The reverse direction is never a migration.
    static func isLauncherMigration(approved: LaunchTuple?, offered: LaunchTuple?) -> Bool {
        guard let approved, let offered, !approved.agent.isEmpty, !offered.agent.isEmpty,
              same(approved.grammar, offered.grammar), approved.agent.count == offered.agent.count else { return false }
        let differing = approved.agent.indices.filter { !same(approved.agent[$0], offered.agent[$0]) }
        guard differing.count == 1, let index = differing.first else { return false }
        let from = slashed(approved.agent[index])
        let to = slashed(offered.agent[index])
        let entry = Array("/agent/src/index.mjs".unicodeScalars)
        let launcher = Array("/bin/launcher.mjs".unicodeScalars)
        guard from.count > entry.count, to.count > launcher.count,
              Array(from.suffix(entry.count)) == entry, Array(to.suffix(launcher.count)) == launcher else { return false }
        let base = Array(from.dropLast(entry.count))
        let dotdot: [Unicode.Scalar] = [".", "."]
        return base == Array(to.dropLast(launcher.count))
            && !base.split(separator: "/", omittingEmptySubsequences: false).contains { Array($0) == dotdot }
    }

    // MARK: - The record

    /// A record approving every launch value of `document`: what this device writes when it seeds
    /// itself from the document it already applies. An inert tuple runs nothing and is accepted
    /// anyway, so it is not recorded. `seededFrom` is the hash of that document, or "none".
    static func approvals(from document: Any?, seededFrom: String) -> LaunchRecord {
        let projection = projection(of: document)
        var record = LaunchRecord(object: ["schema": 1, "seededFrom": seededFrom])
        for machine in projection.machines {
            for system in machine.systems {
                if let tuple = system.tuple { record.set(.tuple(tuple), at: .system(machine: machine.id, system: system.id)) }
            }
            if let path = machine.identityFile { record.set(.text(path), at: .identityFile(machine: machine.id)) }
        }
        if let repo = projection.repo { record.set(.text(repo), at: .repo) }
        if let local = projection.local { record.set(.local(agent: local.agent), at: .local) }
        return record
    }

    // MARK: - Acceptance

    private static func accepts(_ approved: LaunchRecord.Approved, tuple: LaunchTuple?) -> Bool {
        guard let tuple else { return true }
        return approved.tuples.contains { $0 == tuple || isLauncherMigration(approved: $0, offered: tuple) }
    }

    private static func accepts(_ approved: LaunchRecord.Approved, identityFile: String?) -> Bool {
        guard let identityFile else { return true }
        return approved.identityFiles.contains { same($0, identityFile) }
    }

    private static func accepts(_ approved: LaunchRecord.Approved, repo: String?) -> Bool {
        guard let repo else { return true }
        return approved.repo.map { same($0, repo) } ?? false
    }

    private static func accepts(_ approved: LaunchRecord.Approved, local: LaunchProjection.Local?) -> Bool {
        guard let agent = local?.agent else { return true }
        return approved.localAgent.map { same($0, agent) } ?? false
    }

    /// One entry for each offered launch value that is not accepted without asking, in this order:
    /// for each machine its systems and then its identityFile, then the repository, then local.
    ///
    /// `kind` is `changed` when the record holds anything at that path and `new` otherwise.
    /// `approvable` is false for a value that can only be reverted or left: an argv item,
    /// identityFile or local.agent that fails `isLaunchText`, a grammar section 15.1 refuses, a
    /// repository that is not owner/name.
    static func review(record: LaunchRecord, projection: LaunchProjection) -> [LaunchReviewEntry] {
        let approved = record.approved
        var entries: [LaunchReviewEntry] = []
        func add(_ location: LaunchLocation, approvable: Bool, offered: LaunchValue) {
            let recorded = record.recorded(at: location)
            entries.append(LaunchReviewEntry(location: location, kind: recorded == nil ? .new : .changed,
                                             approvable: approvable, approved: record.value(at: location),
                                             offered: offered))
        }
        for machine in projection.machines {
            for system in machine.systems {
                guard let tuple = system.tuple, !accepts(approved, tuple: tuple) else { continue }
                add(.system(machine: machine.id, system: system.id), approvable: isRunnable(tuple), offered: .tuple(tuple))
            }
            if let path = machine.identityFile, !accepts(approved, identityFile: path) {
                add(.identityFile(machine: machine.id), approvable: isLaunchText(path), offered: .text(path))
            }
        }
        if let repo = projection.repo, !accepts(approved, repo: repo) {
            add(.repo, approvable: isRepo(repo), offered: .text(repo))
        }
        if let local = projection.local, !accepts(approved, local: local) {
            add(.local, approvable: local.agent.map(isLaunchText) ?? true, offered: .local(agent: local.agent))
        }
        return entries
    }

    /// What this device runs (section 9.9, "Everything else waits"): one entry per offered system,
    /// then one identityFile entry per machine, then the repository, then local.
    ///
    /// An accepted value that can run is used as offered. Anything else falls back to what the
    /// record holds at that same location, never at another one, and only when the recorded value
    /// can run itself. With nothing usable there a system is not driven, an identityFile or local is
    /// none, and the repository is the built-in one. An inert tuple is not driven: nothing runs on it.
    static func effective(record: LaunchRecord, projection: LaunchProjection) -> [LaunchEffective] {
        let approved = record.approved
        var entries: [LaunchEffective] = []
        for machine in projection.machines {
            for system in machine.systems {
                let location = LaunchLocation.system(machine: machine.id, system: system.id)
                var entry = LaunchEffective(location: location, state: .notDriven)
                if let tuple = system.tuple {
                    if accepts(approved, tuple: tuple), isRunnable(tuple) {
                        entry.state = .offered
                        entry.tuple = tuple
                    } else if case .tuple(let recorded)? = record.value(at: location), isRunnable(recorded) {
                        entry.state = .approved
                        entry.tuple = recorded
                    }
                }
                entries.append(entry)
            }
        }
        for machine in projection.machines {
            let location = LaunchLocation.identityFile(machine: machine.id)
            var entry = LaunchEffective(location: location, state: .none)
            if let offered = machine.identityFile {
                if accepts(approved, identityFile: offered), isLaunchText(offered) {
                    entry.state = .offered
                    entry.text = offered
                } else if case .text(let recorded)? = record.value(at: location), isLaunchText(recorded) {
                    entry.state = .approved
                    entry.text = recorded
                }
            }
            entries.append(entry)
        }
        var repo = LaunchEffective(location: .repo, state: .builtIn)
        if let offered = projection.repo {
            if accepts(approved, repo: offered), isRepo(offered) {
                repo.state = .offered
                repo.text = offered
            } else if case .text(let recorded)? = record.value(at: .repo), isRepo(recorded), !same(recorded, defaultRepo) {
                repo.state = .approved
                repo.text = recorded
            }
        }
        entries.append(repo)
        var local = LaunchEffective(location: .local, state: .none)
        if let offered = projection.local {
            if accepts(approved, local: offered), offered.agent.map(isLaunchText) ?? true {
                local.state = .offered
                local.text = offered.agent
            } else if let recorded = record.recordedLocal, recorded.isRunnable {
                local.state = .approved
                local.text = recorded.agent
            }
        }
        entries.append(local)
        return entries
    }

    // MARK: - Small pieces

    /// Two strings as the contract compares them: by code point, with no normalisation.
    static func same(_ lhs: String, _ rhs: String) -> Bool {
        lhs.unicodeScalars.elementsEqual(rhs.unicodeScalars)
    }

    private static func slashed(_ text: String) -> [Unicode.Scalar] {
        text.unicodeScalars.map { $0 == "\\" ? "/" : $0 }
    }

    /// JSON `true` and nothing else: Foundation reads a 1 as true too when asked for a Bool.
    static func isJSONTrue(_ value: Any?) -> Bool {
        guard let number = value as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() else { return false }
        return number.boolValue
    }

    static func isJSONFalse(_ value: Any?) -> Bool {
        guard let number = value as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() else { return false }
        return !number.boolValue
    }
}

// MARK: - Values

/// A system's launch tuple: the argv this device runs, and the grammar its line is written in.
struct LaunchTuple: Sendable {
    var agent: [String]
    /// One of the four grammars of section 15.1, or `refused:<code>` when that section refuses the
    /// system.
    var grammar: String

    /// A JSON value the record or a vector holds, when it is a tuple: a non-empty argv of strings
    /// and a string grammar.
    init?(json: Any?) {
        guard let object = json as? [String: Any], let items = object["agent"] as? [Any], !items.isEmpty,
              let grammar = object["grammar"] as? String else { return nil }
        let agent = items.compactMap { $0 as? String }
        guard agent.count == items.count else { return nil }
        self.agent = agent
        self.grammar = grammar
    }

    init(agent: [String], grammar: String) {
        self.agent = agent
        self.grammar = grammar
    }

    /// The grammar a line can be written in, or nil for a refused one.
    var commandGrammar: CommandGrammar? {
        CommandGrammar.allCases.first { LaunchValues.same($0.rawValue, grammar) }
    }

    var json: [String: Any] { ["agent": agent, "grammar": grammar] }
}

extension LaunchTuple: Equatable {
    /// JSON equality: the argv in order, every string by code point.
    static func == (lhs: LaunchTuple, rhs: LaunchTuple) -> Bool {
        LaunchValues.same(lhs.grammar, rhs.grammar) && lhs.agent.count == rhs.agent.count
            && zip(lhs.agent, rhs.agent).allSatisfy(LaunchValues.same)
    }
}

/// Which of the four kinds a launch value is.
enum LaunchScope: String, Sendable {
    case system, identityFile, repo, local
}

/// Where a launch value sits in the setup (`common.schema.json#/$defs/launchPath`).
enum LaunchLocation: Hashable, Sendable {
    case system(machine: String, system: String)
    case identityFile(machine: String)
    case repo
    case local

    /// Ids are written as they are, inside the brackets, never joined or escaped: an id may hold a
    /// dot or a slash, which is also why the record nests them as keys.
    var path: String {
        switch self {
        case .system(let machine, let system): "machines[\(machine)].systems[\(system)]"
        case .identityFile(let machine): "machines[\(machine)].ssh.identityFile"
        case .repo: "appUpdates.githubRepo"
        case .local: "local"
        }
    }

    var scope: LaunchScope {
        switch self {
        case .system: .system
        case .identityFile: .identityFile
        case .repo: .repo
        case .local: .local
        }
    }

    var machineId: String? {
        switch self {
        case .system(let machine, _), .identityFile(let machine): machine
        case .repo, .local: nil
        }
    }
}

/// One launch value, as a document offers it or the record holds it.
enum LaunchValue: Sendable {
    case tuple(LaunchTuple)
    /// An identityFile path, or a repository.
    case text(String)
    /// `local`, whose agent is nil for the default path.
    case local(agent: String?)
}

extension LaunchValue: Equatable {
    static func == (lhs: LaunchValue, rhs: LaunchValue) -> Bool {
        switch (lhs, rhs) {
        case (.tuple(let a), .tuple(let b)): a == b
        case (.text(let a), .text(let b)): LaunchValues.same(a, b)
        case (.local(nil), .local(nil)): true
        case (.local(let a?), .local(let b?)): LaunchValues.same(a, b)
        default: false
        }
    }
}

/// The launch values of one document.
struct LaunchProjection: Sendable {
    struct Machine: Sendable {
        var id: String
        var systems: [System]
        /// Nil when the machine names none.
        var identityFile: String?
    }

    struct System: Sendable {
        var id: String
        /// Nil when the tuple is inert: nothing runs on this system.
        var tuple: LaunchTuple?
    }

    struct Local: Sendable {
        /// Nil for the default path.
        var agent: String?
    }

    var machines: [Machine] = []
    /// Nil for the built-in repository.
    var repo: String?
    /// Nil when the setup asks for no local agent.
    var local: Local?
}

/// An offered launch value this device has not accepted without asking.
struct LaunchReviewEntry: Sendable, Equatable, Identifiable {
    enum Kind: String, Sendable {
        /// The record holds something at this path: the value there changed.
        case changed
        /// Nothing was ever approved here.
        case new
    }

    var location: LaunchLocation
    var kind: Kind
    /// False for a value that can only be reverted or left.
    var approvable: Bool
    /// What the record holds at this path, when it holds a value of the right shape: what runs
    /// meanwhile, and what Revert puts back.
    var approved: LaunchValue?
    var offered: LaunchValue

    var path: String { location.path }
    var scope: LaunchScope { location.scope }
    var id: String { path }
}

/// What this device runs at one location.
struct LaunchEffective: Sendable, Equatable {
    enum State: String, Sendable {
        /// The offered value, accepted and able to run.
        case offered
        /// The value the record holds at this location, because the offered one waits.
        case approved
        /// A system nothing runs on from this device.
        case notDriven = "not-driven"
        /// The repository this app was built to update from.
        case builtIn = "built-in"
        /// No identityFile, or no local agent.
        case none
    }

    var location: LaunchLocation
    var state: State
    /// The tuple that runs, for a system in state offered or approved.
    var tuple: LaunchTuple?
    /// The identityFile path, the repository or local.agent; nil for the built-in repository and
    /// for local's default path.
    var text: String?
}
