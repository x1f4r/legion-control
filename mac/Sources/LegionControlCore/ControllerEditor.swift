import Foundation

/// Editing the shared setup from inside the app.
///
/// The file stays the source of truth and stays hand editable: this writes JSON a person can read
/// and never a serialised form of the app's own model. What it adds is the bookkeeping a hand edit
/// cannot do — a revision that counts up and, more importantly, the lineage that lets every other
/// device tell "this descends from what I hold" from "this is a different branch".
///
/// The document is edited as plain JSON rather than through `ControllerConfig`. A setup may
/// legitimately carry keys this build has never heard of: a newer client wrote them, or only the
/// phone reads them. Round-tripping through the typed model would delete every one of them, silently
/// and on one device only.
enum ControllerEditor {

    struct EditFailure: Error, Sendable, Equatable {
        var message: String
        init(_ message: String) { self.message = message }
    }

    /// The result of an edit: what to write, and what the identity became.
    struct Edited: Sendable, Equatable {
        var bytes: Data
        var identity: ControllerIdentity
    }

    /// Apply a change and stamp the new identity on it.
    ///
    /// `parentHash` is the canonical hash of the document being edited, and it becomes the first
    /// entry of the new lineage. That is the whole mechanism: a machine holding the parent sees its
    /// own hash in the lineage of what arrives and accepts it as a fast-forward, and a machine
    /// holding something else does not, and says so instead of being overwritten.
    static func apply(
        to raw: Data,
        parentHash: String,
        parentIdentity: ControllerIdentity,
        deviceName: String,
        now: Date = Date(),
        newIdentifier: @Sendable () -> String = { "setup-" + UUID().uuidString.lowercased() },
        change: (inout [String: Any]) throws -> Void
    ) throws -> Edited {
        guard var root = (try? JSONSerialization.jsonObject(with: raw)) as? [String: Any] else {
            throw EditFailure("The config file is not a JSON object, so it cannot be edited from here. Fix it in an editor first.")
        }

        try change(&root)

        // The identity is stamped after the change rather than before, so a change that threw leaves
        // the document and its lineage exactly as they were.
        let existing = (root["controller"] as? [String: Any]) ?? [:]
        var identity = parentIdentity

        // A document that never had an identity gets one now, and only now: an id invented on every
        // load would make two devices reading the same file believe they were on different setups.
        if identity.id == nil {
            identity.id = (existing["id"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? newIdentifier()
        }
        if identity.name == nil { identity.name = existing["name"] as? String }

        let next = identity.next(parent: parentHash, device: deviceName, now: now)
        root["controller"] = SetupMerge.encode(next)

        let bytes: Data
        do {
            bytes = try JSONText.canonicalDocument(root)
        } catch {
            throw EditFailure("The edited config could not be written back as JSON. \(error.localizedDescription)")
        }

        return Edited(bytes: bytes, identity: next)
    }

    /// Give a document that has never had one an identity, without otherwise touching it.
    ///
    /// This is what happens to a hand written file or a pasted example the first time the app reads
    /// it: it becomes a setup of its own, with a fresh id, revision 1 and no ancestors. It is not a
    /// claim about anybody else's setup, and if the machines turn out to hold another one the user
    /// gets the ordinary "different setup" question.
    static func adopting(
        _ raw: Data,
        deviceName: String,
        setupName: String? = nil,
        now: Date = Date(),
        newIdentifier: @Sendable () -> String = { "setup-" + UUID().uuidString.lowercased() }
    ) throws -> Edited {
        guard var root = (try? JSONSerialization.jsonObject(with: raw)) as? [String: Any] else {
            throw EditFailure("The config file is not a JSON object, so it cannot be given an identity.")
        }
        let identity = ControllerIdentity(
            id: newIdentifier(),
            name: setupName ?? (root["controller"] as? [String: Any])?["name"] as? String,
            revision: 1,
            updatedAt: ISO8601DateFormatter.lenient.string(from: now),
            source: ControllerIdentity.sourceKind,
            device: deviceName,
            lineage: []
        )
        root["controller"] = SetupMerge.encode(identity)
        return Edited(bytes: try JSONText.canonicalDocument(root), identity: identity)
    }

    // MARK: - The changes the app knows how to make

    /// Add or replace one machine, keeping every key of an existing entry the app does not manage.
    static func upsertMachine(_ machine: [String: Any], in root: inout [String: Any]) throws {
        guard let id = machine["id"] as? String, !id.isEmpty else {
            throw EditFailure("A machine needs an id.")
        }
        guard AgentToken.isValid(id) else {
            throw EditFailure("\"\(id)\" is not a usable machine id. Use letters, digits and . _ - : @ / \\ ~ = +.")
        }
        var machines = (root["machines"] as? [[String: Any]]) ?? []
        if let index = machines.firstIndex(where: { $0["id"] as? String == id }) {
            var merged = machines[index]
            for (key, value) in machine {
                if value is NSNull { merged.removeValue(forKey: key) } else { merged[key] = value }
            }
            machines[index] = merged
        } else {
            machines.append(machine)
        }
        root["machines"] = machines
    }

    static func removeMachine(id: String, in root: inout [String: Any]) throws {
        let machines = (root["machines"] as? [[String: Any]]) ?? []
        let remaining = machines.filter { $0["id"] as? String != id }
        guard remaining.count != machines.count else {
            throw EditFailure("There is no machine called \"\(id)\" in the file.")
        }
        root["machines"] = remaining
    }

    /// Add or replace one system on one machine.
    static func upsertSystem(_ system: [String: Any], onMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            try upsertKeyed(system, in: &machine, list: "systems", what: "system")
        }
    }

    static func removeSystem(id: String, fromMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            var systems = (machine["systems"] as? [[String: Any]]) ?? []
            systems.removeAll { $0["id"] as? String == id }
            guard !systems.isEmpty else {
                throw EditFailure("Every machine needs at least one system, so the last one cannot be removed.")
            }
            machine["systems"] = systems
        }
    }

