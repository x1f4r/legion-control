import Foundation
import Observation

/// One configured machine: what it is running, what its services are doing, and everything that can
/// be done to it.
///
/// There is one of these per machine in the config, each with its own link state, its own poll and
/// its own remembered settings. Nothing about one machine is ever read off another.
@MainActor
@Observable
final class MachineModel: @MainActor Identifiable {
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
    private(set) var remembered: [String: RememberedSystem] = RememberedStore.load()
    private(set) var lastChecked: Date?
    private(set) var isRefreshing = false
    /// Non-nil while a user-initiated action runs. Doubles as the "controls are disabled" flag.
    private(set) var activity: String?
    private(set) var statusLine = "Ready."
    private(set) var statusIsError = false
    private(set) var statusDetail: String?
    private var statusIsRefreshFailure = false
    /// When the status line last changed. The window has one status bar for the whole app, so with
    /// more than one machine it shows whichever of them spoke last.
    private(set) var statusStamp = Date.distantPast
    /// What the last action the user started ended in, or nil while none has run since launch.
    private(set) var lastActionOutcome: ActionOutcome?
    /// The last doctor run, when one has been asked for.
    private(set) var doctor: AgentDoctorResult?
    private(set) var doctorRanAt: Date?
    /// The route the last successful call went over, for the diagnosis page.
    private(set) var lastRoute: MachineRoute?
    /// The operation this app is currently watching over there, so the section can show its phase.
    private(set) var watchedOperation: String?
    /// A host key this machine offered that is not in known_hosts. Nothing is ever written without
    /// the user saying so, so this sits here until they do.
    private(set) var hostKeyPrompt: HostKeyPrompt?

    struct ActionOutcome: Equatable, Sendable {
        var text: String
        var isError: Bool
        /// The operation it came from, so the panel can offer to look at it.
        var operationId: String?
    }

    var id: String { machine.id }
    var name: String {
        guard let binding = bindings?(), binding.isSelf(machine.id) else { return machine.name }
        return binding.effectiveDeviceName
    }

    /// Called whenever something a viewer draws has changed.
    var onStateChange: (@MainActor () -> Void)?
    /// How a question reaches the screen. The app owns the one place a question is shown, because
    /// only one of them can be answered at a time whichever machine raised it.
    var ask: (@MainActor (PendingDialog) -> Void)?
    /// The setup this Mac is running on, for the machines to be given a copy of.
    var controllerConfig: (@MainActor () -> ControllerDocument?)?
    /// Where operations are written down. A model without one still works and keeps no history.
    var operations: OperationStore?
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
    /// Adopt a document read off this machine, or hand a merge back to the app.
    var adoptSetup: (@MainActor (Data, String) -> String?)?
    /// Raise the divergence sheet.
    var onSetupDiverged: (@MainActor (SetupDivergence) -> Void)?
    /// The bytes of the newest document both lineages remember, when this device kept them. Without
    /// it a merge can still be done, but every difference needs a decision instead of only the ones
    /// both sides touched.
    var setupBase: (@MainActor (ControllerIdentity, ControllerIdentity) -> Data?)?
    /// Told when something the user explicitly asked for has finished, so it can be announced.
    var onOperationFinished: (@MainActor (OperationRecord) -> Void)?

    private var agent: RemoteAgent
    /// Set only after the agent has acknowledged a reboot in JSON, and cleared the moment the target
    /// is observed running. A dropped link or a timeout never sets it: neither is evidence that
    /// anything happened.
    private var rebootExpectation: (target: SystemConfig, until: Date)?
    /// The same, for a suspend the agent acknowledged.
    private var sleepExpectation: Date?
    private var lastRefreshFinished: Date?
    /// The last time the app asked the far side about an operation it had lost track of.
    private var lastReconcileAttempt: Date = .distantPast

    init(machine: Machine) {
        self.machine = machine
        self.agent = RemoteAgent(machine: machine)
    }

    /// For the tests: a model whose transport never launches a process.
    init(machine: Machine, agent: RemoteAgent) {
        self.machine = machine
        self.agent = agent
    }

    /// How long a machine that will not take the setup is left alone before it is offered again.
    private static let shareRetry: TimeInterval = 600

    /// How long an unreachable machine still reads as "asleep" rather than as a problem. The phone
    /// uses the same ten minutes, and the two apps have to say the same thing about the same machine.
    private static let sleepGrace: TimeInterval = 600

    /// How often an operation with an unknown outcome is chased.
    private static let reconcileEvery: TimeInterval = 60

    var isWorking: Bool { activity != nil }

    /// True only while the wake packet is in flight.
    var isWaking: Bool { activity == wakeLabel }

    private var wakeLabel: String { "Waking \(machine.name)" }

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

    /// Which contract the far side speaks, and therefore what may be sent to it.
    var dialect: AgentDialect { AgentDialect(status) }

    /// True when the agent is too old for the safety properties this app depends on, and the machine
    /// section should offer to install the bundled one.
    var needsAgentUpgrade: Bool {
        guard let status, status.ok != false else { return false }
        return !status.speaksRequiredContract
    }

    var rebootInProgress: (target: SystemConfig, until: Date)? {
        guard let rebootExpectation, rebootExpectation.until > Date() else { return nil }
        return rebootExpectation
    }

    var busyReason: String? {
        guard let busy = status?.busy, busy.isBusy else { return nil }
        return busy.summary
    }

    /// Whether it is safe to assume the machine is idle. An unknown or unmonitored busy state is not
    /// idle: it is an absence of evidence, and disruptive work waits for evidence.
    var isKnownIdle: Bool {
        guard let busy = status?.busy else { return false }
        return busy.verdict == .idle
    }

    var services: [ServiceStatus] { status?.resolvedServices ?? [] }

    func service(id: String) -> ServiceStatus? { status?.service(id: id) }

    var actions: [AgentActionInfo] { (status?.actions ?? []).filter { !$0.id.isEmpty } }

    func actions(for service: ServiceStatus) -> [AgentActionInfo] {
        (service.actions ?? []).filter { !$0.id.isEmpty }
    }

    /// Everything the agent could not read, in its own words.
    var notes: [String] { status?.allNotes ?? [] }

    /// The system-wide update policy the far side reports.
    var policy: AgentUpdatePolicy? { status?.updates }

    /// Requests the agent is holding until the machine is idle.
    var queuedOperations: [AgentOperation] { status?.queuedOperations ?? [] }

    /// What is running over there right now.
    var runningOperations: [AgentOperation] { status?.runningOperations ?? [] }

    var recentOperations: [AgentOperation] { status?.recentOperations ?? [] }

    /// Operations this app started here and lost the answer to.
    var unresolvedOperations: [OperationRecord] {
        (operations?.unresolved ?? []).filter { $0.machineId == machine.id }
    }

    /// The record for the operation the app is watching, so the section can draw its phase.
    var watchedRecord: OperationRecord? {
        guard let watchedOperation else { return nil }
        return operations?.record(id: watchedOperation)
    }

    /// Where the machine can be booted, drawn from the config and filtered by what the agent offers.
    var bootTargets: [SystemConfig] {
        let running = currentSystem?.id
        let others = machine.systems.filter { $0.id != running }
        guard let reported = status?.bootTargets else { return others }
        let ids = Set(reported.map(\.id))
        return others.filter { ids.contains($0.id) }
    }

    // MARK: - Sharing the setup

