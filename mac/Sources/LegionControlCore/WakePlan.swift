import Foundation

/// How to wake one machine from where this device happens to be (section 10.2 of the contract).
///
/// A magic packet is a broadcast on a local network, so it only works from that network. One helper
/// in one house cannot wake a tower in another, whatever a config says. The plan is therefore
/// ordered: send it directly if this device is on the right network, then ask the machines the setup
/// lists as helpers, in its order. It is the port of `planWake` in `contract/tools/wake-plan.mjs`,
/// held to it by the shared wake vectors, so the Mac, the desktop and the phone make the same plan
/// from the same setup in the same place.
///
/// Three rules the plan never breaks. Nothing is ever woken in order to serve as a helper: a wake
/// cascade would turn an always-off tower into an always-on one by accident, which is the opposite
/// of what the user asked for. A command whose first run may still be going is never run a second
/// time somewhere else: while a wake hold exists, only a declared `wol` action runs. And when
/// nothing can be done, the app says so with a reason for each step rather than pretending the
/// packet went somewhere.
enum WakePlan {

    /// One thing to try, in order.
    enum Step: Sendable, Equatable {
        /// Send the packet from this device, `WakeOnLAN.repeats` times to every address and port.
        case direct(broadcasts: [String], ports: [UInt16])
        /// Ask a machine to send it, by running a configured action there. `readFirst` when the
        /// helper has not been read this session: it is read once just before its step.
        case helper(WakeHelper, machineName: String, readFirst: Bool)
    }

    /// What the last status read of a helper this session said, the only evidence a plan reads
    /// about it. Never an extra read: a plan is drawn on every poll of the section.
    struct HelperState: Sendable, Equatable {
        enum State: String, Sendable, Equatable {
            /// It answered. `actions` says which actions it offers and their kind.
            case ready
            /// The read failed with noRoute or hostUnreachable: asleep or off.
            case asleep
            /// The read failed in a way no wake can cure: a rejected key, a host key problem, no
            /// agent or no interpreter, a request it refused.
            case unusable
            /// None of its systems is approved on this device, so nothing is ever sent to it.
            case notDriven = "not-driven"
            /// Not read this session, or the read ended some other way.
            case unknown
        }

        var state: State
        /// Action id to kind (`wol`, `command`), when the status listed its actions.
        var actionKinds: [String: String]?
        /// The sentence that says why it cannot help, for the wake row.
        var note: String?

        init(_ state: State, actionKinds: [String: String]? = nil, note: String? = nil) {
            self.state = state
            self.actionKinds = actionKinds
            self.note = note
        }

        static let unknown = HelperState(.unknown)

        /// What a failed status read says about a helper.
        init(after failure: AgentFailure) {
            switch failure.kind {
            case .noRoute, .hostUnreachable:
                self.init(.asleep)
            case .authenticationFailed, .hostKeyChanged, .hostKeyUnknown, .interpreterMissing, .agentMissing, .badRequest:
                self.init(.unusable, note: "it cannot be driven from this device: \(failure.shortReason).")
            default:
                self.init(.unknown)
            }
        }

        /// The kind of each action a status lists, by id, the first of two with one id winning.
        static func kinds(_ actions: [AgentActionInfo]) -> [String: String] {
            Dictionary(actions.filter { !$0.id.isEmpty }.map { ($0.id, $0.kind ?? "") }, uniquingKeysWith: { first, _ in first })
        }
    }

    /// A wake hold this device keeps for the target: a command on `helper` whose run may still be
    /// going (section 10.2).
    struct Hold: Sendable, Equatable {
        var helper: String
        var action: String
    }

    /// Why the direct step, or one helper entry, is not a step. The words are the contract's.
    enum Reason: String, Sendable, Equatable {
        case noMac = "no-mac"
        case ambiguous
        case unplaced
        case elsewhere
        case missing
        case itself = "self"
        case duplicate
        case asleep
        case unusable
        case noAction = "no-action"
        case held
        case none
    }

    /// One entry that is not a step, and why, in the plan's own order.
    struct Skip: Sendable, Equatable, Identifiable {
        enum Kind: String, Sendable, Equatable {
            case direct, helper, helpers
        }

        var kind: Kind
        var machine: String?
        var action: String?
        var reason: Reason
        /// Who it is, for the wake row.
        var what: String
        /// Why, in a sentence.
        var why: String

        var id: String { "\(kind.rawValue)-\(machine ?? "")-\(action ?? "")-\(reason.rawValue)" }
    }

