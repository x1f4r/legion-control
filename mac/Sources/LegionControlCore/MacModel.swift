import Foundation
import Observation

/// This Mac as a machine of its own.
///
/// The same agent and the same JSON as anywhere else, but it runs here rather than over ssh. There
/// is nothing to wake and nothing to boot, because the device the app runs on is by definition up.
/// Everything else a remote machine offers is offered here too: restart, configured actions, the
/// update policy, queue-until-idle, doctor and logs. It used to have a single Update button and
/// nothing else, which made the one machine you are sitting in front of the one you could do least
/// to.
@MainActor
@Observable
final class MacModel {
    private(set) var config: LocalConfig
    private(set) var status: AgentStatus?
    private(set) var failure: AgentFailure?
    private(set) var lastChecked: Date?
    private(set) var isRefreshing = false
    private(set) var activity: String?
    /// The sentence the local section shows under its buttons after an action.
    private(set) var note: String?
    private(set) var noteIsError = false
    private(set) var noteStamp = Date.distantPast
    private(set) var doctor: AgentDoctorResult?
    private(set) var doctorRanAt: Date?
    private(set) var watchedOperation: String?

    private var agent: MacAgent
    private var lastRefreshFinished: Date?
    /// The document last handed to the local agent, when, and what it ended in.
    private var lastShare: (hash: String, at: Date, failure: String?)?
    private var isSharing = false
    private var lastReconcileAttempt: Date = .distantPast

    private static let shareRetry: TimeInterval = 600
    private static let reconcileEvery: TimeInterval = 60

    /// The id this machine is filed under in the operation history. The local Mac has no entry in
    /// the machines list, so it needs one of its own.
    static let machineId = "local"

    var onStateChange: (@MainActor () -> Void)?
    var ask: (@MainActor (PendingDialog) -> Void)?
    /// The setup this Mac is running on. The local agent gets a copy of it exactly as the machines
    /// over ssh do: the file is the source of truth, and this Mac is not an exception to that.
    var controllerConfig: (@MainActor () -> ControllerDocument?)?
    var operations: OperationStore?
    var onOperationFinished: (@MainActor (OperationRecord) -> Void)?
    /// Adopt a document read off the local agent.
    var adoptSetup: (@MainActor (Data, String) -> String?)?
    /// The machine in the shared setup this device is, when the private bindings say so. Nil when
    /// this is only "the agent that happens to run here".
    var boundMachineId: String?
    var boundAgentArgv: [String]?
    var migrateAgentCommand: (@MainActor ([String]) -> Void)?
    /// The corresponding shared machine, including its boot targets. Private bindings decide that
    /// this is self; the shared setup still describes what the hardware can do.
    var boundMachine: Machine?
    var setupBase: (@MainActor (ControllerIdentity, ControllerIdentity) -> Data?)?
    var onSetupDiverged: (@MainActor (MachineModel.SetupDivergence) -> Void)?

    init(config: LocalConfig) {
        self.config = config
        self.agent = MacAgent(scriptPath: config.agentPath)
    }

    /// For the tests.
    init(config: LocalConfig, agent: MacAgent) {
        self.config = config
        self.agent = agent
    }

    var name: String { config.name }

    var historyMachineId: String { boundMachineId ?? Self.machineId }

    var isWorking: Bool { activity != nil }

    var isReachable: Bool { status != nil }

    var dialect: AgentDialect { AgentDialect(status) }

    var needsAgentUpgrade: Bool {
        guard let status, status.ok != false else { return false }
        return !status.speaksRequiredContract
    }

    var services: [ServiceStatus] { status?.resolvedServices ?? [] }

    func service(id: String) -> ServiceStatus? { status?.service(id: id) }

    /// The named actions this Mac's agent offers, exactly as a remote machine's does. This is the
    /// parity the review asked for: the same capabilities and the same safety behaviour.
    var actions: [AgentActionInfo] { (status?.actions ?? []).filter { !$0.id.isEmpty } }

    func runWakeAction(_ action: String, target: String) async -> MachineModel.WakeActionOutcome {
        if !isReachable { await refresh() }
        let declared = actions.first { $0.id == action }?.isDeclaredWakePacket ?? false
        guard isReachable else {
            return .init(sent: false, summary: "local agent unavailable", failure: nil, isDeclaredWakePacket: declared)
        }
        return await WakeAction.run(action: action, target: target, machineId: historyMachineId, machineName: name,
                                    declaredPacket: declared, dialect: dialect, operations: operations,
                                    send: { try await self.agent.send($0, decoding: AgentActionResult.self) },
                                    read: { try await self.agent.send($0, decoding: AgentOperationResult.self) })
    }

    func actions(for service: ServiceStatus) -> [AgentActionInfo] {
        (service.actions ?? []).filter { !$0.id.isEmpty }
    }

    var notes: [String] { status?.allNotes ?? [] }

    var policy: AgentUpdatePolicy? { status?.updates }

    var queuedOperations: [AgentOperation] { status?.queuedOperations ?? [] }
    var runningOperations: [AgentOperation] { status?.runningOperations ?? [] }
    var recentOperations: [AgentOperation] { status?.recentOperations ?? [] }