    /// What this device last decided about this machine's copy of the setup.
    private(set) var setupDecision: SetupSync.Decision = .unknown
    /// The machine's own lineage, once `config meta` has been read. Read at most once per differing
    /// hash, because status deliberately does not carry it.
    private(set) var remoteSetupIdentity: ControllerIdentity?
    private var remoteLineageForHash: String?
    /// The document last sent, when it went, and what it ended in.
    private var lastShare: (hash: String, at: Date, failure: String?)?
    private var isSharing = false

    /// What the Setup row says.
    var setupSharing: SetupSharing {
        if let lastShare, let local = controllerConfig?(), lastShare.hash == local.hash,
           let failure = lastShare.failure {
            return .failed(failure)
        }
        return SetupSync.sharing(
            for: setupDecision,
            machine: machine.name,
            local: controllerConfig?(),
            remote: remoteSetupIdentity ?? status?.controller?.identity
        )
    }

    /// Everything the divergence sheet needs.
    struct SetupDivergence: Sendable, Identifiable {
        var machineId: String
        var machineName: String
        var mine: ControllerDocument
        var theirsBytes: Data
        var theirsIdentity: ControllerIdentity
        var baseBytes: Data?
        var differences: [SetupMerge.Difference]
        /// True when the two are different setups rather than two branches of one.
        var isDifferentSetup: Bool

        var id: String { machineId }
    }

    func autoUpdateValue(for system: SystemConfig) -> Bool? {
        if currentSystem?.id == system.id, let live = status?.autoUpdate { return live }
        return remembered[RememberedStore.key(machine: machine.id, system: system.id)]?.autoUpdate
    }

