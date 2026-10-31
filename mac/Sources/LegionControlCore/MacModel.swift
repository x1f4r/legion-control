import Foundation
import Observation

/// This Mac as a machine of its own.
///
/// The same agent and the same JSON as anywhere else, but it runs here rather than over ssh. There
/// is nothing to wake and nothing to boot, because the device the app runs on is by definition up.
/// Everything else a remote machine offers is offered here too: restart, configured actions, the
/// update policy, queue-until-idle, doctor and logs. It used to have a single Update button and
/// nothing else, which made the one machine you are sitting in front of the one you could do least
/// to. All of that is now the same code a machine over ssh runs (AgentDevice.swift); this file is
/// how the local agent is spawned and read, and power for the machine this device is bound to.
@MainActor
@Observable
final class MacModel: AgentDevice {
    private(set) var config: LocalConfig
    private(set) var status: AgentStatus?
    private(set) var failure: AgentFailure?
    private(set) var lastChecked: Date?
    private(set) var isRefreshing = false

    let core = DeviceCore()

    private var agent: MacAgent
    private var lastRefreshFinished: Date?

    /// The id this machine is filed under in the operation history. The local Mac has no entry in
    /// the machines list, so it needs one of its own.
    static let machineId = "local"

    /// The machine in the shared setup this device is, when the private bindings say so. Nil when
    /// this is only "the agent that happens to run here".
    var boundMachineId: String?
    var boundAgentArgv: [String]?
    var migrateAgentCommand: (@MainActor ([String]) -> Void)?
    /// The corresponding shared machine, including its boot targets. Private bindings decide that
    /// this is self; the shared setup still describes what the hardware can do.
    var boundMachine: Machine?

    init(config: LocalConfig) {
        self.config = config
        self.agent = MacAgent(scriptPath: config.agentPath)
    }

    /// For the tests.
    init(config: LocalConfig, agent: MacAgent) {
        self.config = config
        self.agent = agent
    }

    /// Follow a change to how this device runs its own agent without starting over.
    ///
    /// The model used to be replaced whenever the local block or the bindings changed, which threw
    /// away anything in flight along with it. The same device is still the same device: only its
    /// name, its agent command or the machine it is bound to change.
    ///
    /// A new command is a different agent, though, and what the old one reported is not the new
    /// one's to show. The section reads "checking" until the new agent answers, as a new model
    /// did, rather than showing the old agent's services until the next read, and a document the
    /// old one refused is pushed to the new one on its first read.
    func update(config: LocalConfig, agent: MacAgent, boundMachineId: String?, boundAgentArgv: [String]?) {
        if (boundAgentArgv ?? [config.agentPath]) != agentCommand {
            status = nil
            failure = nil
            lastChecked = nil
            lastRefreshFinished = nil
            core.agentDidChange()
        }
        self.config = config
        self.agent = agent
        self.boundMachineId = boundMachineId
        self.boundAgentArgv = boundAgentArgv
        onStateChange?()
    }

    /// What this device runs to reach its agent: the argv the bindings give, or else the script the
    /// local block names.
    private var agentCommand: [String] { boundAgentArgv ?? [config.agentPath] }

    var name: String { config.name }

    var historyMachineId: String { boundMachineId ?? Self.machineId }

    /// The sentence the local section shows under its buttons after an action.
    var note: String? { core.line }
    var noteIsError: Bool { core.lineIsError }
    var noteDetail: String? { core.lineDetail }
    var noteStamp: Date { core.lineStamp }

    var isReachable: Bool { status != nil }

    /// What this Mac's last read of its own agent says about it as a wake helper (section 10.2).
    var wakeHelperState: WakePlan.HelperState {
        if status != nil { return .init(.ready, actionKinds: status?.actions.map(WakePlan.HelperState.kinds)) }
        return failure.map(WakePlan.HelperState.init(after:)) ?? .unknown
    }

    var isBusy: Bool { status?.busy?.isBusy ?? false }

    var autoUpdate: Bool? { status?.updates?.automatic ?? status?.autoUpdate }

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