    var currentBoundSystem: SystemConfig? {
        guard let machine = boundMachine, let status else { return nil }
        if let id = status.system?.id, let configured = machine.system(id: id) { return configured }
        if let platform = status.platform { return machine.system(platform: platform) }
        return nil
    }

    var bootTargets: [SystemConfig] {
        guard let machine = boundMachine else { return [] }
        let current = currentBoundSystem?.id
        let others = machine.systems.filter { $0.id != current }
        guard let reported = status?.bootTargets else { return others }
        let ids = Set(reported.map(\.id))
        return others.filter { ids.contains($0.id) }
    }

    var isBusy: Bool { status?.busy?.isBusy ?? false }

    var busyReason: String? {
        guard let busy = status?.busy, busy.isBusy else { return nil }
        return busy.summary
    }

    var autoUpdate: Bool? { status?.updates?.automatic ?? status?.autoUpdate }

    var unresolvedOperations: [OperationRecord] {
        (operations?.unresolved ?? []).filter { $0.machineId == historyMachineId }
    }

    var watchedRecord: OperationRecord? {
        guard let watchedOperation else { return nil }
        return operations?.record(id: watchedOperation)
    }

    /// What this device last decided about the copy its own agent is holding.
    private(set) var setupDecision: SetupSync.Decision = .unknown
    private(set) var remoteSetupIdentity: ControllerIdentity?
    private var remoteLineageForHash: String?

    /// What the Setup row says about the copy this device's own agent is holding.
    ///
    /// The local agent is a replica like any other. It is not privileged, it is not the source of
    /// truth, and it gets the same fast-forward-or-ask treatment as a machine across the world.
    var setupSharing: SetupSharing {
        if let lastShare, let local = controllerConfig?(), lastShare.hash == local.hash,
           let failure = lastShare.failure {
            return .failed(failure)
        }
        return SetupSync.sharing(
            for: setupDecision,
            machine: config.name,
            local: controllerConfig?(),
            remote: remoteSetupIdentity ?? status?.controller?.identity
        )
    }

    /// Why the update button for one service on this Mac can do nothing, or nil when there really is
    /// something to do.
    func updateUnavailableReason(_ service: ServiceStatus) -> String? {
        guard isReachable else { return "This Mac could not be read, so nothing can be applied." }
        return service.updateUnavailableReason
    }

    func restartUnavailableReason(_ service: ServiceStatus) -> String? {
        guard isReachable else { return "This Mac could not be read, so nothing can be restarted." }
        if service.canRestart == false { return "\(service.displayName) cannot be restarted from here." }
        return nil
    }

    func actionUnavailableReason() -> String? {
        guard isReachable else { return "This Mac could not be read, so its actions cannot be run." }
        return nil
    }

    /// The second line of the sidebar entry for this Mac.
    var sidebarSummary: String {
        if failure != nil { return "not readable" }
        guard !services.isEmpty else { return "checking" }
        if !unresolvedOperations.isEmpty { return "outcome not known yet" }
        if services.contains(where: { $0.hasStagedUpdate }) { return "update waiting" }
        if services.contains(where: { $0.upToDate == false }) { return "update available" }
        if let reason = busyReason { return reason }
        if services.allSatisfy({ $0.upToDate == true }) { return "up to date" }
        return "version not checked"
    }

    // MARK: - Reading

    func refreshIfNeeded(minimumAge: TimeInterval = 2) async {
        if let lastRefreshFinished, Date().timeIntervalSince(lastRefreshFinished) < minimumAge { return }
        await refresh()
    }

