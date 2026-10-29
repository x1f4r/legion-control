import Foundation
import Observation

/// One configured machine: what it is running, what its services are doing, and everything that can
/// be done to it.
///
/// There is one of these per machine in the config, each with its own link state and its own poll.
/// Nothing about one machine is ever read off another. What it shares with the local Mac — how an
/// action is sent and written down, how the setup is shared, every verb in between — is in
/// AgentDevice.swift; this file is the part only a machine over ssh has: routes, host keys, the
/// link, and waking, sleeping and booting something that is not the device the app runs on.
@MainActor
@Observable
final class MachineModel: AgentDevice, @MainActor Identifiable {
    enum Link: Sendable, Equatable {
        case unknown
        case online(SystemDescriptor)
        /// ssh got through but the agent is not there. The machine is awake, it just cannot be driven.
        case reachableWithoutAgent(SystemConfig)
        /// Not answering. The failure says which of the several very different reasons it is.
        case offline(AgentFailure)
    }

    // MARK: - Observable state

    private(set) var machine: Machine
    private(set) var link: Link = .unknown
    private(set) var status: AgentStatus?
    private(set) var lastChecked: Date?
    private(set) var isRefreshing = false
    /// The route the last successful call went over, for the diagnosis page.
    private(set) var lastRoute: MachineRoute?
    /// A host key this machine offered that is not in known_hosts. Nothing is ever written without
    /// the user saying so, so this sits here until they do.
    private(set) var hostKeyPrompt: HostKeyPrompt?

    let core = DeviceCore()

    /// The last thing known about each of this machine's systems, in the one store every machine
    /// shares.
    let remembered: RememberedSettings

    var id: String { machine.id }
    var name: String {
        guard let binding = bindings?(), binding.isSelf(machine.id) else { return machine.name }
        return binding.effectiveDeviceName
    }

    var historyMachineId: String { machine.id }

    /// The window has one status bar for the whole app, so with more than one machine it shows
    /// whichever of them spoke last.
    var statusLine: String { core.line ?? "Ready." }
    var statusIsError: Bool { core.lineIsError }
    var statusDetail: String? { core.lineDetail }
    var statusStamp: Date { core.lineStamp }

    /// This device's private settings: which key to offer, which alias to dial, whether this
    /// machine is in fact this device.
    var bindings: (@MainActor () -> Bindings)?
    /// The whole setup, for the parts that are about more than one machine: wake helpers and sites.
    var setup: (@MainActor () -> ControllerConfig?)?
    /// How to reach another machine, for running a wake action on a helper.
    var peer: (@MainActor (String) -> MachineModel?)?
    var localPeer: (@MainActor (String) -> MacModel?)?
    var migrateAgentCommand: (@MainActor (String, [String]) -> Void)?
    /// Where this device thinks it is.
    var placement: (@MainActor () -> SiteAwareness.Placement)?

    private var agent: RemoteAgent
    /// Set only after the agent has acknowledged a reboot in JSON, and cleared the moment the target
    /// is observed running. A dropped link or a timeout never sets it: neither is evidence that
    /// anything happened.
    private var rebootExpectation: (target: SystemConfig, until: Date)?
    /// The same, for a suspend the agent acknowledged.
    private var sleepExpectation: Date?
    private var lastRefreshFinished: Date?

    init(machine: Machine, remembered: RememberedSettings) {
        self.machine = machine
        self.agent = RemoteAgent(machine: machine)
        self.remembered = remembered
    }

    /// A model with its transport handed in. The app passes the one its factory builds for the
    /// machine; the tests pass one that never launches a process.
    init(machine: Machine, agent: RemoteAgent, remembered: RememberedSettings = .inMemory()) {
        self.machine = machine
        self.agent = agent
        self.remembered = remembered
    }

    /// Follow an edit to this machine's entry in the setup without starting over.
    ///
    /// The app used to build a new model whenever the entry changed and throw this one away, with
    /// its link state, its last reading and anything in flight. The action carried on in the
    /// discarded model, out of sight, while the new one let the user start another on the same
    /// machine. The model now stays; only what the setup says about the machine changes, and the
    /// next poll reads it as it is.
    func update(machine new: Machine) {
        guard new != machine else { return }
        machine = new
        agent.machine = new

        // What this model remembers about the machine is re-read against the new entry, so nothing
        // is commanded with a launcher or an address the setup no longer has.
        lastRoute = lastRoute.flatMap(currentRoute)
        switch link {
        case .online(let system):
            if let configured = new.system(id: system.id) {
                link = .online(SystemDescriptor(configured))
            } else if system.configured != nil {
                link = .online(SystemDescriptor(unknownId: system.id, platform: status?.platform))
            }
        case .reachableWithoutAgent(let system):
            if let configured = new.system(id: system.id) { link = .reachableWithoutAgent(configured) }
        case .unknown, .offline:
            break
        }
        if let expectation = rebootExpectation, let target = new.system(id: expectation.target.id) {
            rebootExpectation = (target, expectation.until)
        }
        // A host key belongs to one address. A question about an address the entry has dropped is
        // a question about a machine this device no longer dials, and answering it would pin a key
        // for nothing. If the new address has the same problem, the next poll asks again.
        if let prompt = hostKeyPrompt, !(prompt.target.map(isCurrentAddress) ?? false) {
            clearHostKeyPrompt()
        }
        onStateChange?()
    }

