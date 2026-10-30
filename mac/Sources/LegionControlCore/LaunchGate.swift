import Foundation

/// Whether this device runs a system's command, and which one (section 9.9 of the contract).
enum LaunchState: Sendable, Equatable {
    /// The command the setup names, which this device approved.
    case approved
    /// The command this device approved there before, because the one the setup names now is
    /// waiting for approval.
    case approvedWithChangeWaiting
    /// Nothing: the setup names no command for it, or one this device has not approved.
    case notDriven
}

/// The one place the shared setup becomes what this device runs.
///
/// Adopting a document and running it are separate. Every adoption applies the document exactly as
/// it came, so its hash and lineage advance and it goes on replicating, pending values and all. What
/// runs is this: the document's typed model with every launch value replaced by the gate's, which
/// is the offered value when this device approved it, the value it approved at the same place when
/// the offered one waits, and otherwise nothing. Every transport, bootstrap and update path reads
/// the gated model and nothing else, so it does not matter how a document arrived: a fetch, a
/// merge, a restore and a hand edit all meet the same check on the way to ssh.
///
/// Run on every load, every record change, every rebuild, and after every adoption.
enum LaunchGate {
    struct Gated {
        /// The setup as this device runs it.
        var config: ControllerConfig
        /// The values waiting for a person on this device.
        var review: [LaunchReviewEntry]
        /// Values accepted by reuse or by migration at a location whose record holds something
        /// else, for the record to take at that location.
        var accepted: [LaunchRecording]
    }

    /// The gate over one document. `document` is the bytes `config` was decoded from: the launch
    /// values are read from them, never from the typed model, and written into it.
    static func effective(_ config: ControllerConfig, document: Data?, record: LaunchRecord) -> Gated {
        let projection = LaunchValues.projection(of: document)
        let entries = LaunchValues.effective(record: record, projection: projection)
        var byLocation: [LaunchLocation: LaunchEffective] = [:]
        for entry in entries where byLocation[entry.location] == nil { byLocation[entry.location] = entry }

        var gated = config
        for machine in gated.machines.indices {
            let id = gated.machines[machine].id
            for system in gated.machines[machine].systems.indices {
                // A system the projection does not have is one the typed decoder read differently
                // from the bytes, a repeated key say. It is not driven: what runs is only ever what
                // was read from the bytes and approved.
                let systemId = gated.machines[machine].systems[system].id
                gated.machines[machine].systems[system].applyLaunch(byLocation[.system(machine: id, system: systemId)])
            }
            if gated.machines[machine].ssh != nil {
                gated.machines[machine].ssh?.identityFile = byLocation[.identityFile(machine: id)].flatMap(\.runningText)
            }
        }

        if let repo = byLocation[.repo]?.runningText {
            gated.appUpdates = ControllerConfig.AppUpdatesConfig(githubRepo: repo)
        } else {
            gated.appUpdates?.githubRepo = nil
        }

        if let local = byLocation[.local], local.state == .offered || local.state == .approved {
            gated.local = LocalConfig(enabled: true, name: config.local?.name ?? "This Mac",
                                      agent: local.text ?? LocalConfig.defaultAgentPath)
        } else {
            gated.local = nil
        }

        var accepted: [LaunchRecording] = []
        for entry in entries where entry.state == .offered {
            let value: LaunchValue
            switch entry.location.scope {
            case .system:
                guard let tuple = entry.tuple else { continue }
                value = .tuple(tuple)
            case .identityFile, .repo:
                guard let text = entry.text else { continue }
                value = .text(text)
            case .local:
                // The default path is accepted for being the default, not by reuse, so it is not
                // recorded as an approval of anything.
                guard let text = entry.text else { continue }
                value = .local(agent: text)
            }
            if record.value(at: entry.location) != value {
                accepted.append(LaunchRecording(location: entry.location, value: value))
            }
        }

        return Gated(config: gated, review: LaunchValues.review(record: record, projection: projection), accepted: accepted)
    }
}

private extension LaunchEffective {
    /// The identityFile or repository this device uses, or nil for none and the built-in one.
    var runningText: String? {
        state == .offered || state == .approved ? text : nil
    }
}

private extension SystemConfig {
    /// Take the gate's tuple, or nothing. The grammar goes in with the argv: serialisation reads only
    /// these two, so a peer that changes `shell` or `restricted` in the document changes nothing
    /// that runs here until it is approved.
    mutating func applyLaunch(_ entry: LaunchEffective?) {
        guard let entry, entry.state == .offered || entry.state == .approved,
              let tuple = entry.tuple, let grammar = tuple.commandGrammar else {
            agent = []
            self.grammar = .failure(CommandLineRefusal("no-command"))
            launchState = .notDriven
            return
        }
        agent = tuple.agent
        self.grammar = .success(grammar)
        launchState = entry.state == .offered ? .approved : .approvedWithChangeWaiting
    }
}
