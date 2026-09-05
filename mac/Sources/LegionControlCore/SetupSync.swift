import Foundation

/// Deciding what to do when a machine is holding a different setup document from this device.
///
/// Every device is a peer now: any of them may edit the setup, and none of them is on all the time.
/// That breaks the old rule outright. It compared revision numbers, and a revision number cannot
/// tell "newer" from "different": two devices that both edit revision 5 while apart both produce a
/// revision 6, and whichever reached a machine second would win and the other's work would vanish
/// without anyone being told.
///
/// What can tell the difference is ancestry. The document carries the hashes of the documents it was
/// made from, so:
///
///   - the machine holds one of my ancestors  → it is behind me, push, automatically
///   - I hold one of the machine's ancestors  → I am behind it, fetch, automatically
///   - neither                                → we have diverged, and a person decides
///
/// Both automatic moves follow strict descent, and descent is acyclic, so two devices can never
/// alternately overwrite each other. Everything else stops and asks. Nothing is ever lost, because
/// the agent refuses any push that does not descend from what it is holding.
enum SetupSync {

    /// What to do about one machine, given what it last said.
    enum Decision: Sendable, Equatable {
        /// The machine holds exactly this document.
        case inSync
        /// Send it. The reason is for the log, not for the user.
        case push(Push)
        /// This device is behind: read the machine's document and adopt it.
        case fetch
        /// The hashes differ and descent cannot be decided from the status alone. Read the
        /// machine's `config meta`, which carries the lineage, and decide from that.
        case readMeta
        /// Same setup, neither side descends from the other. A person has to look.
        case diverged
        /// Two different setups. A person has to choose which one this fleet is on.
        case differentSetup
        /// An agent from before any of this. It cannot carry the setup, and that is not a conflict.
        case unsupported
        /// Nothing to say yet: the machine has not answered, or this device has no usable document.
        case unknown

        enum Push: Sendable, Equatable {
            /// The machine holds nothing at all.
            case machineHasNothing
            /// The machine holds something with no identity: a copy from a 2.x client.
            case machineHasUnidentifiedCopy
            /// The machine holds one of this document's ancestors.
            case machineIsBehind
        }
    }

    /// What this device knows about one machine when it decides.
    struct MachineView: Sendable, Equatable {
        /// Whether the agent carries a controller copy at all.
        var reportsCopy: Bool
        /// The contract it speaks.
        var contract: Int
        /// The hash it reports in `status`.
        var hash: String?
        /// Its identity, from `status.controller` and, once read, from `config meta`.
        var identity: ControllerIdentity?
        /// Its lineage, which only `config meta` carries. Nil means "not read yet".
        var lineage: [String]?
    }

    /// The whole rule, as one pure function.
    static func decide(local: ControllerDocument?, machine: MachineView) -> Decision {
        // An agent that has never heard of a controller copy is not part of this at all. Saying it
        // conflicts would be a permanent warning about a machine that is simply older.
        guard machine.reportsCopy, machine.contract >= 3 else { return .unsupported }
        guard let local else { return .unknown }
        guard let remoteHash = machine.hash else {
            // It carries the copy and reports no hash: it is holding nothing.
            return .push(.machineHasNothing)
        }
        if remoteHash == local.hash { return .inSync }

        // A copy with no identity came from a 2.x client, and the agent's own rules let anything
        // replace it. Nothing of anyone's is lost by writing over a document nobody claims.
        if let identity = machine.identity, identity.id == nil, machine.lineage == nil {
            return .push(.machineHasUnidentifiedCopy)
        }
        if machine.identity == nil { return .push(.machineHasUnidentifiedCopy) }

        // Descent, from what the status alone can prove: the machine holds a document this one
        // remembers being made from.
        if local.identity.descends(from: remoteHash) {
            if let remoteId = machine.identity?.id, let mine = local.identity.id, remoteId != mine {
                return .differentSetup
            }
            return .push(.machineIsBehind)
        }

        if let remoteId = machine.identity?.id, let mine = local.identity.id, remoteId != mine {
            return .differentSetup
        }

        // Everything else needs the machine's own lineage, which status deliberately does not carry.
        guard let lineage = machine.lineage else { return .readMeta }
        if lineage.contains(local.hash) { return .fetch }
        return .diverged
    }

    /// How often a push that failed is tried again for the same document and machine.
    static let retryAfter: TimeInterval = 60

    /// What the Setup row shows, given a decision.
    static func sharing(for decision: Decision, machine: String, local: ControllerDocument?,
                        remote: ControllerIdentity?) -> SetupSharing {
        switch decision {
        case .inSync: return .upToDate
        case .push: return .publishing
        case .fetch: return .fetching
        case .readMeta, .unknown: return .unknown
        case .unsupported: return .unsupported
        case .diverged:
            let mine = local?.identity
            return .conflict("""
                \(machine) and this device have both changed the setup since they last agreed. \
                This device has revision \(mine?.revisionNumber ?? 0) (\(mine?.authorDescription ?? "unknown")), \
                \(machine) has revision \(remote?.revisionNumber ?? 0) (\(remote?.authorDescription ?? "unknown")). \
                Neither was made from the other, so nothing has been sent either way.
                """)
        case .differentSetup:
            return .conflict("""
                \(machine) is carrying a different setup (\(remote?.name ?? remote?.id ?? "unnamed")), \
                not an older copy of this one. Nothing has been sent: replacing it is a decision.
                """)
        }
    }
}