    struct Plan: Sendable, Equatable {
        var steps: [Step]
        var skipped: [Skip]
        /// The helpers skipped as asleep that have a wake block of their own. "Wake it first" is a
        /// separate press, never part of this plan.
        var offerWakeFirst: [String]
        /// Where this device thinks it is, for the explanation line.
        var placement: SiteAwareness.Placement

        static func empty(_ placement: SiteAwareness.Placement) -> Plan {
            Plan(steps: [], skipped: [], offerWakeFirst: [], placement: placement)
        }

        var isEmpty: Bool { steps.isEmpty }

        /// Whether a wake hold is what keeps a helper from running.
        var isHeld: Bool { skipped.contains { $0.reason == .held } }

        /// The sentence shown when there is nothing to try.
        var nothingToTry: String {
            guard steps.isEmpty else { return "" }
            if skipped.isEmpty {
                return "There is no way to wake this machine from here: nothing on its network is configured to send the packet."
            }
            return "Nothing here can wake it right now. " + skipped.map { "\($0.what): \($0.why)" }.joined(separator: " ")
        }
    }

    // MARK: - Building the plan

    /// The plan for waking `target` from this device.
    ///
    /// `helpers` holds each helper's last status read (a machine missing from it is `unknown`), and
    /// `holds` the wake holds this device recorded for the target.
    static func plan(
        for target: Machine,
        in config: ControllerConfig,
        placement: SiteAwareness.Placement,
        helpers: [String: HelperState],
        holds: [Hold]
    ) -> Plan {
        var steps: [Step] = []
        var skipped: [Skip] = []
        var offerWakeFirst: [String] = []
        let wake = target.wake
        let site = target.site.flatMap { config.site(id: $0) }
        let here = "Sending it from here"

        // 1. Directly, when this device is on the machine's network. Only a wake block with a
        //    hardware address is woken directly (section 9.7); one without is a setup another peer
        //    may well have written, and its helpers can still wake it.
        if let wake, wake.mac != nil {
            switch SiteAwareness.network(of: target, sites: config.allSites, placement: placement) {
            case .on(let placing):
                steps.append(.direct(broadcasts: broadcasts(for: wake, site: site, placing: placing), ports: ports(for: wake)))
            case .ambiguous:
                skipped.append(Skip(kind: .direct, reason: .ambiguous, what: here,
                                    why: "this device's addresses match more than one of the configured sites, so a broadcast could go to the wrong network. Confirm which site this is to send it directly."))
            case .unplaced:
                skipped.append(Skip(kind: .direct, reason: .unplaced, what: here,
                                    why: "the setup does not say which network \(target.name) is on: it names no site and no LAN prefix this device can use."))
            case .elsewhere:
                skipped.append(Skip(kind: .direct, reason: .elsewhere, what: here,
                                    why: "this device is not on \(site?.name ?? target.name)'s network, and a magic packet does not cross networks."))
            }
        } else {
            skipped.append(Skip(kind: .direct, reason: .noMac, what: here,
                                why: "the setup names no hardware address for \(target.name), so no magic packet can be sent to it. Only its helpers can wake it."))
        }

        // 2. Helpers, in the order the setup lists them.
        let entries = wake?.orderedHelpers ?? []
        for (index, entry) in entries.enumerated() {
            let state = helpers[entry.machine] ?? .unknown
            guard let reason = skipReason(for: entry, target: target, earlier: entries[..<index], in: config,
                                          state: state, holds: holds) else {
                let name = config.machine(id: entry.machine)?.name ?? entry.machine
                steps.append(.helper(entry, machineName: name, readFirst: state.state == .unknown))
                continue
            }
            skipped.append(skip(entry, reason, target: target, in: config, state: state))
            if reason == .asleep, config.machine(id: entry.machine)?.wake != nil, !offerWakeFirst.contains(entry.machine) {
                offerWakeFirst.append(entry.machine)
            }
        }

        // 3. Nothing configured at all.
        if entries.isEmpty, !steps.contains(where: { if case .direct = $0 { true } else { false } }) {
            skipped.append(Skip(kind: .helpers, reason: .none, what: "A helper",
                                why: "no machine on \(site?.name ?? "that network") is configured to send the packet."))
        }

        return Plan(steps: steps, skipped: skipped, offerWakeFirst: offerWakeFirst, placement: placement)
    }