    /// The route with this one's id as the entry has it now, or nil when the entry no longer has it.
    ///
    /// A route is remembered by id and dialled by its target, so a route kept across an edit has to
    /// be looked up again: the id can stay while the address behind it changes.
    private func currentRoute(_ route: MachineRoute) -> MachineRoute? {
        machine.routes(bindings: agent.bindings).first { $0.id == route.id }
    }

    /// Whether the entry still reaches this machine at that host and port, under any route.
    private func isCurrentAddress(_ target: SSHTarget) -> Bool {
        machine.routes(bindings: agent.bindings).contains {
            $0.target.host == target.host && ($0.target.port ?? 22) == (target.port ?? 22)
        }
    }

    /// How long an unreachable machine still reads as "asleep" rather than as a problem. The phone
    /// uses the same ten minutes, and the two apps have to say the same thing about the same machine.
    private static let sleepGrace: TimeInterval = 600

    /// True only while the wake packet is in flight.
    ///
    /// A flag of its own rather than a reading of the activity line. The line names the machine as
    /// it was called when the wake began, and an edit to the entry, a new name included, lands on
    /// this model while the wake is still running.
    private(set) var isWaking = false

    /// The system we can actually command. Everything that talks to the agent keys off this.
    var currentSystem: SystemDescriptor? {
        if case .online(let system) = link { return system }
        return nil
    }

    var commandableSystem: SystemConfig? { currentSystem?.configured }

    /// The system that is awake, agent or no agent. Only the wake button cares about the difference.
    var reachableSystem: SystemDescriptor? {
        switch link {
        case .online(let system): system
        case .reachableWithoutAgent(let system): SystemDescriptor(system)
        case .unknown, .offline: nil
        }
    }

    var isAwake: Bool { reachableSystem != nil }

    var canWake: Bool { machine.wake != nil }

    var rebootInProgress: (target: SystemConfig, until: Date)? {
        guard let rebootExpectation, rebootExpectation.until > Date() else { return nil }
        return rebootExpectation
    }

    /// Whether it is safe to assume the machine is idle. An unknown or unmonitored busy state is not
    /// idle: it is an absence of evidence, and disruptive work waits for evidence.
    var isKnownIdle: Bool {
        guard let busy = status?.busy else { return false }
        return busy.verdict == .idle
    }

    /// Where the machine can be booted, drawn from the config and filtered by what the agent offers.
    var bootTargets: [SystemConfig] {
        let running = currentSystem?.id
        let others = machine.systems.filter { $0.id != running }
        guard let reported = status?.bootTargets else { return others }
        let ids = Set(reported.map(\.id))
        return others.filter { ids.contains($0.id) }
    }

    func autoUpdateValue(for system: SystemConfig) -> Bool? {
        if currentSystem?.id == system.id, let live = status?.autoUpdate { return live }
        return remembered.system(machine: machine.id, system: system.id)?.autoUpdate
    }

    func autoUpdateCheckedAt(for system: SystemConfig) -> Date? {
        remembered.system(machine: machine.id, system: system.id)?.checkedAt
    }

    // MARK: - Reaching the agent

    var isCommandable: Bool { commandableSystem != nil }

    var recordedSystem: (id: String?, name: String?) { (commandableSystem?.id, currentSystem?.name) }

    var recordedRoute: String? { lastRoute?.label }

    func send<Value: Decodable & Sendable>(_ request: AgentRequest, decoding: Value.Type) async throws -> Value {
        try await agent.send(request, decoding: Value.self, preferring: commandableSystem, route: lastRoute).value
    }

    func refreshStatus(duringAction: Bool) async {
        await refresh(userInitiated: false, duringAction: duringAction)
    }

    /// An address that did not answer is otherwise left alone for a minute, which is right for a
    /// machine that is asleep and wrong for one that is expected back any moment.
    func allowRedial() {
        for route in machine.routes(bindings: agent.bindings) { agent.backoff.clear(route.target) }
    }

    func describe(_ failure: AgentFailure) -> String {
        failure.message(machine: machine.name)
    }

    // MARK: - How the state reads at a glance

    var symbolName: String {
        if rebootInProgress != nil, !isAwake { return "arrow.triangle.2.circlepath" }
        if !unresolvedOperations.isEmpty { return "questionmark.circle" }
        switch link {
        case .online(let system): return system.symbolName
        case .reachableWithoutAgent: return "exclamationmark.triangle"
        case .offline(let failure): return failure.meansAsleepOrOff ? "moon.zzz" : "exclamationmark.triangle"
        case .unknown: return "circle.dotted"
        }
    }

    var stateDescription: String {
        if let reboot = rebootInProgress, !isAwake { return "Restarting into \(reboot.target.name)" }
        switch link {
        case .online(let system): return "Running \(system.name)"
        case .reachableWithoutAgent(let system): return "\(system.name) is awake, the control agent is not installed"
        case .offline(let failure): return failure.meansAsleepOrOff ? "Asleep or unreachable" : failure.message(machine: machine.name)
        case .unknown: return "Not checked yet"
        }
    }

    /// The state of the machine's row in the sidebar, which VoiceOver reads as the row's value
    /// (SidebarRow.summary).
    var sidebarSummary: String {
        if let reboot = rebootInProgress, !isAwake { return "restarting into \(reboot.target.name)" }
        if !unresolvedOperations.isEmpty { return "outcome not known yet" }
        switch link {
        case .unknown: return "checking"
        case .offline(let failure): return failure.meansAsleepOrOff ? "asleep or unreachable" : failure.shortReason
        case .reachableWithoutAgent(let system): return "\(system.name), no agent"
        case .online(let system): return needsAgentUpgrade ? "\(system.name), agent too old" : system.name
        }
    }