    /// Why the bundled agent is not installed from here, in words, or nil when nothing stands in the
    /// way but something already running.
    var agentInstallRefusal: String? {
        guard status?.isRestrictedSession == true else { return nil }
        return "The agent is installed from here only from an administrator session, and this one is restricted."
    }

    func actionUnavailableReason() -> String? {
        guard isReachable else { return "This Mac could not be read, so its actions cannot be run." }
        return nil
    }

    /// The state of this Mac's row in the sidebar, which VoiceOver reads as the row's value
    /// (SidebarRow.summary).
    var sidebarSummary: String {
        if failure != nil { return "not readable" }
        guard isReachable else { return "checking" }
        if !unresolvedOperations.isEmpty { return "outcome not known yet" }
        // An agent that answered with nothing to look after has said all there is to say. Reading
        // "checking" here kept VoiceOver waiting for an answer that had already come.
        if services.isEmpty { return "no services configured" }
        if services.contains(where: { $0.hasStagedUpdate }) { return "update waiting" }
        if services.contains(where: { $0.upToDate == false }) { return "update available" }
        if let reason = busyReason { return reason }
        if let newer = newerBundledAgent { return "agent \(newer) available" }
        if services.allSatisfy({ $0.upToDate == true }) { return "up to date" }
        return "version not checked"
    }

    // MARK: - Reaching the agent

    var isCommandable: Bool { isReachable }

    var recordedSystem: (id: String?, name: String?) { (nil, nil) }

    var recordedRoute: String? { nil }

    /// "Adopted the setup from this Mac" would name the device the app is running on as though it
    /// were somewhere else. What is really being read is the local agent's copy.
    var setupHolder: String { "the local agent" }

    /// Reading the local agent costs one process launch and no network, so this Mac reads itself
    /// again after every action, as it always has, and not only after the ones that ask. An action
    /// that never reached the agent, because the agent has gone missing say, then corrects the
    /// section at once rather than at the next poll, and no poll comes while nothing is on screen.
    var readsAfterEveryAction: Bool { true }

    func send<Value: Decodable & Sendable>(_ request: AgentRequest, decoding: Value.Type) async throws -> Value {
        try await agent.send(request, decoding: Value.self)
    }

    func refreshStatus(duringAction: Bool) async {
        await refresh(duringAction: duringAction)
    }

    /// The local agent is a process on this Mac, with no address to hold back.
    func allowRedial() {}

    func describe(_ failure: AgentFailure) -> String {
        failure.localMessage
    }

    // MARK: - Reading

    func refreshIfNeeded(minimumAge: TimeInterval = 2) async {
        if let lastRefreshFinished, Date().timeIntervalSince(lastRefreshFinished) < minimumAge { return }
        await refresh()
    }

