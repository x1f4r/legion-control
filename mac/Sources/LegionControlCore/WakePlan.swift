import Foundation

/// How to wake one machine from where this device happens to be.
///
/// A magic packet is a broadcast on a local network, so it only works from that network. One helper
/// in one house cannot wake a tower in another, whatever a config says. The plan is therefore
/// ordered: send it directly if this device is on the right network, otherwise ask a machine that is
/// — in the order the setup lists them, until one of them does it.
///
/// Two rules the plan never breaks. Nothing is ever woken in order to serve as a helper: a wake
/// cascade would turn an always-off tower into an always-on one by accident, which is the opposite
/// of what the user asked for. And when nothing can be done, the app says so with a reason for each
/// helper rather than pretending the packet went somewhere.
enum WakePlan {

    /// One thing to try, in order.
    enum Step: Sendable, Equatable {
        /// Send the packet from this device.
        case direct(broadcasts: [String], ports: [UInt16])
        /// Ask a machine to send it, by running a configured action there.
        case helper(WakeHelper, machineName: String)
    }

    /// Why a step is not available.
    struct Unavailable: Sendable, Equatable, Identifiable {
        var what: String
        var reason: String
        /// The machine that would have to be woken first, when that is the obstacle. Never acted on
        /// automatically: it is offered as a separate, explicit step.
        var wakeableFirst: String?

        var id: String { what }
    }

    struct Plan: Sendable, Equatable {
        var steps: [Step]
        var unavailable: [Unavailable]
        /// Where this device thinks it is, for the explanation line.
        var placement: SiteAwareness.Placement

        var isEmpty: Bool { steps.isEmpty }

        /// The sentence shown when there is nothing to try.
        var nothingToTry: String {
            guard steps.isEmpty else { return "" }
            if unavailable.isEmpty {
                return "There is no way to wake this machine from here: nothing on its network is configured to send the packet."
            }
            return "Nothing here can wake it right now. " + unavailable.map { "\($0.what): \($0.reason)" }
                .joined(separator: " ")
        }
    }

    /// What a helper machine looked like at the last reading, so a plan can skip one that is known
    /// to be off rather than spending a round trip finding out.
    struct HelperState: Sendable, Equatable {
        var isAwake: Bool
        var isCommandable: Bool
        var canBeWoken: Bool
        var lastCheckedAt: Date?
    }

    /// Build the plan.
    static func plan(
        for machine: Machine,
        in config: ControllerConfig,
        placement: SiteAwareness.Placement,
        addresses: [String],
        helperStates: [String: HelperState]
    ) -> Plan {
        guard let wake = machine.wake else {
            return Plan(steps: [], unavailable: [], placement: placement)
        }

        var steps: [Step] = []
        var unavailable: [Unavailable] = []

        // 1. Directly, when this device is on the machine's network. The site's own broadcast is the
        //    fallback for a machine that names none, which is the usual case once sites exist.
        let site = machine.site.flatMap { config.site(id: $0) }
        let onSite: Bool = {
            if let machineSite = machine.site { return placement.effective == machineSite }
            return SiteAwareness.matchesLANPrefix(wake.lanPrefix, addresses: addresses)
        }()

        if onSite {
            var broadcasts = wake.broadcast
            if broadcasts == ["255.255.255.255"], let site, !site.broadcast.isEmpty {
                broadcasts = site.broadcast
            }
            steps.append(.direct(broadcasts: broadcasts, ports: wake.ports))
        } else if placement.isAmbiguous {
            unavailable.append(Unavailable(
                what: "Sending it from here",
                reason: "this device's addresses match more than one of the configured sites, so a broadcast could go to the wrong network. Confirm which site this is to send it directly.",
                wakeableFirst: nil
            ))
        } else {
            let where_ = site?.name ?? machine.name
            unavailable.append(Unavailable(
                what: "Sending it from here",
                reason: "this device is not on \(where_)'s network, and a magic packet does not cross networks.",
                wakeableFirst: nil
            ))
        }

        // 2. Helpers, in the order the setup lists them.
        for helper in wake.orderedHelpers {
            guard let host = config.machine(id: helper.machine) else {
                unavailable.append(Unavailable(
                    what: helper.machine,
                    reason: "the setup lists it as a helper but has no such machine.",
                    wakeableFirst: nil
                ))
                continue
            }
            let state = helperStates[helper.machine]
            if let state, state.lastCheckedAt != nil, !state.isAwake {
                unavailable.append(Unavailable(
                    what: host.name,
                    reason: host.alwaysOn == true
                        ? "it is marked as always on but did not answer at the last reading."
                        : "it is asleep or off.",
                    // Offered, never taken. Waking a helper to wake something else is a cascade, and
                    // a cascade is how the machine you were trying not to run ends up running.
                    wakeableFirst: host.wake != nil ? host.name : nil
                ))
                continue
            }
            if let state, state.lastCheckedAt != nil, state.isAwake, !state.isCommandable {
                unavailable.append(Unavailable(
                    what: host.name,
                    reason: "it is awake but its control agent is not answering.",
                    wakeableFirst: nil
                ))
                continue
            }
            steps.append(.helper(helper, machineName: host.name))
        }

        if steps.isEmpty, wake.orderedHelpers.isEmpty, !onSite {
            unavailable.append(Unavailable(
                what: "A helper",
                reason: "no machine on \(site?.name ?? "that network") is configured to send the packet.",
                wakeableFirst: nil
            ))
        }

        return Plan(steps: steps, unavailable: unavailable, placement: placement)
    }

    /// Whether a helper failure is safe to move past.
    ///
    /// A `wol` action is a declared, idempotent UDP send: repeating it costs three more datagrams
    /// and nothing else, so an ambiguous failure may be followed by the next helper. Any other
    /// configured action may do anything at all, and an ambiguous outcome there has to be settled —
    /// by reading its operation record when the helper is reachable again — before something else
    /// is tried. Otherwise "the link dropped" turns one requested action into two performed ones.
    static func mayTryNextHelper(after failure: AgentFailure, actionIsDeclaredWakePacket: Bool) -> Bool {
        switch failure.dispatch {
        case .never:
            return true
        case .unknown, .acknowledged:
            return actionIsDeclaredWakePacket
        }
    }
}