    /// The state of a service's row in the sidebar, which VoiceOver reads as the row's value
    /// (SidebarRow.summary).
    func sidebarSummary(for service: ServiceStatus) -> String {
        guard currentSystem != nil else { return "not readable" }
        if service.installed == nil { return "not installed" }
        if service.hasStagedUpdate { return "update waiting" }
        if service.upToDate == false { return "update available" }
        if let busy = service.busy {
            switch busy.verdict {
            case .busy: return busy.summary
            case .unknown: return "busy state unknown"
            case .unmonitored: return "not monitored"
            case .idle: break
            }
        }
        if service.healthy != true && service.running != nil { return "not answering" }
        if service.upToDate == true { return "up to date" }
        return "version not checked"
    }

    // MARK: - Reading

    func refreshIfNeeded(minimumAge: TimeInterval) async {
        if let lastRefreshFinished, Date().timeIntervalSince(lastRefreshFinished) < minimumAge { return }
        await refresh(userInitiated: false)
    }

    /// Bring the transport up to date with this device's private settings and where it thinks it is,
    /// before anything is dialled.
    private func prepareTransport() {
        agent.bindings = bindings?() ?? .empty
        agent.preferLAN = isOnMachineNetwork
    }

    /// Whether this device believes it is on this machine's own network. A hint that decides which
    /// addresses are worth dialling first, and nothing else: which machine actually answered is
    /// settled by the pinned host key, on every connection.
    var isOnMachineNetwork: Bool {
        guard let placement = placement?() else { return false }
        return SiteAwareness.network(of: machine, sites: setup?()?.allSites ?? [], placement: placement).isOn
    }