    /// Why a helper entry is not a step, checked in the contract's order, or nil when it is one.
    ///
    /// Everything after `duplicate` comes from the helper's last status read. An unread helper is
    /// read once just before its step, so only what needs no status can skip it, and a hold, which
    /// no unread helper can be shown to be exempt from. The walk asks again just before each step,
    /// with what it has learnt since: a helper it has just read, and a hold the step before it made.
    static func skipReason(for entry: WakeHelper, target: Machine, earlier: ArraySlice<WakeHelper>,
                           in config: ControllerConfig, state: HelperState, holds: [Hold]) -> Reason? {
        if config.machine(id: entry.machine) == nil { return .missing }
        if entry.machine == target.id { return .itself }
        if earlier.contains(entry) { return .duplicate }
        switch state.state {
        case .asleep: return .asleep
        case .unusable, .notDriven: return .unusable
        case .ready, .unknown: break
        }
        let kinds = state.state == .ready ? state.actionKinds : nil
        if let kinds, kinds[entry.action] == nil { return .noAction }
        // While any hold for the target exists, only a declared wol action runs, the held helper's
        // included: a command whose first run may still be going must not get a second one
        // somewhere else.
        if !holds.isEmpty, kinds?[entry.action] != "wol" { return .held }
        return nil
    }

    /// One skipped helper entry, in words.
    static func skip(_ entry: WakeHelper, _ reason: Reason, target: Machine, in config: ControllerConfig,
                     state: HelperState) -> Skip {
        let helper = config.machine(id: entry.machine)
        let name = helper?.name ?? entry.machine
        let why: String
        switch reason {
        case .missing:
            why = "the setup lists it as a helper but has no such machine."
        case .itself:
            why = "it is \(target.name) itself, so it cannot send the packet."
        case .duplicate:
            why = "it is listed more than once with \(entry.action), and is asked only once."
        case .asleep:
            why = helper?.alwaysOn == true
                ? "it is marked as always on but did not answer at the last reading."
                : "it is asleep or off."
        case .unusable:
            why = state.note ?? "it cannot be driven from this device."
        case .noAction:
            why = "its agent offers no action called \(entry.action)."
        case .held:
            why = "a helper may still be running a command to wake \(target.name), so no other command is run for it until that is settled."
        case .noMac, .ambiguous, .unplaced, .elsewhere, .none:
            why = ""
        }
        return Skip(kind: .helper, machine: entry.machine, action: entry.action, reason: reason, what: name, why: why)
    }

    // MARK: - Going past a hold

    /// What a person chose to do about the wake holds on a target. Only a person ever chooses
    /// anything but `stand`: a hold is what keeps a command from running twice, and every other
    /// release is evidence (section 10.2).
    enum HoldChoice: Sendable, Equatable {
        /// The holds stand, and the wake walks around them.
        case stand
        /// Ask the helpers a hold keeps back other than the held ones. The held commands are not
        /// run again in that walk, but their holds are released, so the next wake asks every helper.
        case otherHelpers
        /// With no other helper to ask, run the held commands again, knowing the first run may still
        /// be going. A helper that keeps no records would otherwise hold until the target answers.
        case runAgain
    }

    /// `plan` without the helper steps `holds` are on: what a person asking for the other helpers
    /// walks. Releasing a hold is not a reason to run its own command a second time while the first
    /// run may still be going.
    static func without(_ plan: Plan, holds: [Hold], target: Machine) -> Plan {
        guard !holds.isEmpty else { return plan }
        var result = plan
        result.steps = []
        for step in plan.steps {
            if case .helper(let helper, let name, _) = step, holds.contains(Hold(helper: helper.machine, action: helper.action)) {
                result.skipped.append(Skip(kind: .helper, machine: helper.machine, action: helper.action, reason: .held, what: name,
                                           why: "its \(helper.action) was run to wake \(target.name) earlier and may still be going, so only the other helpers are asked."))
            } else {
                result.steps.append(step)
            }
        }
        return result
    }

    /// What trying the other helpers walks: `unheld`, the plan as if nothing were held, without the
    /// helper steps of `holds`. Nil unless that asks a helper `held`, the plan as it stands, skips
    /// for a hold. With the held helper the only one, going past its hold would ask nobody new, and
    /// the next ordinary press would run the held command a second time.
    static func otherHelpers(held: Plan, unheld: Plan, holds: [Hold], target: Machine) -> Plan? {
        guard !holds.isEmpty else { return nil }
        let others = without(unheld, holds: holds, target: target)
        let heldBack = held.skipped.filter { $0.reason == .held }.map { Hold(helper: $0.machine ?? "", action: $0.action ?? "") }
        let asksOne = others.steps.contains { step in
            guard case .helper(let helper, _, _) = step else { return false }
            return heldBack.contains(Hold(helper: helper.machine, action: helper.action))
        }
        return asksOne ? others : nil
    }