    func autoUpdateCheckedAt(for system: SystemConfig) -> Date? {
        remembered[RememberedStore.key(machine: machine.id, system: system.id)]?.checkedAt
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

    /// The second line of the machine's row in the sidebar.
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

    /// The second line of a service's row.
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
        if let site = machine.site { return placement.effective == site }
        return SiteAwareness.matchesLANPrefix(machine.wake?.lanPrefix, addresses: SiteAwareness.localAddresses())
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
            lastRoute = reply.route
            lastChecked = Date()
            if !userInitiated, statusIsRefreshFailure {
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
        let key = RememberedStore.key(machine: machine.id, system: configured.id)
        var snapshot = remembered[key] ?? RememberedSystem()
        snapshot.autoUpdate = newStatus.autoUpdate ?? snapshot.autoUpdate
        snapshot.checkedAt = Date()
        remembered[key] = snapshot
        RememberedStore.save(remembered)
    }

    private func handleStatusFailure(_ failure: AgentFailure) {
        switch failure.kind {
        case .agentMissing, .interpreterMissing:
            status = nil
            if let system = failure.system {
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
            let target = failure.target ?? machine.sshTarget
            if hostKeyPrompt?.target != target || hostKeyPrompt == nil {
                hostKeyPrompt = HostKeyPrompt(machine: machine.name, target: target, detail: failure.detail)
            }
            setStatus(failure.message(machine: machine.name), isError: true, detail: failure.detailText)
        case .hostKeyChanged:
            status = nil
            link = .offline(failure)
            let target = failure.target ?? machine.sshTarget
            if hostKeyPrompt?.target != target || hostKeyPrompt == nil {
                hostKeyPrompt = HostKeyPrompt(machine: machine.name, target: target, detail: failure.detail)
            }
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
        statusIsRefreshFailure = true
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

    func readHostKeyFingerprint() {
        guard let prompt = hostKeyPrompt, let target = prompt.target else { return }
        hostSystemsConfirmed = false
        Task { @MainActor in
            do {
                let context = try await HostIdentityContext.resolve(target)
                let scan = await Shell.run(executable: "/usr/bin/ssh-keyscan",
                                           arguments: ["-T", "8", "-p", String(context.port), context.scanHost], timeout: 20)
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

    // MARK: - Chasing what we lost track of

    /// Ask the far side what became of an operation whose outcome this app never learned.
    ///
    /// This is the whole reason mutations carry an id. Before, an update that timed out left nothing
    /// behind but a red sentence, and the only way to find out whether it had actually installed was
    /// to read the version afterwards and guess.
    private func reconcileIfNeeded() {
        guard let operations, dialect.supportsOperations else { return }
        guard Date().timeIntervalSince(lastReconcileAttempt) > Self.reconcileEvery else { return }
        let owed = operations.unresolved.filter { $0.machineId == machine.id && $0.agentTracked }
        guard let next = owed.first else { return }
        lastReconcileAttempt = Date()

        // The status the poll just read may already carry it, which costs nothing at all.
        if let known = status?.operation(id: next.id) {
            operations.reconcile(next.id, with: known)
            onStateChange?()
            return
        }

        Task { @MainActor in
            do {
                let request = try AgentRequest.operation(id: next.id, wait: nil)
                let reply = try await self.agent.send(request, decoding: AgentOperationResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                if let operation = reply.value.operation {
                    operations.reconcile(next.id, with: operation)
                } else if reply.value.isUnknownOperation {
                    operations.resolveAsNeverStarted(next.id)
                }
                self.onStateChange?()
            } catch {
                // Still no answer. It stays owed, and the next poll asks again.
            }
        }
    }

    /// Ask now, because the user pressed the button that asks.
    func reconcileNow() {
        lastReconcileAttempt = .distantPast
        reconcileIfNeeded()
    }

    func dismissUnknownOutcome(_ id: String) {
        operations?.dismissReconciliation(id)
        onStateChange?()
    }

    // MARK: - Reconciling the setup

    /// Decide what to do about this machine's copy, and do the safe half of it automatically.
    ///
    /// Two moves need no permission, because both follow strict descent and neither can lose
    /// anything: push when the machine is holding one of this document's ancestors, and fetch when
    /// this document is one of the machine's ancestors. Descent is acyclic, so two devices can never
    /// take turns overwriting each other. Everything else stops and asks.
    private func reconcileSetup() {
        guard activity == nil, !isSharing else { return }
        guard let status else { return }
        let local = controllerConfig?()

        let view = SetupSync.MachineView(
            reportsCopy: status.reportsControllerCopy,
            contract: status.contractVersion,
            hash: status.controllerHash,
            identity: remoteIdentity(for: status),
            lineage: lineageIfCurrent(for: status)
        )
        let decision = SetupSync.decide(local: local, machine: view)
        setupDecision = decision

        switch decision {
        case .inSync, .unsupported, .unknown, .diverged, .differentSetup:
            return
        case .push:
            guard let local else { return }
            // One attempt per document per machine until the reply is read, then a minute's backoff.
            if let lastShare, lastShare.hash == local.hash,
               Date().timeIntervalSince(lastShare.at) < SetupSync.retryAfter { return }
            publish(local)
        case .fetch:
            fetchSetup()
        case .readMeta:
            readRemoteSetupMeta()
        }
    }

    private func remoteIdentity(for status: AgentStatus) -> ControllerIdentity? {
        if let remoteSetupIdentity, remoteLineageForHash == status.controllerHash { return remoteSetupIdentity }
        return status.controller?.identity
    }

    /// The machine's lineage, but only when it belongs to the hash the machine is reporting now.
    private func lineageIfCurrent(for status: AgentStatus) -> [String]? {
        guard let remoteLineageForHash, remoteLineageForHash == status.controllerHash else { return nil }
        return remoteSetupIdentity?.lineage ?? []
    }

    /// Send this document, because the machine holds one of its ancestors.
    private func publish(_ local: ControllerDocument) {
        isSharing = true
        let dialect = dialect
        Task { @MainActor in
            var failure: String?
            do {
                let request = try AgentRequest.configSet(local, replace: false, dialect: dialect)
                let reply = try await self.agent.send(request, decoding: AgentConfigResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                if reply.value.ok == false {
                    // A refusal here is information, not a fault: the machine has told us our
                    // document does not descend from what it holds, which is exactly the case a
                    // person has to look at.
                    self.remoteSetupIdentity = reply.value.identity
                    self.remoteLineageForHash = reply.value.storedHash
                    failure = reply.value.reason?.sentence(subject: "the setup", message: reply.value.error ?? reply.value.message)
                        ?? reply.value.error ?? "the machine kept the copy it had."
                } else if let stored = reply.value.storedHash, stored != local.hash {
                    failure = "the machine stored something other than the document on this device."
                }
            } catch let error as AgentFailure {
                failure = error.detailText ?? error.message(machine: self.machine.name)
            } catch let invalid as AgentRequest.Invalid {
                failure = invalid.message
            } catch {
                failure = error.localizedDescription
            }

            let sentence = failure.map { "The setup could not be shared with \(self.machine.name): \($0)" }
            self.lastShare = (hash: local.hash, at: Date(), failure: sentence)
            if let sentence { self.setStatus(sentence, isError: true) }
            self.isSharing = false
            self.onStateChange?()
        }
    }

    /// Read the machine's document and adopt it, because this device is behind.
    private func fetchSetup() {
        isSharing = true
        Task { @MainActor in
            defer {
                self.isSharing = false
                self.onStateChange?()
            }
            guard let fetched = await self.readRemoteDocument() else { return }
            if let problem = self.adoptSetup?(fetched.bytes, "adopted from \(self.machine.name)") {
                self.setStatus("The setup from \(self.machine.name) could not be adopted: \(problem)", isError: true)
            } else {
                self.setStatus("Adopted the setup from \(self.machine.name): revision \(fetched.identity.revisionNumber).")
            }
        }
    }

    /// Read `config` and check that what came back really is what the machine says it holds.
    private func readRemoteDocument() async -> (bytes: Data, identity: ControllerIdentity)? {
        do {
            let reply = try await agent.send(.configRead(), decoding: RemoteControllerDocument.self,
                                             preferring: commandableSystem, route: lastRoute)
            guard let document = reply.value.documentBytes else {
                setStatus("\(machine.name) did not return a setup document.", isError: true)
                return nil
            }
            let canonical = try Canonical.bytes(document)
            let hash = Canonical.sha256(canonical)
            // The hash the machine reports has to match the bytes it sent. Anything else means the
            // document was mangled on the way, and adopting it would spread the damage.
            if let reported = reply.value.hash, reported != hash {
                setStatus("\(machine.name) sent a setup whose hash does not match what it reports, so it was not adopted.",
                          isError: true)
                return nil
            }
            return (canonical, reply.value.meta?.identity ?? ControllerIdentity())
        } catch let error as AgentFailure {
            setStatus("The setup on \(machine.name) could not be read: \(error.message(machine: machine.name))",
                      isError: true, detail: error.detailText)
            return nil
        } catch {
            setStatus("The setup on \(machine.name) could not be read: \(error.localizedDescription)", isError: true)
            return nil
        }
    }

    /// Read the machine's lineage, which `status` deliberately does not carry.
    private func readRemoteSetupMeta() {
        guard let hash = status?.controllerHash, remoteLineageForHash != hash else { return }
        isSharing = true
        Task { @MainActor in
            defer {
                self.isSharing = false
                self.onStateChange?()
            }
            do {
                let reply = try await self.agent.send(.configMeta(), decoding: AgentConfigResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                self.remoteSetupIdentity = reply.value.identity
                self.remoteLineageForHash = reply.value.storedHash ?? hash
                self.reconcileSetup()
            } catch {
                // Nothing decided. The next poll asks again.
            }
        }
    }

    /// Gather everything the divergence sheet needs, then raise it.
    func openSetupDivergence() {
        guard let local = controllerConfig?() else { return }
        let isDifferent = setupDecision == .differentSetup
        guard isDifferent || setupDecision == .diverged else { return }

        Task { @MainActor in
            guard let fetched = await self.readRemoteDocument() else { return }
            let base = self.setupBase?(local.identity, fetched.identity)
            let differences = (try? SetupMerge.differences(mine: local.bytes, theirs: fetched.bytes, base: base)) ?? []
            self.onSetupDiverged?(SetupDivergence(
                machineId: self.machine.id,
                machineName: self.machine.name,
                mine: local,
                theirsBytes: fetched.bytes,
                theirsIdentity: fetched.identity,
                baseBytes: base,
                differences: differences,
                isDifferentSetup: isDifferent
            ))
        }
    }

    /// Overwrite the machine's copy with this device's, because the user decided to.
    ///
    /// The only place `--replace` is ever sent, and only after a question with both revisions and
    /// their authors in it.
    func replaceSetup() {
        guard let local = controllerConfig?() else { return }
        let dialect = dialect
        perform("Replacing the setup on \(machine.name)", kind: .shareSetup, subject: "setup") { _ in
            do {
                let request = try AgentRequest.configSet(local, replace: true, dialect: dialect)
                let reply = try await self.agent.send(request, decoding: AgentConfigResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                if reply.value.ok == false {
                    let sentence = reply.value.error ?? reply.value.message ?? "the machine refused the document."
                    self.setStatus("The setup was not replaced: \(sentence)", isError: true)
                    return .init(state: .failed, summary: "The setup on \(self.machine.name) was not replaced. \(sentence)")
                }
                self.lastShare = (hash: local.hash, at: Date(), failure: nil)
                self.remoteSetupIdentity = nil
                self.remoteLineageForHash = nil
                self.setStatus("\(self.machine.name) now holds this device's setup.")
                return .init(state: .succeeded,
                             summary: "\(self.machine.name) now holds revision \(local.identity.revisionNumber).",
                             refresh: true)
            } catch let failure as AgentFailure {
                return self.reportMutationFailure(failure, verb: "Replacing the setup", subject: "setup")
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportMutationFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "Replacing the setup", subject: "setup"
                )
            }
        }
    }

    /// Adopt what the machine holds, dropping this device's branch.
    func adoptRemoteSetup() {
        Task { @MainActor in
            guard let fetched = await self.readRemoteDocument() else { return }
            if let problem = self.adoptSetup?(fetched.bytes, "took the setup from \(self.machine.name)") {
                self.setStatus(problem, isError: true)
            } else {
                self.setStatus("This device now uses the setup from \(self.machine.name).")
                self.remoteSetupIdentity = nil
                self.remoteLineageForHash = nil
                self.setupDecision = .unknown
            }
            self.onStateChange?()
        }
    }

    // MARK: - Power

    /// What waking this machine would do from where this device is, and what stands in the way.
    var wakePlan: WakePlan.Plan {
        let here = placement?() ?? SiteAwareness.Placement(matching: [], confirmed: nil)
        guard let setup = setup?() else {
            return WakePlan.Plan(steps: [], unavailable: [], placement: here)
        }
        var states: [String: WakePlan.HelperState] = [:]
        for helper in machine.wake?.orderedHelpers ?? [] {
            if let local = localPeer?(helper.machine) {
                states[helper.machine] = WakePlan.HelperState(isAwake: true, isCommandable: local.isReachable,
                                                            canBeWoken: false, lastCheckedAt: local.lastChecked)
                continue
            }
            guard let model = peer?(helper.machine) else { continue }
            states[helper.machine] = WakePlan.HelperState(
                isAwake: model.isAwake,
                isCommandable: model.commandableSystem != nil,
                canBeWoken: model.canWake,
                lastCheckedAt: model.lastChecked
            )
        }
        return WakePlan.plan(
            for: machine,
            in: setup,
            placement: here,
            addresses: SiteAwareness.localAddresses(),
            helperStates: states
        )
    }

    /// Wake the machine: from here when this device is on its network, and through a helper when it
    /// is not.
    ///
    /// The helpers are tried in the order the setup lists them and no further than that. Nothing is
    /// ever woken in order to become a helper: a cascade would turn the machine the user is trying
    /// not to run into one that runs whenever anything else needs waking, which is the opposite of
    /// what was asked for. When a helper is itself asleep, the app says so and offers waking it as a
    /// separate, explicit step.
    func wake() {
        guard machine.wake != nil else { return }
        let plan = wakePlan
        guard !plan.isEmpty else {
            setStatus(plan.nothingToTry, isError: true)
            onStateChange?()
            return
        }

        perform(wakeLabel, kind: .wake, subject: machine.name) { operation in
            var attempts: [String] = []

            for step in plan.steps {
                switch step {
                case .direct(let broadcasts, let ports):
                    self.setStatus("Sending the wake packet to \(self.machine.name).")
                    guard let wake = self.machine.wake else { continue }
                    let sent = WakeConfig(mac: wake.mac, broadcast: broadcasts, ports: ports)
                    if let failure = await WakeOnLAN.sendMagicPackets(sent) {
                        attempts.append("sending it from here: \(failure)")
                        continue
                    }
                    self.operations?.addPhase(operation, name: "packet sent from this device")
                    attempts.append("sent from this device to \(broadcasts.joined(separator: ", "))")

                case .helper(let helper, let machineName):
                    let outcome: WakeActionOutcome
                    if let local = self.localPeer?(helper.machine) {
                        outcome = await local.runWakeAction(helper.action, target: self.machine.name)
                    } else if let host = self.peer?(helper.machine) {
                        self.setStatus("Asking \(machineName) to wake \(self.machine.name).")
                        outcome = await host.runWakeAction(helper.action, target: self.machine.name)
                    } else {
                        attempts.append("\(machineName): this device has no model for it")
                        continue
                    }
                    self.operations?.addPhase(operation, name: "asked \(machineName)", detail: outcome.summary)
                    attempts.append("\(machineName): \(outcome.summary)")
                    if !outcome.sent {
                        // An ambiguous outcome on anything but a declared magic packet has to be
                        // settled before something else is tried. A `wol` action is an idempotent UDP
                        // send and costs three more datagrams to repeat; a general command may do
                        // anything, and "the link dropped" must not turn one requested action into
                        // two performed ones.
                        if let failure = outcome.failure,
                           !WakePlan.mayTryNextHelper(after: failure, actionIsDeclaredWakePacket: outcome.isDeclaredWakePacket) {
                            self.setStatus(
                                "\(machineName) may or may not have run \(helper.action), so no other helper was tried. \(failure.message(machine: machineName))",
                                isError: true
                            )
                            return .init(
                                state: .unknown,
                                summary: "Waking \(self.machine.name) through \(machineName) ended in an unknown state, so no other helper was tried.",
                                detail: attempts.joined(separator: "\n"),
                                unresolved: true
                            )
                        }
                        continue
                    }
                }

                // Something was sent. Readiness is decided by an authenticated agent call, never by
                // opening a TCP connection to port 22 and dropping it: repeated connections that
                // never complete authentication are exactly what OpenSSH's per-source penalties are
                // for, and earning a temporary refusal from the machine you are trying to reach is a
                // self-inflicted outage.
                self.setStatus("Waiting for \(self.machine.name) to answer.")
                if await self.waitUntilAgentAnswers(timeout: 120) {
                    self.setStatus("\(self.machine.name) is awake.")
                    self.sleepExpectation = nil
                    self.continueWakeIntoTargetIfNeeded()
                    return .init(state: .succeeded, summary: "\(self.machine.name) is awake.",
                                 detail: attempts.joined(separator: "\n"))
                }
                attempts.append("no answer within two minutes")
            }

            // Every step was taken and it is still not answering. A packet leaving is not a machine
            // waking, and saying otherwise would invent the one fact that matters.
            self.wakeThenBoot = nil
            let unavailable = plan.unavailable.map { "\($0.what): \($0.reason)" }
            self.setStatus("\(self.machine.name) did not come up.", isError: true,
                           detail: (attempts + unavailable).joined(separator: "\n"))
            return .init(
                state: .unknown,
                summary: "\(self.machine.name) was asked to wake and had not answered two minutes later.",
                detail: (attempts + unavailable).joined(separator: "\n")
            )
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

    /// What running one wake action on this machine came to.
    struct WakeActionOutcome: Sendable {
        var sent: Bool
        var summary: String
        var failure: AgentFailure?
        var isDeclaredWakePacket: Bool
    }

    /// Run a configured action here on another machine's behalf and retain its operation identity.
    func runWakeAction(_ actionId: String, target: String) async -> WakeActionOutcome {
        if commandableSystem == nil { await refresh(userInitiated: false) }
        let declared = actions.first { $0.id == actionId }?.isDeclaredWakePacket ?? false
        guard commandableSystem != nil else {
            return .init(sent: false, summary: "not reachable", failure: nil,
                         isDeclaredWakePacket: declared)
        }
        return await WakeAction.run(action: actionId, target: target, machineId: id, machineName: name,
                                    declaredPacket: declared, dialect: dialect, operations: operations,
                                    send: { request in
            try await self.agent.send(request, decoding: AgentActionResult.self,
                                      preferring: self.commandableSystem, route: self.lastRoute).value
        }, read: { request in
            try await self.agent.send(request, decoding: AgentOperationResult.self,
                                      preferring: self.commandableSystem, route: self.lastRoute).value
        })
    }

    /// Poll for the machine by asking the agent a real, authenticated question, backing off between
    /// tries. Never faster than every three seconds, which is the contract's floor.
    private func waitUntilAgentAnswers(timeout: TimeInterval) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        var wait: TimeInterval = 5
        while Date() < deadline {
            try? await Task.sleep(for: .seconds(wait))
            if Task.isCancelled { return false }
            if Date() >= deadline { break }
            await refresh(userInitiated: false, duringAction: true)
            if isAwake { return true }
            wait = min(wait * 1.6, 20)
        }
        return false
    }

    func requestSleep() {
        guard commandableSystem != nil else { return }
        offerDisruptive(
            id: "sleep-\(machine.id)",
            title: "Put \(machine.name) to sleep?",
            body: {
                if let reason = busyReason {
                    return "\(machine.name) is working right now (\(reason)). Sleeping interrupts it and loses the work in progress."
                }
                return canWake
                    ? "\(machine.name) suspends to memory now. Wake on LAN stays armed, so the Wake button brings it back."
                    : "\(machine.name) suspends to memory now."
            }(),
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
                if let reason = busyReason {
                    return "\(machine.name) is working right now (\(reason)). Rebooting into \(target.name) interrupts it and loses the work in progress."
                }
                return "\(machine.name) will reboot now and come back up in \(target.name). This takes about a minute."
            }(),
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

    func requestRestart(_ service: ServiceStatus) {
        let name = service.displayName
        offerDisruptive(
            id: "restart-\(machine.id)-\(service.id)",
            title: "Restart \(name)?",
            body: {
                if let reason = busyReason {
                    return "\(name) stops and starts again. \(machine.name) looked busy at the last reading (\(reason)), so the restart is checked against the machine as it is now and held back if something is still running."
                }
                return "\(name) stops and starts again. Anything in flight is dropped."
            }(),
            confirmTitle: "Restart",
            // Never forced from here, however busy the last reading looked. Force is the agent's own
            // check being skipped, and it is only offered back after the agent has looked at the
            // machine as it is now and said no.
            now: { [weak self] in self?.restart(service, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.restart(service, force: false, whenIdle: true) }
        )
    }

    func restart(_ service: ServiceStatus, force: Bool, whenIdle: Bool) {
        let name = service.displayName
        let dialect = dialect
        let serviceId = serviceArgument(service)
        perform("Restarting \(name)", kind: .restart, subject: name) { operation in
            await self.dispatch(
                operation: operation,
                subject: name,
                verb: "The restart of \(name)",
                build: { intent in try AgentRequest.restart(service: serviceId, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "restarted":
                        self.setStatus("\(name) restarted.", detail: result.message)
                        return .init(state: .succeeded, summary: "\(name) restarted on \(self.machine.name).",
                                     detail: result.message, refresh: true)
                    case "deferred":
                        return self.deferred(result, subject: name, verb: "The restart", force: force) {
                            self.restart(service, force: true, whenIdle: false)
                        }
                    default:
                        return self.otherOutcome(result, subject: name, verb: "The restart of \(name)")
                    }
                }
            )
        }
    }

    /// Deliberately NOT forced, and deliberately not judged from what we last read.
    func requestUpdate(_ service: ServiceStatus) {
        update(service, force: false, whenIdle: false)
    }

    func requestUpdateWhenIdle(_ service: ServiceStatus) {
        update(service, force: false, whenIdle: true)
    }

    func update(_ service: ServiceStatus, force: Bool, whenIdle: Bool) {
        let name = service.displayName
        let dialect = dialect
        let serviceId = serviceArgument(service)
        perform("Updating \(name)", kind: .update, subject: name, targetVersion: service.latest) { operation in
            await self.dispatch(
                operation: operation,
                subject: name,
                verb: "The update of \(name)",
                build: { intent in try AgentRequest.update(service: serviceId, intent: intent, dialect: dialect) },
                // A person pressed the button. A manual request ignores the schedule entirely, which
                // is finding 06: "automatic updates off" is a statement about the schedule and never
                // a refusal to ever be updated. No flag is needed for that in v3; it is what a
                // request that is not the cycle means.
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in self.interpretUpdate(result, service: service, force: force) }
            )
        }
    }

    private func interpretUpdate(_ result: AgentActionResult, service: ServiceStatus, force: Bool) -> ActionResolution {
        let name = service.displayName
        let from = result.from ?? "the installed version"
        let to = result.to ?? "the latest build"
        switch result.action {
        case "updated":
            setStatus("\(name) updated from \(from) to \(to).", detail: result.message)
            return .init(state: .succeeded, summary: "\(name) updated from \(from) to \(to).",
                         detail: result.message, refresh: true)
        case "noop":
            // Every "nothing happened" used to read as "already on the latest build", which is one of
            // at least five things it can mean and the least consequential of them. The agent's own
            // reason code decides the sentence now, and where there is none the app says so rather
            // than inventing one.
            let reason = result.reason
            let sentence = reason?.sentence(subject: name, message: result.message)
                ?? result.message
                ?? "Nothing was installed, and the agent did not say why."
            setStatus(sentence, isError: !(reason?.isBenign ?? false), detail: result.message)
            return .init(state: .noop, summary: sentence, detail: result.message, refresh: true)
        case "deferred":
            return deferred(result, subject: name, verb: "The update", force: force) { [weak self] in
                self?.update(service, force: true, whenIdle: false)
            }
        case "rolled-back":
            setStatus("The update failed and the previous version was put back.", isError: true, detail: result.message)
            return .init(state: .failed, summary: "The update of \(name) failed and the previous version was put back.",
                         detail: result.message, refresh: true)
        default:
            return otherOutcome(result, subject: name, verb: "The update of \(name)")
        }
    }

    /// One cycle across every service the machine looks after, rather than only the first.
    func requestCycle() {
        ask?(PendingDialog(
            id: "cycle-\(machine.id)",
            title: "Run the maintenance cycle on \(machine.name)?",
            message: "This is the same cycle the schedule runs: every eligible service is offered an update in turn, and any that is busy or outside its window is left alone rather than interrupted.",
            confirmTitle: "Run the cycle",
            perform: { [weak self] in self?.runCycle() }
        ))
    }

    func runCycle() {
        let dialect = dialect
        perform("Running the maintenance cycle on \(machine.name)", kind: .updateAll, subject: machine.name) { operation in
            await self.dispatch(
                operation: operation,
                subject: self.machine.name,
                verb: "The maintenance cycle",
                build: { intent in try AgentRequest.cycle(intent: intent, dryRun: false, dialect: dialect) },
                intent: .init(detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, conclusion in
                    let children = conclusion.operation?.children ?? result.children ?? []
                    let updated = children.filter { $0.action == "updated" }.count
                    let held = children.filter { $0.action == "deferred" || $0.action == "queued" }.count
                    let failed = children.filter { $0.action == "failed" || $0.action == "rolled-back" }.count
                    var parts: [String] = []
                    if updated > 0 { parts.append("\(updated) updated") }
                    if held > 0 { parts.append("\(held) held back") }
                    if failed > 0 { parts.append("\(failed) failed") }
                    if parts.isEmpty { parts.append("nothing to do") }
                    let sentence = "Maintenance cycle on \(self.machine.name): \(parts.joined(separator: ", "))."
                    let detail = children.map { child in
                        "\(child.service ?? "service"): \(child.action ?? "no result")\(child.reasonCode.map { " (\($0))" } ?? "")"
                    }.joined(separator: "\n")
                    self.setStatus(sentence, isError: failed > 0, detail: detail.isEmpty ? result.message : detail)
                    return .init(state: failed == 0 ? .succeeded : .failed, summary: sentence,
                                 detail: detail.isEmpty ? result.message : detail, refresh: true)
                }
            )
        }
    }

    /// The service to name on the command line, or nil for an agent that has never heard of the flag.
    private func serviceArgument(_ service: ServiceStatus) -> String? {
        status?.reportsServices == true ? service.id : nil
    }

    // MARK: - Policy

    func setAutoUpdate(_ enabled: Bool, on system: SystemConfig, service: ServiceStatus? = nil) {
        guard currentSystem?.id == system.id else { return }
        let subject = service?.displayName ?? system.name
        let dialect = dialect
        let serviceId = service.flatMap { serviceArgument($0) }
        perform("Saving the update setting", kind: .autoUpdate, subject: subject) { _ in
            do {
                let request = try AgentRequest.autoUpdate(enabled, service: serviceId, dialect: dialect)
                let reply = try await self.agent.send(request, decoding: AgentActionResult.self,
                                                      preferring: system, route: self.lastRoute)
                let value = reply.value.updates?.automatic ?? reply.value.autoUpdate ?? enabled
                if service == nil {
                    let key = RememberedStore.key(machine: self.machine.id, system: system.id)
                    var snapshot = self.remembered[key] ?? RememberedSystem()
                    snapshot.autoUpdate = value
                    snapshot.checkedAt = Date()
                    self.remembered[key] = snapshot
                    RememberedStore.save(self.remembered)
                }
                let sentence = value
                    ? "\(subject) updates on the schedule."
                    : "\(subject) is off the schedule. You can still update it whenever you ask."
                self.setStatus(sentence, detail: reply.value.message)
                return .init(state: .succeeded, summary: sentence, detail: reply.value.message, refresh: true)
            } catch let failure as AgentFailure {
                return self.reportMutationFailure(failure, verb: "The update setting", subject: subject)
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportMutationFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The update setting", subject: subject
                )
            }
        }
    }

    /// Change the policy: automatic, paused until, and the maintenance windows. Sent as one patch, so
    /// a change to the windows never disturbs the pause and the other way round.
    func setPolicy(
        automatic: Bool?? = nil,
        pauseUntil: Date?? = nil,
        windows: [MaintenanceWindow]?? = nil,
        service: ServiceStatus?,
        describedAs description: String
    ) {
        guard let patch = AgentUpdatePolicy.patch(automatic: automatic, pauseUntil: pauseUntil, windows: windows) else { return }
        let subject = service?.displayName ?? machine.name
        let serviceId = service.flatMap { serviceArgument($0) }
        perform(description, kind: .policy, subject: subject) { _ in
            do {
                let request = try AgentRequest.policySet(service: serviceId, patch: patch)
                let reply = try await self.agent.send(request, decoding: AgentPolicyResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                guard reply.value.ok != false else {
                    let sentence = reply.value.message ?? "\(subject): the policy was not changed."
                    self.setStatus(sentence, isError: true)
                    return .init(state: .failed, summary: sentence)
                }
                let sentence = reply.value.updates.map { "\(subject) is now \($0.scheduleSummary)." }
                    ?? "\(subject): the policy was saved."
                self.setStatus(sentence)
                return .init(state: .succeeded, summary: sentence, refresh: true)
            } catch let failure as AgentFailure {
                return self.reportMutationFailure(failure, verb: "The policy change", subject: subject)
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportMutationFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The policy change", subject: subject
                )
            }
        }
    }

    func pause(for duration: TimeInterval, service: ServiceStatus?) {
        let until = Date().addingTimeInterval(duration)
        setPolicy(pauseUntil: .some(until), service: service,
                  describedAs: "Pausing scheduled updates until \(until.formatted(date: .abbreviated, time: .shortened))")
    }

    func resume(service: ServiceStatus?) {
        setPolicy(pauseUntil: .some(nil), service: service, describedAs: "Resuming scheduled updates")
    }

    func setWindows(_ windows: [MaintenanceWindow], service: ServiceStatus?) {
        setPolicy(windows: .some(windows.isEmpty ? nil : windows), service: service,
                  describedAs: windows.isEmpty ? "Clearing the maintenance window" : "Saving the maintenance window")
    }

    /// Put a service back on its inherited policy rather than giving it one of its own.
    func inheritPolicy(service: ServiceStatus) {
        setPolicy(automatic: .some(nil), pauseUntil: .some(nil), windows: .some(nil), service: service,
                  describedAs: "Following the system policy for \(service.displayName)")
    }

    // MARK: - Queue

    func cancelQueued(_ operation: AgentOperation) {
        perform("Cancelling \(operation.displayName)", kind: .queue, subject: operation.displayName) { _ in
            do {
                let request = try AgentRequest.cancel(id: operation.id)
                let reply = try await self.agent.send(request, decoding: AgentActionResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                let sentence = reply.value.ok == false
                    ? (reply.value.message ?? "The queued request could not be cancelled.")
                    : "\(operation.displayName) is no longer queued."
                self.setStatus(sentence, isError: reply.value.ok == false)
                if reply.value.ok != false { self.operations?.finish(operation.id, state: .cancelled, summary: sentence) }
                return .init(state: reply.value.ok == false ? .failed : .succeeded, summary: sentence, refresh: true)
            } catch let failure as AgentFailure {
                return self.reportMutationFailure(failure, verb: "The cancellation", subject: operation.displayName)
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportMutationFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The cancellation", subject: operation.displayName
                )
            }
        }
    }

    // MARK: - Configured actions

    func requestAction(_ action: AgentActionInfo) {
        guard let confirm = action.confirm, !confirm.isEmpty else {
            run(action, force: false, whenIdle: false)
            return
        }
        offerDisruptive(
            id: "run-\(machine.id)-\(action.id)",
            title: "\(action.displayName)?",
            body: confirm,
            confirmTitle: action.displayName,
            now: { [weak self] in self?.run(action, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.run(action, force: false, whenIdle: true) }
        )
    }

    func run(_ action: AgentActionInfo, force: Bool, whenIdle: Bool) {
        let name = action.displayName
        let dialect = dialect
        perform(name, kind: .action, subject: name) { operation in
            await self.dispatch(
                operation: operation,
                subject: name,
                verb: name,
                build: { intent in try AgentRequest.run(action: action.id, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in
                    let output = result.output?.trimmingCharacters(in: .whitespacesAndNewlines)
                    switch result.action {
                    case "ran":
                        // The output has always come back and neither app ever showed it, which made
                        // "the action ran" the whole of what anyone learned from a button whose
                        // entire purpose is to run a command and read what it said.
                        self.setStatus("\(name) ran.", detail: output?.isEmpty == false ? output : result.message)
                        return .init(state: .succeeded, summary: "\(name) ran.",
                                     detail: result.message, output: output, refresh: true)
                    case "deferred":
                        return self.deferred(result, subject: name, verb: name, force: force) {
                            self.run(action, force: true, whenIdle: false)
                        }
                    default:
                        var resolution = self.otherOutcome(result, subject: name, verb: name)
                        resolution.output = output
                        return resolution
                    }
                }
            )
        }
    }

    // MARK: - Diagnostics

    func serviceConfig(_ request: AgentRequest) async throws -> ServiceConfigReply {
        guard commandableSystem?.isRestricted != true, status?.isRestrictedSession != true else {
            throw AgentBootstrap.Failure(message: "Service setup requires an administrator connection. Restricted keys cannot edit this configuration.")
        }
        return try await agent.send(request, decoding: ServiceConfigReply.self, preferring: commandableSystem, route: lastRoute).value
    }

    func runDoctor(deep: Bool = false) {
        guard dialect.supportsDoctor else { return }
        perform(deep ? "Checking \(machine.name) thoroughly" : "Checking \(machine.name)",
                kind: .policy, subject: "diagnostics") { _ in
            do {
                let reply = try await self.agent.send(.doctor(deep: deep), decoding: AgentDoctorResult.self,
                                                      preferring: self.commandableSystem, route: self.lastRoute)
                self.doctor = reply.value
                self.doctorRanAt = Date()
                let checks = reply.value.checks ?? []
                let bad = checks.filter { $0.verdict == .failed }.count
                let warned = checks.filter { $0.verdict == .warning }.count
                let sentence = bad == 0 && warned == 0
                    ? "\(self.machine.name) checked out: \(checks.count) checks, nothing wrong."
                    : "\(self.machine.name): \(bad) failing, \(warned) worth a look, out of \(checks.count) checks."
                self.setStatus(sentence, isError: bad > 0)
                return .init(state: .succeeded, summary: sentence)
            } catch let failure as AgentFailure {
                self.setStatus(failure.message(machine: self.machine.name), isError: true, detail: failure.detailText)
                return .init(state: .failed, summary: failure.message(machine: self.machine.name), detail: failure.detailText)
            } catch {
                self.setStatus(error.localizedDescription, isError: true)
                return .init(state: .failed, summary: error.localizedDescription)
            }
        }
    }

    /// The agent's own log, for the diagnostics export and the "show recent log" action.
    func fetchLog(lines: Int = 200, operation: String? = nil) async -> [String] {
        guard dialect.supportsLogs else { return [] }
        do {
            let request = try AgentRequest.logs(lines: lines, operation: operation)
            let reply = try await agent.send(request, decoding: AgentLogsResult.self,
                                             preferring: commandableSystem, route: lastRoute)
            return reply.value.text
        } catch let failure as AgentFailure {
            return ["the agent log could not be read: \(failure.message(machine: machine.name))"]
        } catch {
            return ["the agent log could not be read: \(error.localizedDescription)"]
        }
    }

    /// The agent's whole diagnostic bundle, as it sent it.
    func fetchBundle() async -> String {
        guard dialect.supportsDoctor else { return "This agent does not support diagnostic bundles." }
        do {
            let reply = try await agent.send(.bundle(), decoding: RawJSON.self,
                                             preferring: commandableSystem, route: lastRoute)
            return reply.value.text
        } catch {
            return "The agent diagnostic bundle could not be read: \(error.localizedDescription)"
        }
    }

    /// Recent operations from the far side's own history, which outlives anything this app kept.
    func fetchHistory(limit: Int = 30) async -> [AgentOperation] {
        guard dialect.supportsHistory else { return [] }
        do {
            let reply = try await agent.send(.history(limit: limit), decoding: AgentHistoryResult.self,
                                             preferring: commandableSystem, route: lastRoute)
            return reply.value.records
        } catch {
            return []
        }
    }

    // MARK: - Trusted agent deployment

    /// Install the agent this build was made with, over ssh, after verifying its signature here.
    func installBundledAgent() {
        guard let bundle = AgentBundle.shared, let status, status.ok == true, let system = commandableSystem else { return }
        if let previous = unresolvedOperations.first(where: { $0.kind == .appUpdate && $0.subject == "control agent" }) {
            setStatus("Agent installation \(previous.id) still has an unknown outcome. Check its result before starting another installation.", isError: true)
            reconcileNow()
            return
        }
        let isLegacy = AgentBootstrap.isAuthenticatedLegacy(status)
        guard isLegacy || status.speaksRequiredContract else { return }
        let route = lastRoute
        perform("Installing agent \(bundle.version) on \(machine.name)", kind: .appUpdate,
                subject: "control agent") { operation in
            do {
                let payload = try bundle.verifiedArchive()
                self.operations?.addPhase(operation, name: "verified", detail: "signature and sha256 checked")
                if isLegacy {
                    guard let route, let layout = AgentBootstrap.chooseLayout(argv: system.agent) else {
                        return .init(state: .cancelled, summary: "The legacy agent upgrade was cancelled; no installation was attempted.")
                    }
                    self.operations?.markAgentTracked(operation)
                    do {
                        let result = try await AgentBootstrap.installRemote(payload: payload, operation: operation,
                                                                           layout: layout, system: system, target: route.target)
                        guard result.didInstallAgent else {
                            return self.otherOutcome(result, subject: "control agent", verb: "Installing the agent")
                        }
                        self.ask?(PendingDialog(id: "migrate-agent-\(self.id)", title: "Use the stable agent launcher?",
                                                message: "The upgrade completed. Update this system's command?\n\nCurrent: \(CommandArguments.text(system.agent))\nProposed: \(CommandArguments.text(layout.stableArgv))",
                                                confirmTitle: "Update command", perform: { self.migrateAgentCommand?(system.id, layout.stableArgv) }))
                        return .init(state: .succeeded, summary: "Agent \(bundle.version) was installed on \(self.machine.name).", refresh: true)
                    } catch let failure as AgentBootstrap.Failure {
                        return .init(state: failure.uncertain ? .unknown : .failed, summary: failure.message, unresolved: failure.uncertain)
                    }
                }
                return await self.dispatch(operation: operation, subject: "control agent", verb: "Installing the agent",
                                           build: { _ in try AgentRequest.selfUpdateFromStdin(operation: operation).sending(payload) },
                                           intent: .init(operationId: operation)) { result, _ in
                    guard result.didInstallAgent else {
                        return self.otherOutcome(result, subject: "control agent", verb: "Installing the agent")
                    }
                    let sentence = "Agent \(bundle.version) is installed on \(self.machine.name)."
                    self.setStatus(sentence)
                    return .init(state: .succeeded, summary: sentence, refresh: true)
                }
            } catch {
                self.setStatus(error.localizedDescription, isError: true)
                return .init(state: .failed, summary: error.localizedDescription)
            }
        }
    }

    // MARK: - Dispatching one mutation

    /// Send one mutation, follow it if the far side detached, and turn the answer into a resolution.
    ///
    /// Everything that can go wrong on the way is handled in one place, which is what stops the
    /// timeout rule from being written eight times and getting it wrong once.
    private func dispatch(
        operation: String,
        subject: String,
        verb: String,
        build: (AgentRequest.Intent) throws -> AgentRequest,
        intent: AgentRequest.Intent,
        interpret: @escaping @MainActor (AgentActionResult, OperationDriver.Conclusion) -> ActionResolution
    ) async -> ActionResolution {
        let dialect = dialect
        do {
            let request = try build(intent)
            let driver = OperationDriver(
                send: { [weak self] request in
                    guard let self else { throw CancellationError() }
                    let reply = try await self.agent.send(request, decoding: AgentActionResult.self,
                                                          preferring: self.commandableSystem, route: self.lastRoute)
                    return reply.value
                },
                readOperation: { [weak self] request in
                    guard let self else { throw CancellationError() }
                    let reply = try await self.agent.send(request, decoding: AgentOperationResult.self,
                                                          preferring: self.commandableSystem, route: self.lastRoute)
                    return reply.value
                },
                onProgress: { [weak self] op in
                    self?.operations?.note(operation, phase: op.phase, progress: op.progress?.description)
                    if let phase = op.phase {
                        self?.setStatus("\(verb): \(phase)\(op.progress?.note.map { " — \($0)" } ?? "").")
                    }
                    self?.onStateChange?()
                }
            )

            watchedOperation = operation
            defer { watchedOperation = nil }

            let conclusion = try await driver.perform(request, operationId: intent.operationId, dialect: dialect)

            if conclusion.replayed {
                // The far side already had this id and handed the record back unchanged. That is the
                // whole point of carrying one: the retry after a dropped link did not run it twice.
                setStatus("\(verb) had already been sent, so \(machine.name) returned the record it already had rather than doing it again.")
            }

            if conclusion.stillRunning {
                let sentence = "\(verb) is still running on \(machine.name). It carries on there whether this window is open or not."
                setStatus(sentence)
                return .init(state: .running, summary: sentence, unresolved: true)
            }

            let result = conclusion.result

            if result.isConflict {
                let what = result.conflict?.summary ?? "another operation"
                let sentence = "\(verb) was refused: \(what) is already running on \(machine.name)."
                setStatus(sentence, isError: true, detail: result.message)
                return .init(state: .conflict, summary: sentence, detail: result.message, refresh: true)
            }

            if result.isQueued {
                let expires = ISO8601DateFormatter.lenient.date(from: result.expiresAt ?? result.op?.expiresAt)
                let when = expires.map { ", expiring \($0.relativeDescription())" } ?? ""
                let sentence = "\(verb) is queued and runs the next time \(machine.name) is idle\(when)."
                setStatus(sentence)
                return .init(state: .queued, summary: sentence, detail: result.message,
                             refresh: true, expiresAt: expires)
            }

            return interpret(result, conclusion)
        } catch let invalid as AgentRequest.Invalid {
            setStatus(invalid.message, isError: true)
            return .init(state: .failed, summary: invalid.message)
        } catch let failure as AgentFailure {
            return reportMutationFailure(failure, verb: verb, subject: subject)
        } catch {
            return reportMutationFailure(
                AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                verb: verb, subject: subject
            )
        }
    }

    /// The agent looked at the machine as it is now and said no.
    private func deferred(
        _ result: AgentActionResult,
        subject: String,
        verb: String,
        force: Bool,
        forceAgain: @escaping @MainActor () -> Void
    ) -> ActionResolution {
        let reason = result.reason
        let sentence = reason?.sentence(subject: subject, message: result.message)
            ?? result.message
            ?? "\(machine.name) is busy."
        setStatus("\(verb) was held back: \(sentence)")

        // Force is offered, never taken, and only after the agent has refused on the strength of a
        // fresh look. It skips the busy check and nothing else: a policy refusal is not something
        // force overrides, because a manual request already ignores the schedule.
        let overridable = reason == .busy || reason == .busyUnknown || reason == nil
        if !force, overridable {
            ask?(PendingDialog(
                id: "force-\(machine.id)-\(subject)",
                title: "\(verb) \(subject) anyway?",
                message: "\(sentence)\n\nGoing ahead interrupts that work and what is in progress is lost.",
                confirmTitle: "Do it anyway",
                perform: forceAgain
            ))
        }
        return .init(state: .deferred, summary: "\(verb) was held back: \(sentence)",
                     detail: result.message, awaitingDecision: !force && overridable)
    }

    /// Everything the happy paths did not cover, said in the agent's own terms.
    private func otherOutcome(_ result: AgentActionResult, subject: String, verb: String) -> ActionResolution {
        let reason = result.reason
        let sentence = reason?.sentence(subject: subject, message: result.message)
            ?? result.message
            ?? "\(verb) did not go through."
        let state = OperationOutcome.state(for: result, stillRunning: false)
        setStatus(sentence, isError: state.isProblem, detail: result.message)
        return .init(state: state, summary: sentence, detail: result.message, refresh: true)
    }

    // MARK: - Plumbing

    /// What one action decided, handed back to `perform` so the operation record and the status line
    /// can never drift apart.
    struct ActionResolution: Sendable {
        var state: OperationState
        var summary: String
        var detail: String?
        var output: String?
        var refresh: Bool = false
        /// True when a question is on screen waiting for an answer. No refresh runs then: this action
        /// is still in flight until the closure returns, and `perform` drops anything started while
        /// one is running, so the answer would be swallowed.
        var awaitingDecision: Bool = false
        /// True when the app gave up watching an operation that is still going. The record stays
        /// owed and the background reconciliation picks it up.
        var unresolved: Bool = false
        var expiresAt: Date?

        init(state: OperationState, summary: String, detail: String? = nil, output: String? = nil,
             refresh: Bool = false, awaitingDecision: Bool = false, unresolved: Bool = false,
             expiresAt: Date? = nil) {
            self.state = state
            self.summary = summary
            self.detail = detail
            self.output = output
            self.refresh = refresh && !awaitingDecision
            self.awaitingDecision = awaitingDecision
            self.unresolved = unresolved
            self.expiresAt = expiresAt
        }
    }

    /// The one place a mutation failure becomes words, so the timeout rule is written once.
    private func reportMutationFailure(_ failure: AgentFailure, verb: String, subject: String) -> ActionResolution {
        switch failure.dispatch {
        case .never:
            let sentence = "\(verb) did not happen: \(failure.message(machine: machine.name))"
            setStatus(sentence, isError: true, detail: failure.detailText)
            return .init(state: .failed, summary: sentence, detail: failure.detailText)
        case .unknown, .acknowledged:
            // The one that used to be guessed at. A timeout or a dropped link says nothing about
            // whether the far side did the work, and this app has no business claiming either way.
            // The record stays owed and the next status settles it.
            let sentence = "\(verb) may or may not have happened: \(failure.message(machine: machine.name))"
            setStatus(sentence, isError: true, detail: failure.detailText)
            return .init(state: .unknown, summary: sentence, detail: failure.detailText,
                         refresh: true, unresolved: true)
        }
    }

    /// A disruptive request, with the third answer the app used to be missing: neither interrupt the
    /// work nor give up, but wait for the machine to be idle.
    private func offerDisruptive(
        id: String,
        title: String,
        body: String,
        confirmTitle: String,
        now: @escaping @MainActor () -> Void,
        whenIdle: @escaping @MainActor () -> Void
    ) {
        ask?(PendingDialog(
            id: id,
            title: title,
            message: body + ((bindings?().isSelf(machine.id) == true && (id.hasPrefix("boot-") || id.hasPrefix("sleep-")))
                ? "\n\nThis device is the controller; the outcome is read from the operation record after it comes back." : ""),
            confirmTitle: confirmTitle,
            perform: now,
            alternativeTitle: dialect.supportsQueue ? "When idle" : nil,
            alternative: dialect.supportsQueue ? whenIdle : nil
        ))
    }

    /// Start one action: write it down, run it, and write down what it ended in.
    private func perform(
        _ label: String,
        kind: OperationKind,
        subject: String,
        targetVersion: String? = nil,
        _ work: @escaping @MainActor (_ operationId: String) async -> ActionResolution
    ) {
        guard activity == nil else { return }
        activity = label
        lastActionOutcome = nil
        setStatus(label + ".")

        let record = OperationRecord(
            id: AgentToken.newOperationId(),
            machineId: machine.id,
            machineName: machine.name,
            systemId: commandableSystem?.id,
            systemName: currentSystem?.name,
            kind: kind,
            subject: subject,
            targetVersion: targetVersion,
            summary: label + ".",
            // Only an agent that understands operation ids can be asked about one afterwards.
            agentTracked: dialect.supportsOperations
        )
        operations?.begin(record)
        onStateChange?()

        Task { @MainActor in
            let resolution = await work(record.id)
            if resolution.state == .queued {
                self.operations?.markQueued(record.id, expiresAt: resolution.expiresAt, summary: resolution.summary)
            } else {
                self.operations?.finish(
                    record.id,
                    state: resolution.state,
                    summary: resolution.summary,
                    detail: resolution.detail,
                    output: resolution.output,
                    route: self.lastRoute?.label,
                    forceUnresolved: resolution.unresolved
                )
            }
            if resolution.refresh { await self.refreshQuietly() }
            self.activity = nil
            self.lastActionOutcome = ActionOutcome(
                text: resolution.summary,
                isError: resolution.state.isProblem,
                operationId: record.id
            )
            if let finished = self.operations?.record(id: record.id) {
                self.onOperationFinished?(finished)
            }
            self.onStateChange?()
        }
    }

    private func setStatus(_ line: String, isError: Bool = false, detail: String? = nil) {
        statusIsRefreshFailure = false
        statusLine = line
        statusIsError = isError
        statusDetail = detail
        statusStamp = Date()
    }

    /// Re-reads the status after an action without disturbing the sentence the action just wrote.
    private func refreshQuietly() async {
        let line = statusLine
        let isError = statusIsError
        let detail = statusDetail
        await refresh(userInitiated: false, duringAction: true)
        setStatus(line, isError: isError, detail: detail)
    }
}