    func refresh(userInitiated: Bool, duringAction: Bool = false) async {
        guard !isRefreshing, duringAction || activity == nil else { return }
        prepareTransport()
        isRefreshing = true
        defer {
            isRefreshing = false
            lastRefreshFinished = Date()
            onStateChange?()
        }

        if userInitiated { setStatus("Checking \(machine.name).") }
        let asked = Date()

        do {
            let reply = try await agent.status(dialect: dialect, preferring: rebootInProgress?.target ?? commandableSystem, route: lastRoute)
            guard reply.value.ok != false else {
                handleStatusFailure(AgentFailure(
                    .agentFailed,
                    system: reply.system,
                    route: reply.route.label,
                    detail: reply.value.message ?? "the agent reported a failure",
                    dispatch: reply.value.reasonCode == "restricted" ? .never : .acknowledged
                ))
                lastChecked = Date()
                return
            }
            hostKeyPrompt = nil
            apply(reply.value, fallback: reply.system)
            // The machine answered, so whatever a helper's command recorded before this read was
            // doing, it is up: every wake hold for it from before the read ends.
            operations?.releaseWakeHolds(target: machine.id, before: asked, reason: "\(machine.name) answered")
            // The entry may have been edited while the read was out, and the reply names the route
            // as the entry had it then. Kept as it is, it would put an address back that the edit
            // took away, and the legacy agent install uploads to exactly that address.
            lastRoute = currentRoute(reply.route)
            lastChecked = Date()
            if !userInitiated, core.lineIsRefreshFailure {
                if let outcome = lastActionOutcome {
                    setStatus(outcome.text, isError: outcome.isError,
                              detail: outcome.operationId.flatMap { operations?.record(id: $0)?.detail })
                } else {
                    setStatus("Status refreshed.")
                }
            }
            if userInitiated {
                setStatus(reply.value.isPartial
                    ? "\(machine.name) answered with what it had: it ran out of its own time budget before it had checked everything."
                    : "Status refreshed.")
            }
            reconcileSetup()
            reconcileIfNeeded()
        } catch is CancellationError {
            // Nobody is waiting for this read any more, and it learnt nothing about the machine,
            // so the machine keeps reading as it did.
        } catch let failure as AgentFailure {
            handleStatusFailure(failure)
            lastChecked = Date()
        } catch {
            handleStatusFailure(AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown))
            lastChecked = Date()
        }
    }

    private func apply(_ newStatus: AgentStatus, fallback: SystemConfig) {
        let system = SystemDescriptor.resolve(newStatus, on: machine, fallback: fallback)
        status = newStatus

        // The machine answered, which settles the sleep expectation outright. The reboot expectation
        // is only settled by seeing the target actually running: a machine that came back into the
        // system it started in did not do what was asked, and saying it did would be the same lie
        // the timeout path used to tell.
        sleepExpectation = nil
        if let expectation = rebootExpectation {
            if system.id == expectation.target.id {
                rebootExpectation = nil
            } else if expectation.until <= Date() {
                rebootExpectation = nil
                setStatus("\(machine.name) came back running \(system.name), not \(expectation.target.name). The switch did not take.",
                          isError: true)
            }
        }

        // A machine that has come back as something else is reachable at different addresses: the
        // endpoint that only answers while Linux is up says nothing about Windows. Forgetting the
        // remembered route makes the next call re-choose from the endpoints hinted at the system
        // that is actually running.
        if case .online(let previous) = link, previous.id != system.id { lastRoute = nil }
        link = .online(system)

        guard let configured = system.configured else { return }
        remembered.record(machine: machine.id, system: configured.id, autoUpdate: newStatus.autoUpdate)
    }

    private func handleStatusFailure(_ failure: AgentFailure) {
        // The failure names the system and the address as the entry had them when the read went
        // out, which is not always the entry as it is now. Both are taken only while the entry
        // still has them.
        switch failure.kind {
        case .agentMissing, .interpreterMissing:
            status = nil
            if let system = failure.system.flatMap({ machine.system(id: $0.id) }) {
                link = .reachableWithoutAgent(system)
                setStatus("The control agent is missing on \(system.name). Run the installer there, or use Install agent below.",
                          isError: true, detail: failure.detailText)
            } else {
                link = .offline(failure)
                setStatus(failure.message(machine: machine.name), isError: true, detail: failure.detailText)
            }
        case .hostKeyUnknown:
            status = nil
            link = .offline(failure)
            // Nothing is written to known_hosts here. Accepting a key is a trust decision and it gets
            // its own question, with the fingerprint in it.
            askAboutHostKey(failure)
            setStatus(failure.message(machine: machine.name), isError: true, detail: failure.detailText)
        case .hostKeyChanged:
            status = nil
            link = .offline(failure)
            askAboutHostKey(failure)
            setStatus(failure.message(machine: machine.name), isError: true, detail: failure.detailText)
        default:
            status = nil
            link = .offline(failure)
            if let reboot = rebootInProgress {
                setStatus("Waiting for \(reboot.target.name) to come back.", detail: failure.detailText)
            } else if let sleepExpectation, sleepExpectation > Date(), failure.meansAsleepOrOff {
                setStatus(canWake
                    ? "\(machine.name) is asleep. Wake brings it back."
                    : "\(machine.name) is asleep.")
            } else if failure.meansAsleepOrOff {
                setStatus("\(machine.name) is asleep or unreachable.", isError: true, detail: failure.detailText)
            } else {
                // Everything else is a real diagnosis and must not be dressed up as a sleeping
                // machine: a rejected key and a stalled agent are both things only a person can fix.
                setStatus(failure.message(machine: machine.name), isError: true, detail: failure.detailText)
            }
        }
        core.markLineAsRefreshFailure()
    }

    // MARK: - Host keys

    /// An unknown host key, waiting for a decision. Never acted on without one.
    struct HostKeyPrompt: Sendable {
        var machine: String
        var target: SSHTarget?
        var detail: String
        var fingerprints: [String] = []
        var problem: String?
        var context: HostIdentityContext?
        var offered: [HostPublicKey] = []
        var revision = 0
        var groups: [HostIdentityDocument.System] = []
        var unassigned: [HostPublicKey] = []
        var initialSetup = false
        var canApprove = false
    }

    var selectedHostSystem = ""
    var confirmedHostSystems: Set<String> = []
    var legacyHostAssignments: [String: String] = [:]
    var hostSystemsConfirmed = false

    /// Raise the question about the key a status read met, unless the same question is already up.
    /// A read that went out before an edit can come back about an address the entry has since
    /// dropped, and that one gets no question at all.
    private func askAboutHostKey(_ failure: AgentFailure) {
        let target = failure.target ?? machine.sshTarget
        if let target, !isCurrentAddress(target) { return }
        if hostKeyPrompt?.target != target || hostKeyPrompt == nil {
            hostKeyPrompt = HostKeyPrompt(machine: machine.name, target: target, detail: failure.detail)
        }
    }

    /// Drop the question along with every answer given to it so far, so none of them can be applied
    /// to a different one.
    private func clearHostKeyPrompt() {
        hostKeyPrompt = nil
        selectedHostSystem = ""
        confirmedHostSystems = []
        legacyHostAssignments = [:]
        hostSystemsConfirmed = false
    }

    func readHostKeyFingerprint() {
        guard let prompt = hostKeyPrompt, let target = prompt.target else { return }
        hostSystemsConfirmed = false
        Task { @MainActor in
            do {
                let context = try await HostIdentityContext.resolve(target)
                let scan = await Shell.run(executable: "/usr/bin/ssh-keyscan", arguments: context.keyscanArguments, timeout: 20)
                guard scan.succeeded else { throw HostIdentityFailure("The offered keys could not be read. \(scan.failureText)") }
                let offered = try HostPublicKey.parse(scan.standardOutput, uniqueAlgorithms: true)
                guard !offered.isEmpty else { throw HostIdentityFailure("The server offered no inspectable keys.") }
                let document = try HostIdentityStore().load()
                let groups = document.endpoints.first { $0.address == context.address }?.systems ?? []
                let tracked = Set(groups.flatMap(\.keys))
                let unassigned = context.existing.filter { !tracked.contains($0) }
                let initial = groups.isEmpty || !unassigned.isEmpty
                let suggestions = initial ? machine.systems.map { HostIdentityDocument.System(id: $0.id, name: $0.name) } : groups
                let eligible = initial ? suggestions : groups.filter { $0.keys.isEmpty || $0.keys == offered }
                guard self.hostKeyPrompt?.target == target else { return }
                self.selectedHostSystem = eligible.first?.id ?? ""
                self.confirmedHostSystems = Set(suggestions.map(\.id))
                self.legacyHostAssignments = [:]
                self.hostKeyPrompt = HostKeyPrompt(machine: machine.name, target: target, detail: prompt.detail,
                                                    fingerprints: offered.map(\.fingerprint),
                                                    problem: eligible.isEmpty ? "Every locally confirmed operating system already has keys. This changed key was refused. Inspect a deliberate reinstall or rotation in your SSH trust settings; existing pins were retained." : nil,
                                                    context: context, offered: offered, revision: document.revision,
                                                    groups: suggestions, unassigned: unassigned,
                                                    initialSetup: initial, canApprove: !eligible.isEmpty)
            } catch {
                self.hostKeyPrompt?.problem = error.localizedDescription
                self.hostKeyPrompt?.canApprove = false
            }
            self.onStateChange?()
        }
    }

    func trustHostKey() {
        guard let prompt = hostKeyPrompt, prompt.canApprove, let context = prompt.context,
              !prompt.initialSetup || hostSystemsConfirmed else { return }
        do {
            try HostIdentityStore().approve(context: context, expectedRevision: prompt.revision,
                                            systems: prompt.groups.filter { confirmedHostSystems.contains($0.id) }.map {
                                                .init(id: $0.id, name: $0.name)
                                            }, selected: selectedHostSystem, offered: prompt.offered,
                                            legacyAssignments: legacyHostAssignments)
            if let target = prompt.target { agent.backoff.clear(target) }
            hostKeyPrompt = nil
            setStatus("The displayed host keys for \(machine.name) were approved for \(selectedHostSystem).")
            Task { @MainActor in await self.refresh(userInitiated: true) }
        } catch {
            hostKeyPrompt?.problem = error.localizedDescription
        }
        onStateChange?()
    }

    func dismissHostKeyPrompt() {
        hostKeyPrompt = nil
        onStateChange?()
    }

    // MARK: - Power

    /// What this machine's last status read this session says about it as a wake helper, the only
    /// evidence a wake plan reads about a helper (section 10.2). Never an extra read.
    var wakeHelperState: WakePlan.HelperState {
        if !machine.systems.isEmpty, machine.systems.allSatisfy({ $0.launchState == .notDriven }) {
            return .init(.notDriven, note: "none of its systems is approved on this device, so nothing is sent to it.")
        }
        switch link {
        case .unknown:
            return .unknown
        case .online:
            return .init(.ready, actionKinds: status?.actions.map(WakePlan.HelperState.kinds))
        case .reachableWithoutAgent:
            return .init(.unusable, note: "it is awake but its control agent is not answering.")
        case .offline(let failure):
            return WakePlan.HelperState(after: failure)
        }
    }

    /// The wake holds this device keeps for this machine, as the plan reads them.
    var wakeHolds: [WakePlan.Hold] {
        (operations?.wakeHolds(target: machine.id) ?? []).map { WakePlan.Hold(helper: $0.machineId, action: $0.subject) }
    }

    /// What waking this machine would do from where this device is, and what stands in the way.
    var wakePlan: WakePlan.Plan {
        plan(from: placement?() ?? .nowhere)
    }

    /// Whether pressing Wake can do anything: there is a step to take, or a hold that the press
    /// reads again before it plans, which may well leave a step.
    var canTryWaking: Bool { !wakePlan.isEmpty || !wakeHolds.isEmpty }

    private func plan(from here: SiteAwareness.Placement) -> WakePlan.Plan {
        guard let setup = setup?() else { return .empty(here) }
        var states: [String: WakePlan.HelperState] = [:]
        for helper in machine.wake?.orderedHelpers ?? [] {
            states[helper.machine] = helperState(helper.machine)
        }
        return WakePlan.plan(for: machine, in: setup, placement: here, helpers: states, holds: wakeHolds)
    }

    /// A helper's state, from whichever model drives it here. A machine this device has no model
    /// for has never been read.
    private func helperState(_ id: String) -> WakePlan.HelperState {
        if let local = localPeer?(id) { return local.wakeHelperState }
        return peer?(id)?.wakeHelperState ?? .unknown
    }

    /// The device that drives a helper here, the local Mac included.
    private func helperDevice(_ id: String) -> (any AgentDevice)? {
        if let local = localPeer?(id) { return local }
        return peer?(id)
    }

    /// Where this device is right now, read from the interfaces again rather than from the last poll:
    /// a laptop that has just joined another network wakes from where it is.
    var freshPlacement: (@MainActor () -> SiteAwareness.Placement)?

    /// How the direct step sends its packets. The tests hand in one that writes down what it was
    /// asked to send and sends nothing.
    @ObservationIgnored var sendWakePackets: @MainActor (WakeOnLAN.Packets) async -> String? = { await WakeOnLAN.sendMagicPackets($0) }

    /// The clock the readiness polls are timed on. The tests hand in one that moves only when a
    /// poll waits.
    @ObservationIgnored var wakeClock = WakeClock.system

    /// Wake the machine: from here when this device is on its network, and through its helpers.
    ///
    /// The helpers are tried in the order the setup lists them and no further than that. Nothing is
    /// ever woken in order to become a helper: a cascade would turn the machine the user is trying
    /// not to run into one that runs whenever anything else needs waking, which is the opposite of
    /// what was asked for. When a helper is itself asleep, the app says so and offers waking it as a
    /// separate, explicit step.
    ///
    /// A helper's command whose run may still be going leaves a wake hold, and while one stands only
    /// a declared `wol` action runs: before planning, each held helper that keeps records is asked
    /// about it once, and a hold that appears during the walk takes every later command out of it.
    func wake() {
        guard machine.wake != nil else {
            wakeThenBoot = nil
            return
        }
        // Nothing to try and nothing a press could settle: said without starting anything.
        let cached = wakePlan
        if cached.isEmpty, wakeHolds.isEmpty {
            wakeThenBoot = nil
            setStatus(cached.nothingToTry, isError: true)
            onStateChange?()
            return
        }

        let started = perform("Waking \(machine.name)", kind: .wake, subject: machine.name) { operation in
            defer { self.isWaking = false }
            let resolution = await self.walkWakePlan(operation: operation)
            // Only a machine seen answering goes on into the system it was asked to wake into. Every
            // other ending lets the request go, so a later wake that does work never boots into a
            // target nobody asked for this time (M13).
            if resolution.state == .succeeded {
                self.continueWakeIntoTargetIfNeeded()
            } else {
                self.wakeThenBoot = nil
            }
            return resolution
        }
        // The work itself starts on a later turn of the main actor, so the flag goes up here, and
        // only when this wake took the device rather than finding another action on it.
        if started { isWaking = true } else { wakeThenBoot = nil }
    }

    /// Release every hold for this machine because a person chose to, then wake it.
    func tryOtherHelpers() {
        operations?.releaseWakeHolds(target: machine.id, before: .distantFuture,
                                     reason: "you chose to try the other helpers anyway")
        wake()
    }

    private func walkWakePlan(operation: String) async -> ActionResolution {
        await settleWakeHolds()
        let plan = self.plan(from: freshPlacement?() ?? placement?() ?? .nowhere)
        guard !plan.isEmpty else {
            setStatus(plan.nothingToTry, isError: true)
            return .init(state: .failed, summary: plan.nothingToTry)
        }
        let entries = machine.wake?.orderedHelpers ?? []
        var lines: [String] = []

        for step in plan.steps {
            switch step {
            case .direct(let broadcasts, let ports):
                setStatus("Sending the wake packet to \(machine.name).")
                let packets = WakeOnLAN.Packets(mac: machine.wake?.mac, broadcasts: broadcasts, ports: ports)
                if let failure = await sendWakePackets(packets) {
                    // No packet left, so there is nothing to wait for.
                    lines.append("sending it from here: \(failure)")
                    continue
                }
                operations?.addPhase(operation, name: "packet sent from this device")
                lines.append("sent from this device to \(broadcasts.joined(separator: ", ")), ports \(ports.map(String.init).joined(separator: ", "))")

            case .helper(let helper, let machineName, let readFirst):
                // Read once just before its step, when it was not read this session. A helper read
                // and still not ready is skipped as asleep or as unusable, never asked anyway.
                var state = helperState(helper.machine)
                if readFirst {
                    let device = helperDevice(helper.machine)
                    if let device {
                        setStatus("Checking \(machineName) before asking it to wake \(machine.name).")
                        await device.refreshStatus(duringAction: false)
                        state = helperState(helper.machine)
                    }
                    if state.state == .unknown {
                        state = .init(.unusable, note: device == nil ? "this device does not drive it."
                                                                     : "it did not answer the read just before its turn.")
                    }
                }
                // Asked again with what the walk has learnt since it planned: a helper it has just
                // read, and a hold an earlier step made.
                guard let setup = setup?() else { continue }
                let index = entries.firstIndex(of: helper) ?? entries.count
                if let reason = WakePlan.skipReason(for: helper, target: machine, earlier: entries[..<index],
                                                    in: setup, state: state, holds: wakeHolds) {
                    let skip = WakePlan.skip(helper, reason, target: machine, in: setup, state: state)
                    lines.append("\(skip.what): \(skip.why)")
                    continue
                }

                setStatus("Asking \(machineName) to wake \(machine.name).")
                guard let device = helperDevice(helper.machine) else {
                    lines.append("\(machineName): this device has no model for it")
                    continue
                }
                let outcome = await device.runWakeAction(helper.action, target: machine.name, targetId: machine.id)
                operations?.addPhase(operation, name: "asked \(machineName)", detail: outcome.summary)
                lines.append("\(machineName): \(outcome.summary)")
                // Anything but `ran` moves straight on. An ambiguous command has left a hold, and
                // every later step that is not a declared magic packet is skipped for it.
                guard outcome.sent else { continue }
            }

            // Something was sent. Readiness is decided by an authenticated agent call, never by
            // opening a TCP connection to port 22 and dropping it: repeated connections that
            // never complete authentication are exactly what OpenSSH's per-source penalties are
            // for, and earning a temporary refusal from the machine you are trying to reach is a
            // self-inflicted outage.
            setStatus("Waiting for \(machine.name) to answer.")
            if await waitUntilAgentAnswers() {
                setStatus("\(machine.name) is awake.")
                sleepExpectation = nil
                return .init(state: .succeeded, summary: "\(machine.name) is awake.",
                             detail: (lines + plan.skipped.map { "\($0.what): \($0.why)" }).joined(separator: "\n"))
            }
            lines.append("no answer within two minutes")
        }

        // Every step was taken and it is still not answering. A packet leaving is not a machine
        // waking, and saying otherwise would invent the one fact that matters.
        let detail = (lines + plan.skipped.map { "\($0.what): \($0.why)" }).joined(separator: "\n")
        setStatus("\(machine.name) did not come up.", isError: true, detail: detail)
        return .init(state: .unknown, summary: "\(machine.name) was asked to wake and had not answered two minutes after the last step.",
                     detail: detail)
    }

    /// Ask each held helper that keeps records about its command, once, before a wake is planned.
    ///
    /// A helper last seen asleep is not asked: the answer would be a timeout, and a hold is never
    /// released by its helper being unreachable. A 2.x helper keeps no records, so its hold waits
    /// for this machine to answer, for the helper to leave the setup, or for a person.
    private func settleWakeHolds() async {
        guard let operations else { return }
        for held in operations.wakeHolds(target: machine.id) where held.agentTracked {
            guard helperState(held.machineId).state != .asleep, let device = helperDevice(held.machineId) else { continue }
            guard let reply = try? await device.send(try AgentRequest.operation(id: held.id, wait: nil),
                                                     decoding: AgentOperationResult.self) else { continue }
            if let record = reply.operation {
                operations.reconcile(held.id, with: record)
            } else if reply.isUnknownOperation {
                operations.resolveAsNeverStarted(held.id)
            }
        }
    }

    /// Wake this machine and then boot it into a particular system.
    ///
    /// The second half only happens after the machine has been seen answering as something else. A
    /// packet leaving proves nothing, and a reboot request sent into the dark is a reboot nobody can
    /// account for.
    func wake(into target: SystemConfig) {
        wakeThenBoot = target
        wake()
    }

    private var wakeThenBoot: SystemConfig?

    private func continueWakeIntoTargetIfNeeded() {
        guard let target = wakeThenBoot else { return }
        wakeThenBoot = nil
        guard let current = currentSystem, current.id != target.id else { return }
        requestBoot(into: target)
    }

    /// Poll for the machine by asking the agent a real, authenticated question, at the times the
    /// contract gives: 5 s after the send, then at intervals growing 1.6 times to at most 20 s, for
    /// two minutes. Never faster than every three seconds, which is the contract's floor.
    ///
    /// Each poll is a real one. An address that did not answer is otherwise given a minute before
    /// it is dialled again, and the polls at 13, 25.8 and 45.8 seconds fell inside the minute the
    /// first one started, so a machine that came up in twenty seconds was not seen for over sixty.
    /// A packet has just been sent, and coming up is exactly what the machine is expected to do.
    private func waitUntilAgentAnswers() async -> Bool {
        let sent = wakeClock.now()
        for start in WakePlan.readinessSchedule(sentAnything: true) {
            let wait = sent.addingTimeInterval(Double(start) / 1000).timeIntervalSince(wakeClock.now())
            if wait > 0 { await wakeClock.sleep(wait) }
            if Task.isCancelled { return false }
            allowRedial()
            await refresh(userInitiated: false, duringAction: true)
            if isAwake { return true }
        }
        return false
    }

    func requestSleep() {
        guard commandableSystem != nil else { return }
        offerDisruptive(
            id: "sleep-\(machine.id)",
            title: "Put \(machine.name) to sleep?",
            body: {
                if let _ = busyReason {
                    return "Activity is busy or unconfirmed. Sleeping now can interrupt work."
                }
                return canWake
                    ? "\(machine.name) suspends to memory now. Wake on LAN stays armed, so the Wake button brings it back."
                    : "\(machine.name) suspends to memory now."
            }() + reconnectsAfterward,
            confirmTitle: busyReason == nil ? "Sleep now" : "Sleep anyway",
            now: { [weak self] in self?.sleep(force: self?.busyReason != nil, whenIdle: false) },
            whenIdle: { [weak self] in self?.sleep(force: false, whenIdle: true) }
        )
    }

    /// Suspend to memory. Gated exactly like a reboot, because work in flight is lost either way.
    func sleep(force: Bool, whenIdle: Bool) {
        guard commandableSystem != nil else { return }
        let dialect = dialect
        perform("Putting \(machine.name) to sleep", kind: .sleep, subject: machine.name) { operation in
            await self.dispatch(
                operation: operation,
                subject: self.machine.name,
                verb: "The sleep",
                build: { intent in try AgentRequest.sleep(intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: false, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "sleeping", "slept":
                        // The agent said so in JSON. That is the only evidence this app ever accepts
                        // for a machine having gone to sleep.
                        self.markAsleep(
                            line: self.canWake
                                ? "\(self.machine.name) is going to sleep. Wake brings it back."
                                : "\(self.machine.name) is going to sleep.",
                            detail: result.message
                        )
                        return .init(state: .succeeded, summary: "\(self.machine.name) is going to sleep.", detail: result.message)
                    case "deferred":
                        return self.deferred(result, subject: self.machine.name, verb: "Sleep", force: force) {
                            self.sleep(force: true, whenIdle: false)
                        }
                    default:
                        return self.otherOutcome(result, subject: self.machine.name, verb: "The sleep")
                    }
                }
            )
        }
    }

    /// Said under a sleep or a reboot of the machine this device is, because the app goes with it.
    private var reconnectsAfterward: String {
        bindings?().isSelf(machine.id) == true ? "\n\nThis controller reconnects after the device returns." : ""
    }

    /// Paint the machine as gone straight away rather than waiting for a poll to find out.
    private func markAsleep(line: String, detail: String?) {
        rebootExpectation = nil
        sleepExpectation = Date().addingTimeInterval(Self.sleepGrace)
        status = nil
        link = .offline(AgentFailure(.hostUnreachable, detail: "Asleep on request.", dispatch: .never))
        setStatus(line, detail: detail)
    }

    func requestBoot(into target: SystemConfig) {
        guard commandableSystem != nil else { return }
        offerDisruptive(
            id: "boot-\(machine.id)-\(target.id)",
            title: "Boot into \(target.name)?",
            body: {
                if let _ = busyReason {
                    return "Activity is busy or unconfirmed. Rebooting into \(target.name) now can lose work."
                }
                return "\(machine.name) will reboot now and come back up in \(target.name). This takes about a minute."
            }() + reconnectsAfterward,
            confirmTitle: busyReason == nil ? "Reboot now" : "Switch anyway",
            now: { [weak self] in self?.boot(into: target, force: self?.busyReason != nil, whenIdle: false) },
            whenIdle: { [weak self] in self?.boot(into: target, force: false, whenIdle: true) }
        )
    }

    func boot(into target: SystemConfig, force: Bool, whenIdle: Bool) {
        guard commandableSystem != nil else { return }
        let dialect = dialect
        perform("Switching to \(target.name)", kind: .boot, subject: target.name) { operation in
            await self.dispatch(
                operation: operation,
                subject: target.name,
                verb: "The switch to \(target.name)",
                build: { intent in try AgentRequest.boot(target: target.id, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: false, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "rebooting", "rebooted":
                        self.expectReboot(into: target)
                        self.setStatus("Rebooting into \(target.name). This takes about a minute.", detail: result.message)
                        return .init(state: .succeeded, summary: "Rebooting into \(target.name).", detail: result.message)
                    case "armed":
                        self.setStatus("The next boot is set to \(target.name). Nothing has rebooted yet.", detail: result.message)
                        return .init(state: .succeeded, summary: "The next boot is set to \(target.name).", detail: result.message)
                    case "noop":
                        self.setStatus("\(target.name) is already running.", detail: result.message)
                        return .init(state: .noop, summary: "\(target.name) is already running.", detail: result.message)
                    case "deferred":
                        return self.deferred(result, subject: target.name, verb: "The switch", force: force) {
                            self.boot(into: target, force: true, whenIdle: false)
                        }
                    default:
                        return self.otherOutcome(result, subject: target.name, verb: "The switch to \(target.name)")
                    }
                }
            )
        }
    }

    private func expectReboot(into target: SystemConfig) {
        // The remembered route belongs to the system that is going away.
        lastRoute = nil
        rebootExpectation = (target, Date().addingTimeInterval(240))
        link = .offline(AgentFailure(.hostUnreachable, detail: "Rebooting into \(target.name).", dispatch: .never))
        status = nil
    }

    // MARK: - Services

    /// Deliberately NOT forced, and deliberately not judged from what we last read.
    func requestUpdate(_ service: ServiceStatus) {
        update(service, force: false, whenIdle: false)
    }

    // MARK: - Policy

    func setAutoUpdate(_ enabled: Bool, on system: SystemConfig, service: ServiceStatus? = nil) {
        guard currentSystem?.id == system.id else { return }
        let machineId = machine.id
        saveAutoUpdate(enabled, service: service, subject: service?.displayName ?? system.name) { [remembered] value in
            guard service == nil else { return }
            remembered.record(machine: machineId, system: system.id, autoUpdate: value)
        }
    }

    // MARK: - Diagnostics

    func serviceConfig(_ request: AgentRequest) async throws -> ServiceConfigReply {
        guard commandableSystem?.isRestricted != true, status?.isRestrictedSession != true else {
            throw AgentBootstrap.Failure(message: "Service setup requires an administrator connection. Restricted keys cannot edit this configuration.")
        }
        return try await send(request, decoding: ServiceConfigReply.self)
    }

    // MARK: - Trusted agent deployment

    /// Why the bundled agent cannot be installed over this machine's 2.x agent, or nil when it can,
    /// or when the agent is new enough to take it on stdin. Shown before the install is offered.
    var legacyInstallRefusal: String? {
        guard let status, AgentBootstrap.isAuthenticatedLegacy(status), let system = commandableSystem else { return nil }
        // The bootstrap runs under the system's own interpreter, which is part of its command. A
        // system this device has not approved a command for has none to run it under, and an
        // installation base worked out from nothing would be asked of the person instead.
        if system.launchState == .notDriven {
            return "\(system.name) is not driven from this device until its command is approved."
        }
        return AgentBootstrap.remoteRefusal(system: system, restrictedSession: status.isRestrictedSession)
    }

    /// A 2.x agent cannot take the payload on stdin, so the bootstrap is uploaded over the route
    /// the last call went over and run there.
    func installOverLegacyAgent(_ payload: Data, operation: String, version: String) async -> ActionResolution {
        if let refusal = legacyInstallRefusal { return .init(state: .failed, summary: refusal) }
        guard let system = commandableSystem, let route = lastRoute,
              let layout = AgentBootstrap.chooseLayout(argv: system.agent) else {
            return .init(state: .cancelled, summary: "The legacy agent upgrade was cancelled; no installation was attempted.")
        }
        operations?.markAgentTracked(operation)
        do {
            let result = try await AgentBootstrap.installRemote(payload: payload, operation: operation,
                                                               layout: layout, system: system, target: route.target)
            guard result.didInstallAgent else {
                return otherOutcome(result, subject: "control agent", verb: "Installing the agent")
            }
            ask?(PendingDialog(id: "migrate-agent-\(id)", title: "Use the stable agent launcher?",
                               message: "The upgrade completed. Update this system's command?\n\nCurrent: \(CommandArguments.text(system.agent))\nProposed: \(CommandArguments.text(layout.stableArgv))",
                               confirmTitle: "Update command", perform: { self.migrateAgentCommand?(system.id, layout.stableArgv) }))
            return .init(state: .succeeded, summary: "Agent \(version) was installed on \(machine.name).", refresh: true)
        } catch let failure as AgentBootstrap.Failure {
            return .init(state: failure.uncertain ? .unknown : .failed, summary: failure.message, unresolved: failure.uncertain)
        } catch {
            setStatus(error.localizedDescription, isError: true)
            return .init(state: .failed, summary: error.localizedDescription)
        }
    }
}