    func refresh(duringAction: Bool = false) async {
        guard !isRefreshing, duringAction || activity == nil else { return }
        isRefreshing = true
        let generation = core.agentGeneration
        defer {
            isRefreshing = false
            // A read of an agent this device no longer runs does not count as a read, so nothing
            // waits two seconds before asking the new one.
            if core.agentGeneration == generation { lastRefreshFinished = Date() }
            onStateChange?()
        }

        do {
            let reply = try await agent.status(dialect: dialect)
            // The agent may have been replaced while this read was out. What the old one said is not
            // the new one's state, so it is dropped, here and below, and the next read asks the new
            // one.
            guard core.agentGeneration == generation else { return }
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
            guard core.agentGeneration == generation else { return }
            status = nil
            failure = error
            lastChecked = Date()
        } catch {
            guard core.agentGeneration == generation else { return }
            status = nil
            failure = AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown)
            lastChecked = Date()
        }
    }

    // MARK: - Power for a bound self machine

    func requestSleep() {
        guard boundMachine != nil, isReachable else { return }
        let reason = busyReason
        offerDisruptive(
            id: "sleep-self-\(historyMachineId)",
            title: "Put this device to sleep?",
            body: (reason.map { _ in "Activity is busy or unconfirmed. Sleeping now can interrupt work.\n\n" } ?? "")
                + "This controller reconnects after the device returns.",
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
                        // The app goes to sleep with the device, so the record cannot be read until it
                        // is back. It stays owed rather than being called a success in advance.
                        let sentence = "Sleep was acknowledged. Its operation record will be read after this device comes back."
                        self.setStatus(sentence, detail: result.message)
                        return .init(state: .unknown, summary: sentence, detail: result.message, refresh: true, unresolved: true)
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
            body: (reason.map { _ in "Activity is busy or unconfirmed. Rebooting now can lose work.\n\n" } ?? "")
                + "This controller reconnects after the device returns.",
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
                        self.setStatus(sentence, detail: result.message)
                        return .init(state: .unknown, summary: sentence, detail: result.message, refresh: true, unresolved: true)
                    case "armed":
                        let sentence = "The next boot is set to \(target.name). Nothing has rebooted yet."
                        self.setStatus(sentence, detail: result.message)
                        return .init(state: .succeeded, summary: sentence, detail: result.message, refresh: true)
                    case "noop":
                        let sentence = "\(target.name) is already running."
                        self.setStatus(sentence, detail: result.message)
                        return .init(state: .noop, summary: sentence, detail: result.message, refresh: true)
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
        let serviceName = service.displayName
        let staged = service.stagedVersion
        offerDisruptive(
            id: "update-\(historyMachineId)-\(service.id)",
            title: "Update \(serviceName) on \(name)?",
            body: (staged.map { "Build \($0) is downloaded and waiting. " } ?? "")
                + "\(serviceName) quits and restarts to apply the update. Unsaved work may be lost. "
                + "The agent holds the update if activity is busy or unconfirmed.",
            confirmTitle: "Quit and update",
            now: { [weak self] in self?.update(service, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.update(service, force: false, whenIdle: true) }
        )
    }

    // MARK: - Policy

    func setAutoUpdate(_ enabled: Bool, service: ServiceStatus? = nil) {
        saveAutoUpdate(enabled, service: service, subject: service?.displayName ?? name)
    }

    // MARK: - Diagnostics

    func serviceConfig(_ request: AgentRequest) async throws -> ServiceConfigReply {
        try await send(request, decoding: ServiceConfigReply.self)
    }

    // MARK: - Trusted agent deployment

    /// A 2.x agent here cannot take the payload on stdin either, so the bootstrap is run locally with
    /// the command this device starts its agent with. Same verification, no ssh.
    func installOverLegacyAgent(_ payload: Data, operation: String, version: String) async -> ActionResolution {
        let argv: [String]
        do {
            argv = try await agent.bootstrapCommand()
        } catch {
            setStatus(error.localizedDescription, isError: true)
            return .init(state: .failed, summary: error.localizedDescription)
        }
        guard let layout = AgentBootstrap.chooseLayout(argv: argv) else {
            return .init(state: .cancelled, summary: "The legacy agent upgrade was cancelled.")
        }
        operations?.markAgentTracked(operation)
        do {
            let result = try await AgentBootstrap.installLocal(payload: payload, operation: operation, layout: layout)
            guard result.didInstallAgent else {
                return otherOutcome(result, subject: "control agent", verb: "Installing the agent")
            }
            if boundMachineId != nil {
                ask?(PendingDialog(id: "migrate-local-agent", title: "Use the stable agent launcher?",
                                   message: "The upgrade completed. Update this device's command?\n\nCurrent: \(CommandArguments.text(argv))\nProposed: \(CommandArguments.text(layout.stableArgv))",
                                   confirmTitle: "Update command", perform: { self.migrateAgentCommand?(layout.stableArgv) }))
            }
            return .init(state: .succeeded, summary: "Agent \(version) was installed here.", refresh: true)
        } catch let failure as AgentBootstrap.Failure {
            return .init(state: failure.uncertain ? .unknown : .failed, summary: failure.message, unresolved: failure.uncertain)
        } catch {
            setStatus(error.localizedDescription, isError: true)
            return .init(state: .failed, summary: error.localizedDescription)
        }
    }
}