    func refresh(duringAction: Bool = false) async {
        guard !isRefreshing, duringAction || activity == nil else { return }
        isRefreshing = true
        defer {
            isRefreshing = false
            lastRefreshFinished = Date()
            onStateChange?()
        }

        do {
            let reply = try await agent.status(dialect: dialect)
            guard reply.ok != false else {
                status = nil
                failure = AgentFailure(.agentFailed, detail: reply.message ?? "the agent reported a failure",
                                       dispatch: reply.reasonCode == "restricted" ? .never : .acknowledged)
                lastChecked = Date()
                return
            }
            status = reply
            failure = nil
            lastChecked = Date()
            reconcileSetup()
            reconcileIfNeeded()
        } catch let error as AgentFailure {
            status = nil
            failure = error
            lastChecked = Date()
        } catch {
            status = nil
            failure = AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown)
            lastChecked = Date()
        }
    }

    private func reconcileIfNeeded() {
        guard let operations, dialect.supportsOperations else { return }
        guard Date().timeIntervalSince(lastReconcileAttempt) > Self.reconcileEvery else { return }
        let owed = operations.unresolved.filter { $0.machineId == historyMachineId && $0.agentTracked }
        guard let next = owed.first else { return }
        lastReconcileAttempt = Date()

        if let known = status?.operation(id: next.id) {
            operations.reconcile(next.id, with: known)
            onStateChange?()
            return
        }
        Task { @MainActor in
            do {
                let request = try AgentRequest.operation(id: next.id, wait: nil)
                let reply = try await self.agent.send(request, decoding: AgentOperationResult.self)
                if let operation = reply.operation {
                    operations.reconcile(next.id, with: operation)
                } else if reply.isUnknownOperation {
                    operations.resolveAsNeverStarted(next.id)
                }
                self.onStateChange?()
            } catch {
                // Still owed. The next poll asks again.
            }
        }
    }

    func reconcileNow() {
        lastReconcileAttempt = .distantPast
        reconcileIfNeeded()
    }

    func dismissUnknownOutcome(_ id: String) {
        operations?.dismissReconciliation(id)
        onStateChange?()
    }

    // MARK: - Reconciling the setup

    private func reconcileSetup() {
        guard activity == nil, !isSharing else { return }
        guard let status else { return }
        let local = controllerConfig?()

        let view = SetupSync.MachineView(
            reportsCopy: status.reportsControllerCopy,
            contract: status.contractVersion,
            hash: status.controllerHash,
            identity: (remoteLineageForHash == status.controllerHash ? remoteSetupIdentity : nil)
                ?? status.controller?.identity,
            lineage: remoteLineageForHash == status.controllerHash ? (remoteSetupIdentity?.lineage ?? []) : nil
        )
        let decision = SetupSync.decide(local: local, machine: view)
        setupDecision = decision

        switch decision {
        case .inSync, .unsupported, .unknown, .diverged, .differentSetup:
            return
        case .push:
            guard let local else { return }
            if let lastShare, lastShare.hash == local.hash,
               Date().timeIntervalSince(lastShare.at) < SetupSync.retryAfter { return }
            publish(local)
        case .fetch:
            fetchSetup()
        case .readMeta:
            readRemoteSetupMeta()
        }
    }

    private func publish(_ local: ControllerDocument) {
        isSharing = true
        let dialect = dialect
        Task { @MainActor in
            var failure: String?
            do {
                let request = try AgentRequest.configSet(local, replace: false, dialect: dialect)
                let result = try await self.agent.send(request, decoding: AgentConfigResult.self)
                if result.ok == false {
                    self.remoteSetupIdentity = result.identity
                    self.remoteLineageForHash = result.storedHash
                    failure = result.reason?.sentence(subject: "the setup", message: result.error ?? result.message)
                        ?? result.error ?? "the agent kept the copy it had."
                } else if let stored = result.storedHash, stored != local.hash {
                    failure = "the agent stored something other than the document it was given."
                }
            } catch let error as AgentFailure {
                failure = error.detailText ?? error.localMessage
            } catch let invalid as AgentRequest.Invalid {
                failure = invalid.message
            } catch {
                failure = error.localizedDescription
            }

            let sentence = failure.map { "The setup could not be shared with \(self.config.name): \($0)" }
            self.lastShare = (hash: local.hash, at: Date(), failure: sentence)
            if let sentence { self.setNote(sentence, isError: true) }
            self.isSharing = false
            self.onStateChange?()
        }
    }

    private func fetchSetup() {
        isSharing = true
        Task { @MainActor in
            defer {
                self.isSharing = false
                self.onStateChange?()
            }
            guard let fetched = await self.readRemoteDocument() else { return }
            if let problem = self.adoptSetup?(fetched.bytes, "adopted from the local agent") {
                self.setNote("The setup held here could not be adopted: \(problem)", isError: true)
            } else {
                self.setNote("Adopted revision \(fetched.identity.revisionNumber) from the local agent.", isError: false)
            }
        }
    }

    private func readRemoteDocument() async -> (bytes: Data, identity: ControllerIdentity)? {
        do {
            let reply = try await agent.send(.configRead(), decoding: RemoteControllerDocument.self)
            guard let document = reply.documentBytes else {
                setNote("The local agent did not return a setup document.", isError: true)
                return nil
            }
            let canonical = try Canonical.bytes(document)
            let hash = Canonical.sha256(canonical)
            if let reported = reply.hash, reported != hash {
                setNote("The local agent sent a setup whose hash does not match what it reports, so it was not adopted.",
                        isError: true)
                return nil
            }
            return (canonical, reply.meta?.identity ?? ControllerIdentity())
        } catch {
            return nil
        }
    }

    private func readRemoteSetupMeta() {
        guard let hash = status?.controllerHash, remoteLineageForHash != hash else { return }
        isSharing = true
        Task { @MainActor in
            defer {
                self.isSharing = false
                self.onStateChange?()
            }
            do {
                let result = try await self.agent.send(.configMeta(), decoding: AgentConfigResult.self)
                self.remoteSetupIdentity = result.identity
                self.remoteLineageForHash = result.storedHash ?? hash
                self.reconcileSetup()
            } catch {
                // Nothing decided. The next poll asks again.
            }
        }
    }

    /// Adopt what this device's own agent holds, dropping the branch in the config file.
    func adoptRemoteSetup() {
        Task { @MainActor in
            guard let fetched = await self.readRemoteDocument() else { return }
            if let problem = self.adoptSetup?(fetched.bytes, "took the setup from the local agent") {
                self.setNote(problem, isError: true)
            } else {
                self.setNote("This device now uses the setup its own agent was holding.", isError: false)
                self.remoteSetupIdentity = nil
                self.remoteLineageForHash = nil
                self.setupDecision = .unknown
            }
            self.onStateChange?()
        }
    }

    /// Gather the local agent's branch for the same merge screen used by every remote machine.
    func openSetupDivergence() {
        guard let local = controllerConfig?() else { return }
        let isDifferent = setupDecision == .differentSetup
        guard isDifferent || setupDecision == .diverged else { return }
        Task { @MainActor in
            guard let fetched = await self.readRemoteDocument() else { return }
            let base = self.setupBase?(local.identity, fetched.identity)
            let differences = (try? SetupMerge.differences(mine: local.bytes, theirs: fetched.bytes,
                                                           base: base)) ?? []
            self.onSetupDiverged?(MachineModel.SetupDivergence(
                machineId: self.historyMachineId,
                machineName: self.config.name,
                mine: local,
                theirsBytes: fetched.bytes,
                theirsIdentity: fetched.identity,
                baseBytes: base,
                differences: differences,
                isDifferentSetup: isDifferent
            ))
        }
    }

    func requestSetupReplacement() {
        guard case .conflict(let reason) = setupSharing else { return }
        ask?(PendingDialog(
            id: "replace-setup-local",
            title: "Replace the setup this device's agent is holding?",
            message: "\(reason)\n\nReplacing it overwrites the copy the agent has with the document this app is running on.",
            confirmTitle: "Replace it",
            perform: { [weak self] in self?.replaceSetup() }
        ))
    }

    func replaceSetup() {
        guard let local = controllerConfig?() else { return }
        let dialect = dialect
        perform("Replacing the setup on \(config.name)", kind: .shareSetup, subject: "setup") { _ in
            do {
                let request = try AgentRequest.configSet(local, replace: true, dialect: dialect)
                let result = try await self.agent.send(request, decoding: AgentConfigResult.self)
                if result.ok == false {
                    let sentence = result.error ?? result.message ?? "the agent refused the document."
                    self.setNote("The setup was not replaced: \(sentence)", isError: true)
                    return .init(state: .failed, summary: sentence)
                }
                self.lastShare = (hash: local.hash, at: Date(), failure: nil)
                self.remoteSetupIdentity = nil
                self.remoteLineageForHash = nil
                let sentence = "\(self.config.name) now holds revision \(local.identity.revisionNumber)."
                self.setNote(sentence, isError: false)
                return .init(state: .succeeded, summary: sentence)
            } catch let error as AgentFailure {
                return self.reportFailure(error, verb: "Replacing the setup")
            } catch let invalid as AgentRequest.Invalid {
                self.setNote(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "Replacing the setup"
                )
            }
        }
    }

    // MARK: - Power for a bound self machine

    func requestSleep() {
        guard boundMachine != nil, isReachable else { return }
        let reason = busyReason
        offerDisruptive(
            id: "sleep-self-\(historyMachineId)",
            title: "Put this device to sleep?",
            body: (reason.map { "This device is working right now (\($0)). Sleeping interrupts it and loses the work in progress.\n\n" } ?? "")
                + "This device is the controller; the outcome is read from the operation record after it comes back.",
            confirmTitle: reason == nil ? "Sleep now" : "Sleep anyway",
            now: { [weak self] in self?.sleep(force: reason != nil, whenIdle: false) },
            whenIdle: { [weak self] in self?.sleep(force: false, whenIdle: true) }
        )
    }

    func sleep(force: Bool, whenIdle: Bool) {
        guard boundMachine != nil, isReachable else { return }
        let dialect = dialect
        perform("Putting this device to sleep", kind: .sleep, subject: name) { operation in
            await self.dispatch(
                operation: operation,
                subject: self.name,
                verb: "The sleep",
                build: { intent in try AgentRequest.sleep(intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "sleeping", "slept":
                        let sentence = "Sleep was acknowledged. Its operation record will be read after this device comes back."
                        self.setNote(sentence, isError: false)
                        return .init(state: .unknown, summary: sentence, detail: result.message, unresolved: true)
                    case "deferred":
                        return self.deferred(result, subject: self.name, verb: "Sleep", force: force) {
                            self.sleep(force: true, whenIdle: false)
                        }
                    default:
                        return self.otherOutcome(result, subject: self.name, verb: "The sleep")
                    }
                }
            )
        }
    }

    func requestBoot(into target: SystemConfig) {
        guard boundMachine != nil, isReachable else { return }
        let reason = busyReason
        offerDisruptive(
            id: "boot-self-\(historyMachineId)-\(target.id)",
            title: "Boot this device into \(target.name)?",
            body: (reason.map { "This device is working right now (\($0)). Rebooting interrupts it and loses the work in progress.\n\n" } ?? "")
                + "This device is the controller; the outcome is read from the operation record after it comes back.",
            confirmTitle: reason == nil ? "Reboot now" : "Switch anyway",
            now: { [weak self] in self?.boot(into: target, force: reason != nil, whenIdle: false) },
            whenIdle: { [weak self] in self?.boot(into: target, force: false, whenIdle: true) }
        )
    }

    func boot(into target: SystemConfig, force: Bool, whenIdle: Bool) {
        guard boundMachine != nil, isReachable else { return }
        let dialect = dialect
        perform("Switching this device to \(target.name)", kind: .boot, subject: target.name) { operation in
            await self.dispatch(
                operation: operation,
                subject: target.name,
                verb: "The switch to \(target.name)",
                build: { intent in try AgentRequest.boot(target: target.id, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "rebooting", "rebooted":
                        let sentence = "The reboot was acknowledged. Its operation record will be read after this device comes back."
                        self.setNote(sentence, isError: false)
                        return .init(state: .unknown, summary: sentence, detail: result.message, unresolved: true)
                    case "armed":
                        let sentence = "The next boot is set to \(target.name). Nothing has rebooted yet."
                        self.setNote(sentence, isError: false)
                        return .init(state: .succeeded, summary: sentence, detail: result.message)
                    case "noop":
                        let sentence = "\(target.name) is already running."
                        self.setNote(sentence, isError: false)
                        return .init(state: .noop, summary: sentence, detail: result.message)
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

    // MARK: - Updating

    /// The local update is disruptive in a way a remote one is not: it can close the app the user is
    /// sitting in front of. It always asks first, even when nothing is running.
    func requestUpdate(_ service: ServiceStatus) {
        let name = service.displayName
        let staged = service.stagedVersion
        offerDisruptive(
            id: "local-update-\(service.id)",
            title: "Update \(name) on \(config.name)?",
            body: (staged.map { "Build \($0) is downloaded and waiting. " } ?? "")
                + "\(name) quits and starts again to apply it, so anything open in it goes with it. "
                + "If something is running, this Mac holds the update back instead and nothing is interrupted.",
            confirmTitle: "Quit and update",
            now: { [weak self] in self?.update(service, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.update(service, force: false, whenIdle: true) }
        )
    }

    func requestUpdateWhenIdle(_ service: ServiceStatus) {
        update(service, force: false, whenIdle: true)
    }

    func update(_ service: ServiceStatus, force: Bool, whenIdle: Bool) {
        let name = service.displayName
        let dialect = dialect
        let serviceId = serviceArgument(service)
        perform("Updating \(name) on \(config.name)", kind: .update, subject: name,
                targetVersion: service.latest) { operation in
            await self.dispatch(
                operation: operation,
                subject: name,
                verb: "The update of \(name)",
                build: { intent in try AgentRequest.update(service: serviceId, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in
                    let from = result.from ?? "the installed version"
                    let to = result.to ?? "the staged build"
                    switch result.action {
                    case "updated":
                        let sentence = "\(name) went from \(from) to \(to) and was started again."
                        self.setNote(sentence, isError: false)
                        return .init(state: .succeeded, summary: sentence, detail: result.message)
                    case "noop":
                        let reason = result.reason
                        let sentence = reason?.sentence(subject: name, message: result.message)
                            ?? result.message
                            ?? "Nothing was installed, and the agent did not say why."
                        self.setNote(sentence, isError: !(reason?.isBenign ?? false))
                        return .init(state: .noop, summary: sentence, detail: result.message)
                    case "deferred":
                        return self.deferred(result, subject: name, verb: "The update", force: force) {
                            self.update(service, force: true, whenIdle: false)
                        }
                    case "downloaded", "staged":
                        let sentence = result.message ?? "The build was downloaded and is waiting for \(name) to quit."
                        self.setNote(sentence, isError: false)
                        return .init(state: .succeeded, summary: sentence)
                    default:
                        return self.otherOutcome(result, subject: name, verb: "The update of \(name)")
                    }
                }
            )
        }
    }

    func requestCycle() {
        ask?(PendingDialog(
            id: "local-cycle",
            title: "Run the maintenance cycle on \(config.name)?",
            message: "Every service this Mac looks after is offered an update in turn. Anything that is busy or outside its window is left alone rather than interrupted.",
            confirmTitle: "Run the cycle",
            perform: { [weak self] in self?.runCycle() }
        ))
    }

    func runCycle() {
        let dialect = dialect
        perform("Running the maintenance cycle on \(config.name)", kind: .updateAll, subject: config.name) { operation in
            await self.dispatch(
                operation: operation,
                subject: self.config.name,
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
                    let sentence = "Maintenance cycle on \(self.config.name): \(parts.joined(separator: ", "))."
                    self.setNote(sentence, isError: failed > 0)
                    return .init(state: failed == 0 ? .succeeded : .failed, summary: sentence, detail: result.message)
                }
            )
        }
    }

    // MARK: - Restarting

    func requestRestart(_ service: ServiceStatus) {
        let name = service.displayName
        offerDisruptive(
            id: "local-restart-\(service.id)",
            title: "Restart \(name) on \(config.name)?",
            body: {
                if let reason = busyReason {
                    return "\(name) stops and starts again. This Mac looked busy at the last reading (\(reason)), so the restart is checked against it as it is now and held back if something is still running."
                }
                return "\(name) stops and starts again. Anything in flight is dropped."
            }(),
            confirmTitle: "Restart",
            now: { [weak self] in self?.restart(service, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.restart(service, force: false, whenIdle: true) }
        )
    }

    func restart(_ service: ServiceStatus, force: Bool, whenIdle: Bool) {
        let name = service.displayName
        let dialect = dialect
        let serviceId = serviceArgument(service)
        perform("Restarting \(name) on \(config.name)", kind: .restart, subject: name) { operation in
            await self.dispatch(
                operation: operation,
                subject: name,
                verb: "The restart of \(name)",
                build: { intent in try AgentRequest.restart(service: serviceId, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "restarted":
                        self.setNote("\(name) restarted.", isError: false)
                        return .init(state: .succeeded, summary: "\(name) restarted.", detail: result.message)
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

    // MARK: - Configured actions

    func requestAction(_ action: AgentActionInfo) {
        guard let confirm = action.confirm, !confirm.isEmpty else {
            run(action, force: false, whenIdle: false)
            return
        }
        offerDisruptive(
            id: "local-run-\(action.id)",
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
                        self.setNote(output?.isEmpty == false ? "\(name) ran. \(output!)" : "\(name) ran.", isError: false)
                        return .init(state: .succeeded, summary: "\(name) ran.", detail: result.message, output: output)
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

    // MARK: - Policy

    func setAutoUpdate(_ enabled: Bool, service: ServiceStatus? = nil) {
        let subject = service?.displayName ?? config.name
        let dialect = dialect
        let serviceId = service.flatMap { serviceArgument($0) }
        perform("Saving the update setting for \(subject)", kind: .autoUpdate, subject: subject) { _ in
            do {
                let request = try AgentRequest.autoUpdate(enabled, service: serviceId, dialect: dialect)
                let result = try await self.agent.send(request, decoding: AgentActionResult.self)
                let value = result.updates?.automatic ?? result.autoUpdate ?? enabled
                let sentence = value
                    ? "\(subject) applies a staged build on its own, once nothing is running."
                    : "\(subject) only updates when you ask."
                self.setNote(sentence, isError: false)
                return .init(state: .succeeded, summary: sentence, detail: result.message)
            } catch let error as AgentFailure {
                return self.reportFailure(error, verb: "The update setting")
            } catch let invalid as AgentRequest.Invalid {
                self.setNote(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The update setting"
                )
            }
        }
    }

    func setPolicy(
        automatic: Bool?? = nil,
        pauseUntil: Date?? = nil,
        windows: [MaintenanceWindow]?? = nil,
        service: ServiceStatus?,
        describedAs description: String
    ) {
        guard let patch = AgentUpdatePolicy.patch(automatic: automatic, pauseUntil: pauseUntil, windows: windows) else { return }
        let subject = service?.displayName ?? config.name
        let serviceId = service.flatMap { serviceArgument($0) }
        perform(description, kind: .policy, subject: subject) { _ in
            do {
                let request = try AgentRequest.policySet(service: serviceId, patch: patch)
                let result = try await self.agent.send(request, decoding: AgentPolicyResult.self)
                guard result.ok != false else {
                    let sentence = result.message ?? "\(subject): the policy was not changed."
                    self.setNote(sentence, isError: true)
                    return .init(state: .failed, summary: sentence)
                }
                let sentence = result.updates.map { "\(subject) is now \($0.scheduleSummary)." }
                    ?? "\(subject): the policy was saved."
                self.setNote(sentence, isError: false)
                return .init(state: .succeeded, summary: sentence)
            } catch let error as AgentFailure {
                return self.reportFailure(error, verb: "The policy change")
            } catch let invalid as AgentRequest.Invalid {
                self.setNote(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The policy change"
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

    func inheritPolicy(service: ServiceStatus) {
        setPolicy(automatic: .some(nil), pauseUntil: .some(nil), windows: .some(nil), service: service,
                  describedAs: "Following the system policy for \(service.displayName)")
    }

    // MARK: - Queue

    func cancelQueued(_ operation: AgentOperation) {
        perform("Cancelling \(operation.displayName)", kind: .queue, subject: operation.displayName) { _ in
            do {
                let request = try AgentRequest.cancel(id: operation.id)
                let result = try await self.agent.send(request, decoding: AgentActionResult.self)
                let sentence = result.ok == false
                    ? (result.message ?? "The queued request could not be cancelled.")
                    : "\(operation.displayName) is no longer queued."
                self.setNote(sentence, isError: result.ok == false)
                if result.ok != false { self.operations?.finish(operation.id, state: .cancelled, summary: sentence) }
                return .init(state: result.ok == false ? .failed : .succeeded, summary: sentence)
            } catch let error as AgentFailure {
                return self.reportFailure(error, verb: "The cancellation")
            } catch let invalid as AgentRequest.Invalid {
                self.setNote(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The cancellation"
                )
            }
        }
    }

    // MARK: - Diagnostics

    func serviceConfig(_ request: AgentRequest) async throws -> ServiceConfigReply {
        try await agent.send(request, decoding: ServiceConfigReply.self)
    }

    func runDoctor(deep: Bool = false) {
        guard dialect.supportsDoctor else { return }
        perform(deep ? "Checking \(config.name) thoroughly" : "Checking \(config.name)",
                kind: .policy, subject: "diagnostics") { _ in
            do {
                let result = try await self.agent.send(.doctor(deep: deep), decoding: AgentDoctorResult.self)
                self.doctor = result
                self.doctorRanAt = Date()
                let checks = result.checks ?? []
                let bad = checks.filter { $0.verdict == .failed }.count
                let warned = checks.filter { $0.verdict == .warning }.count
                let sentence = bad == 0 && warned == 0
                    ? "\(self.config.name) checked out: \(checks.count) checks, nothing wrong."
                    : "\(self.config.name): \(bad) failing, \(warned) worth a look, out of \(checks.count) checks."
                self.setNote(sentence, isError: bad > 0)
                return .init(state: .succeeded, summary: sentence)
            } catch let error as AgentFailure {
                self.setNote(error.localMessage, isError: true)
                return .init(state: .failed, summary: error.localMessage, detail: error.detailText)
            } catch {
                self.setNote(error.localizedDescription, isError: true)
                return .init(state: .failed, summary: error.localizedDescription)
            }
        }
    }

    func fetchLog(lines: Int = 200, operation: String? = nil) async -> [String] {
        guard dialect.supportsLogs else { return [] }
        do {
            let request = try AgentRequest.logs(lines: lines, operation: operation)
            return try await agent.send(request, decoding: AgentLogsResult.self).text
        } catch let failure as AgentFailure {
            return ["the local agent log could not be read: \(failure.localMessage)"]
        } catch {
            return ["the local agent log could not be read: \(error.localizedDescription)"]
        }
    }

    func fetchBundle() async -> String {
        guard dialect.supportsDoctor else { return "This agent does not support diagnostic bundles." }
        do { return try await agent.send(.bundle(), decoding: RawJSON.self).text }
        catch { return "The local agent diagnostic bundle could not be read: \(error.localizedDescription)" }
    }

    func fetchHistory(limit: Int = 30) async -> [AgentOperation] {
        guard dialect.supportsHistory else { return [] }
        return (try? await agent.send(.history(limit: limit), decoding: AgentHistoryResult.self).records) ?? []
    }

    /// Install the bundled agent locally. Same verification, no ssh.
    func installBundledAgent() {
        guard let bundle = AgentBundle.shared, let status, status.ok == true else { return }
        if let previous = unresolvedOperations.first(where: { $0.kind == .appUpdate && $0.subject == "control agent" }) {
            setNote("Agent installation \(previous.id) still has an unknown outcome. Check its result before starting another installation.", isError: true)
            reconcileNow()
            return
        }
        let isLegacy = AgentBootstrap.isAuthenticatedLegacy(status)
        guard isLegacy || status.speaksRequiredContract else { return }
        perform("Installing agent \(bundle.version) on \(config.name)", kind: .appUpdate, subject: "control agent") { operation in
            do {
                let payload = try bundle.verifiedArchive()
                if isLegacy {
                    let argv = try await self.agent.bootstrapCommand()
                    guard let layout = AgentBootstrap.chooseLayout(argv: argv) else {
                        return .init(state: .cancelled, summary: "The legacy agent upgrade was cancelled.")
                    }
                    self.operations?.markAgentTracked(operation)
                    do {
                        let result = try await AgentBootstrap.installLocal(payload: payload, operation: operation, layout: layout)
                        guard result.didInstallAgent else {
                            return self.otherOutcome(result, subject: "control agent", verb: "Installing the agent")
                        }
                        if self.boundMachineId != nil { self.ask?(PendingDialog(id: "migrate-local-agent", title: "Use the stable agent launcher?",
                                                message: "The upgrade completed. Update this device's command?\n\nCurrent: \(CommandArguments.text(argv))\nProposed: \(CommandArguments.text(layout.stableArgv))",
                                                confirmTitle: "Update command", perform: { self.migrateAgentCommand?(layout.stableArgv) })) }
                        return .init(state: .succeeded, summary: "Agent \(bundle.version) was installed here.")
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
                    let sentence = "Agent \(bundle.version) is installed here."
                    self.setNote(sentence, isError: false)
                    return .init(state: .succeeded, summary: sentence)
                }
            } catch {
                self.setNote(error.localizedDescription, isError: true)
                return .init(state: .failed, summary: error.localizedDescription)
            }
        }
    }

    /// The same rule as over ssh: an agent that reported no services array has never heard of the
    /// flag, and its one service is the default anyway.
    private func serviceArgument(_ service: ServiceStatus) -> String? {
        status?.reportsServices == true ? service.id : nil
    }

    // MARK: - Plumbing

    typealias ActionResolution = MachineModel.ActionResolution

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
                    return try await self.agent.send(request, decoding: AgentActionResult.self)
                },
                readOperation: { [weak self] request in
                    guard let self else { throw CancellationError() }
                    return try await self.agent.send(request, decoding: AgentOperationResult.self)
                },
                onProgress: { [weak self] op in
                    self?.operations?.note(operation, phase: op.phase, progress: op.progress?.description)
                    if let phase = op.phase {
                        self?.setNote("\(verb): \(phase)\(op.progress?.note.map { " — \($0)" } ?? "").", isError: false)
                    }
                    self?.onStateChange?()
                }
            )

            watchedOperation = operation
            defer { watchedOperation = nil }

            let conclusion = try await driver.perform(request, operationId: intent.operationId, dialect: dialect)

            if conclusion.replayed {
                setNote("\(verb) had already been sent, so the agent returned the record it already had rather than doing it again.", isError: false)
            }
            if conclusion.stillRunning {
                let sentence = "\(verb) is still running. It carries on whether this window is open or not."
                setNote(sentence, isError: false)
                return .init(state: .running, summary: sentence, unresolved: true)
            }

            let result = conclusion.result
            if result.isConflict {
                let what = result.conflict?.summary ?? "another operation"
                let sentence = "\(verb) was refused: \(what) is already running on this Mac."
                setNote(sentence, isError: true)
                return .init(state: .conflict, summary: sentence, detail: result.message)
            }
            if result.isQueued {
                let expires = ISO8601DateFormatter.lenient.date(from: result.expiresAt ?? result.op?.expiresAt)
                let when = expires.map { ", expiring \($0.relativeDescription())" } ?? ""
                let sentence = "\(verb) is queued and runs the next time this Mac is idle\(when)."
                setNote(sentence, isError: false)
                return .init(state: .queued, summary: sentence, detail: result.message, expiresAt: expires)
            }
            return interpret(result, conclusion)
        } catch let invalid as AgentRequest.Invalid {
            setNote(invalid.message, isError: true)
            return .init(state: .failed, summary: invalid.message)
        } catch let failure as AgentFailure {
            return reportFailure(failure, verb: verb)
        } catch {
            return reportFailure(
                AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                verb: verb
            )
        }
    }

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
            ?? "something is running on this Mac."
        setNote("\(verb) was held back: \(sentence) Nothing was interrupted.", isError: false)

        // The override is offered, never taken. Force skips the agent's own busy check, and that is a
        // decision only a person gets to make — but this Mac used to have no way to make it at all,
        // so a stuck probe meant the local machine could not be updated by any means.
        let overridable = reason == .busy || reason == .busyUnknown || reason == nil
        if !force, overridable {
            ask?(PendingDialog(
                id: "force-local-\(subject)",
                title: "\(verb) \(subject) anyway?",
                message: "\(sentence)\n\nGoing ahead interrupts that work and what is in progress is lost.",
                confirmTitle: "Do it anyway",
                perform: forceAgain
            ))
        }
        return .init(state: .deferred, summary: "\(verb) was held back: \(sentence)",
                     detail: result.message, awaitingDecision: !force && overridable)
    }

    private func otherOutcome(_ result: AgentActionResult, subject: String, verb: String) -> ActionResolution {
        let reason = result.reason
        let sentence = reason?.sentence(subject: subject, message: result.message)
            ?? result.message
            ?? "\(verb) did not go through."
        let state = OperationOutcome.state(for: result, stillRunning: false)
        setNote(sentence, isError: state.isProblem)
        return .init(state: state, summary: sentence, detail: result.message)
    }

    private func reportFailure(_ failure: AgentFailure, verb: String) -> ActionResolution {
        switch failure.dispatch {
        case .never:
            let sentence = "\(verb) did not happen: \(failure.localMessage)"
            setNote(sentence, isError: true)
            return .init(state: .failed, summary: sentence, detail: failure.detailText)
        case .unknown, .acknowledged:
            let sentence = "\(verb) may or may not have happened: \(failure.localMessage)"
            setNote(sentence, isError: true)
            return .init(state: .unknown, summary: sentence, detail: failure.detailText, unresolved: true)
        }
    }

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
            message: body,
            confirmTitle: confirmTitle,
            perform: now,
            alternativeTitle: dialect.supportsQueue ? "When idle" : nil,
            alternative: dialect.supportsQueue ? whenIdle : nil
        ))
    }

    private func perform(
        _ label: String,
        kind: OperationKind,
        subject: String,
        targetVersion: String? = nil,
        _ work: @escaping @MainActor (_ operationId: String) async -> ActionResolution
    ) {
        guard activity == nil else { return }
        activity = label
        setNote(label + ".", isError: false)

        let record = OperationRecord(
            id: AgentToken.newOperationId(),
            machineId: historyMachineId,
            machineName: config.name,
            kind: kind,
            subject: subject,
            targetVersion: targetVersion,
            summary: label + ".",
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
                    forceUnresolved: resolution.unresolved
                )
            }
            if !resolution.awaitingDecision { await self.refresh(duringAction: true) }
            self.activity = nil
            if let finished = self.operations?.record(id: record.id) {
                self.onOperationFinished?(finished)
            }
            self.onStateChange?()
        }
    }

    private func setNote(_ text: String, isError: Bool) {
        note = text
        noteIsError = isError
        noteStamp = Date()
    }
}