    /// Add or replace one endpoint on one machine.
    static func upsertEndpoint(_ endpoint: [String: Any], onMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            try upsertKeyed(endpoint, in: &machine, list: "endpoints", what: "endpoint")
        }
    }

    static func removeEndpoint(id: String, fromMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            var endpoints = (machine["endpoints"] as? [[String: Any]]) ?? []
            endpoints.removeAll { $0["id"] as? String == id }
            machine["endpoints"] = endpoints
        }
    }

    /// Which site a machine is at, or nil to unplace it.
    static func setSite(_ site: String?, onMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            if let site, !site.isEmpty { machine["site"] = site } else { machine.removeValue(forKey: "site") }
        }
    }

    static func setAlwaysOn(_ alwaysOn: Bool, onMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            machine["alwaysOn"] = alwaysOn
        }
    }

    /// Replace the ordered helper list for one machine.
    ///
    /// Both keys are written: `helpers` for anything that understands failover, and `helper` set to
    /// the first of them so a 1.2 client keeps waking the machine the one way it knows. Clearing the
    /// list removes both.
    static func setWakeHelpers(_ helpers: [WakeHelper], onMachine machineId: String, in root: inout [String: Any]) throws {
        try mutateMachine(machineId, in: &root) { machine in
            guard var wake = machine["wake"] as? [String: Any] else {
                throw EditFailure("\(machineId) has no wake block, so it cannot have wake helpers.")
            }
            if helpers.isEmpty {
                wake.removeValue(forKey: "helpers")
                wake.removeValue(forKey: "helper")
            } else {
                wake["helpers"] = helpers.map { ["machine": $0.machine, "action": $0.action] }
                wake["helper"] = ["machine": helpers[0].machine, "action": helpers[0].action]
            }
            machine["wake"] = wake
        }
    }

    /// Add or replace one site.
    static func upsertSite(_ site: [String: Any], in root: inout [String: Any]) throws {
        guard let id = site["id"] as? String, !id.isEmpty else {
            throw EditFailure("A site needs an id.")
        }
        var sites = (root["sites"] as? [[String: Any]]) ?? []
        if let index = sites.firstIndex(where: { $0["id"] as? String == id }) {
            var merged = sites[index]
            for (key, value) in site {
                if value is NSNull { merged.removeValue(forKey: key) } else { merged[key] = value }
            }
            sites[index] = merged
        } else {
            sites.append(site)
        }
        root["sites"] = sites
    }

    static func removeSite(id: String, in root: inout [String: Any]) throws {
        var sites = (root["sites"] as? [[String: Any]]) ?? []
        let before = sites.count
        sites.removeAll { $0["id"] as? String == id }
        guard sites.count != before else { throw EditFailure("There is no site called \"\(id)\".") }
        // A machine left pointing at a site that no longer exists would fail validation, so the
        // reference goes with it.
        var machines = (root["machines"] as? [[String: Any]]) ?? []
        for index in machines.indices where machines[index]["site"] as? String == id {
            machines[index].removeValue(forKey: "site")
        }
        root["sites"] = sites
        root["machines"] = machines
    }

    /// Name the setup itself.
    static func setSetupName(_ name: String, in root: inout [String: Any]) throws {
        var controller = (root["controller"] as? [String: Any]) ?? [:]
        controller["name"] = name
        root["controller"] = controller
    }

    static func setUpdateRepo(_ repo: String, in root: inout [String: Any]) throws {
        var updates = (root["appUpdates"] as? [String: Any]) ?? [:]
        updates["githubRepo"] = repo
        root["appUpdates"] = updates
    }

    /// Take this device's private settings out of the shared document.
    ///
    /// Never done on its own. `machine.ssh.identityFile` is a path that means something on exactly
    /// one computer and `local` means "whichever device is reading this", so both are wrong in a
    /// document every peer carries — but removing them changes the bytes, and therefore the hash,
    /// and doing that quietly on one device would look like a divergence to everyone else. So it is
    /// an edit a person asks for, and it produces an ordinary new revision.
    static func stripPrivateKeys(in root: inout [String: Any]) throws {
        var machines = (root["machines"] as? [[String: Any]]) ?? []
        for index in machines.indices {
            guard var ssh = machines[index]["ssh"] as? [String: Any] else { continue }
            ssh.removeValue(forKey: "identityFile")
            if ssh.isEmpty {
                machines[index].removeValue(forKey: "ssh")
            } else {
                machines[index]["ssh"] = ssh
            }
        }
        root["machines"] = machines
        root.removeValue(forKey: "local")
    }

    // MARK: - Small helpers

    private static func mutateMachine(
        _ id: String,
        in root: inout [String: Any],
        _ change: (inout [String: Any]) throws -> Void
    ) throws {
        var machines = (root["machines"] as? [[String: Any]]) ?? []
        guard let index = machines.firstIndex(where: { $0["id"] as? String == id }) else {
            throw EditFailure("There is no machine called \"\(id)\" in the file.")
        }
        var machine = machines[index]
        try change(&machine)
        machines[index] = machine
        root["machines"] = machines
    }

    private static func upsertKeyed(
        _ entry: [String: Any],
        in machine: inout [String: Any],
        list: String,
        what: String
    ) throws {
        guard let id = entry["id"] as? String, !id.isEmpty else {
            throw EditFailure("A \(what) needs an id.")
        }
        guard AgentToken.isValid(id) else {
            throw EditFailure("\"\(id)\" is not a usable \(what) id. Use letters, digits and . _ - : @ / \\ ~ = +.")
        }
        var entries = (machine[list] as? [[String: Any]]) ?? []
        if let index = entries.firstIndex(where: { $0["id"] as? String == id }) {
            var merged = entries[index]
            for (key, value) in entry {
                if value is NSNull { merged.removeValue(forKey: key) } else { merged[key] = value }
            }
            entries[index] = merged
        } else {
            entries.append(entry)
        }
        machine[list] = entries
    }
}