    /// What running the held commands again walks: `unheld`, the plan as if nothing were held. Nil
    /// unless `held` skips a helper for a hold and no other helper could be asked instead: with one,
    /// asking it is the way past, and that frees the held command for the next press without
    /// running it twice now.
    static func runAgain(held: Plan, unheld: Plan, holds: [Hold], target: Machine) -> Plan? {
        guard held.isHeld, otherHelpers(held: held, unheld: unheld, holds: holds, target: target) == nil else { return nil }
        return unheld
    }

    /// The helpers whose command `plan` runs a second time while the first run may still be going,
    /// in plan order.
    static func runsAgain(_ plan: Plan, holds: [Hold]) -> [String] {
        var names: [String] = []
        for case .helper(let helper, let name, _) in plan.steps
        where holds.contains(Hold(helper: helper.machine, action: helper.action)) && !names.contains(name) {
            names.append(name)
        }
        return names
    }

    /// What to say when a person asks for the other helpers and there is none to ask.
    static func noOtherHelper(_ targetName: String) -> String {
        "No other helper could be asked to wake \(targetName). The helper command run for it earlier may still be going, so it is not run again, and its hold stands."
    }

    /// What to say when a person asks to run a held command again while another helper could be
    /// asked instead.
    static func otherHelperFirst(_ targetName: String) -> String {
        "Another helper could be asked to wake \(targetName), so the command held for it is not run again. Trying the other helpers releases its hold, and the next wake asks every helper."
    }

    /// What running the held commands of a plan is called where a person chooses it.
    static func runAgainLabel(_ helpers: [String]) -> String {
        helpers.count == 1 ? "Run \(helpers[0])'s command again" : "Run the held commands again"
    }

    /// What a person is told before running the held commands again: that a run may still be going,
    /// and that this can make it two.
    static func runAgainWarning(_ helpers: [String], target targetName: String) -> String {
        helpers.count == 1
            ? "\(helpers[0])'s command to wake \(targetName) has not reported how it ended, so it may still be running, and no other helper could be asked instead. Running it again can run it a second time beside the first."
            : "The commands run to wake \(targetName) have not reported how they ended, so they may still be running, and no other helper could be asked instead. Running them again can run each a second time beside the first."
    }

    /// Where direct wake sends: the dotted entries of `wake.broadcast`; else those of the machine's
    /// site; else the directed broadcast of the placing address, when its length is known and at
    /// most 30; else 255.255.255.255. A device that guesses the limited broadcast first wakes
    /// nothing behind a router that drops it, which is why a site's own broadcast comes before it.
    /// Nothing else is ever sent to, and nothing is resolved as a name.
    static func broadcasts(for wake: WakeConfig, site: Site?, placing: LocalAddress?) -> [String] {
        let listed = wake.broadcast.filter { LanPrefix.ipv4($0) != nil }
        if !listed.isEmpty { return listed }
        let shared = (site?.broadcast ?? []).filter { LanPrefix.ipv4($0) != nil }
        if !shared.isEmpty { return shared }
        return [placing?.directedBroadcast ?? limitedBroadcast]
    }

    static let limitedBroadcast = "255.255.255.255"
    /// The ports direct wake sends to when the wake block names none it can use.
    static let defaultPorts: [UInt16] = [9, 7]

    /// The ports of a wake block from 1 to 65535, or 9 and 7 when it names none of those.
    static func ports(for wake: WakeConfig) -> [UInt16] {
        let ports = wake.ports.filter { $0 >= 1 }
        return ports.isEmpty ? defaultPorts : ports
    }

    // MARK: - Readiness

    /// How long after a send the target is waited for.
    static let readinessWindowMs = 120_000

    /// When the readiness polls start, in milliseconds after a send: the first after 5 s, then at
    /// intervals growing by 8/5 up to 20 s, for as long as a poll starts within 120 s. A step that
    /// sent nothing waits for nothing.
    static func readinessSchedule(sentAnything: Bool) -> [Int] {
        guard sentAnything else { return [] }
        var starts: [Int] = []
        var wait = 5_000
        var at = 5_000
        while at <= readinessWindowMs {
            starts.append(at)
            wait = min(wait * 8 / 5, 20_000)
            at += wait
        }
        return starts
    }
}

/// The clock the readiness polls are timed on.
struct WakeClock {
    var now: @MainActor () -> Date
    var sleep: @MainActor (TimeInterval) async -> Void

    static var system: WakeClock {
        WakeClock(now: { Date() }, sleep: { try? await Task.sleep(for: .seconds($0)) })
    }
}
