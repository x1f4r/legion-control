import Foundation
import Observation

/// One device the app drives through its agent: a machine reached over ssh, or the Mac the app is
/// running on.
///
/// All of what follows used to be written twice. MachineModel and MacModel each carried their own
/// copy of how an action is started, sent, followed and written down, how a lost outcome is chased,
/// how the setup is shared, and every verb in between: about six hundred lines apiece, and the
/// copies had drifted. A failure on this Mac never reached the panel's error notice, a setup this
/// Mac could not read failed without a word, and the local maintenance cycle threw away what
/// happened to each service. Each of those rules is now written once, in this file and in
/// AgentDeviceSetup.swift, and a model adds only what is really its own: how its agent is reached,
/// how its status is read, and what power means for the machine it is.
@MainActor
protocol AgentDevice: SetupChannel {
    /// What every device keeps the same way.
    var core: DeviceCore { get }
    /// What sentences and the operation history call this device.
    var name: String { get }
    /// The id this device's operations are filed under.
    var historyMachineId: String { get }
    /// What the agent last said, or nil while it is not answering.
    var status: AgentStatus? { get }
    /// Whether there is anything to send a command to right now.
    var isCommandable: Bool { get }
    /// What an operation record says about where it ran, when the device has more than one system.
    var recordedSystem: (id: String?, name: String?) { get }
    /// The route the last call went over, for the operation record.
    var recordedRoute: String? { get }
    /// Whether a fresh status read follows every action, rather than only the ones whose resolution
    /// asks for one.
    var readsAfterEveryAction: Bool { get }

    /// Put one request in front of this device's agent and decode what comes back.
    ///
    /// The one place the two transports differ. Everything that decides what a reply means sits
    /// above this, so the local Mac and a machine across the world can never disagree about it.
    func send<Value: Decodable & Sendable>(_ request: AgentRequest, decoding: Value.Type) async throws -> Value
    /// Read the status again, as the poll would.
    func refreshStatus(duringAction: Bool) async
    /// A transport failure in this device's own words: a machine names itself and talks about ssh,
    /// this Mac has no connection to blame.
    func describe(_ failure: AgentFailure) -> String
    /// Install the bundled agent over a 2.x one, which cannot take the payload on stdin. Each
    /// transport bootstraps it in its own way.
    func installOverLegacyAgent(_ payload: Data, operation: String, version: String) async -> ActionResolution
}

/// The state every device keeps the same way, whichever way its agent is reached.
///
/// Each model holds one rather than inheriting it. Everything here is written by the shared code in
/// this file and in AgentDeviceSetup.swift, and read through the model.
@MainActor
@Observable
final class DeviceCore {
    /// Non-nil while a user-initiated action runs. Doubles as the "controls are disabled" flag.
    fileprivate(set) var activity: String?
    /// What the last action the user started ended in, or nil while none has run since launch.
    fileprivate(set) var lastActionOutcome: ActionOutcome?
    /// The operation this app is currently watching over there, so the section can show its phase.
    fileprivate(set) var watchedOperation: String?
    /// The last doctor run, when one has been asked for.
    fileprivate(set) var doctor: AgentDoctorResult?
    fileprivate(set) var doctorRanAt: Date?

    /// The last sentence this device said about itself, for its own section and the window's status
    /// bar. Nil until it has said anything.
    fileprivate(set) var line: String?
    fileprivate(set) var lineIsError = false
    fileprivate(set) var lineDetail: String?
    /// When the line last changed. The window has one status bar for the whole app, so with more
    /// than one device it shows whichever of them spoke last.
    fileprivate(set) var lineStamp = Date.distantPast
    /// True while the line is a failed status read rather than anything an action said, so the next
    /// good read can put the last action's own outcome back.
    fileprivate(set) var lineIsRefreshFailure = false

    /// Sharing the setup with this device's agent (SetupPeerSync.swift).
    let setup = SetupPeerSync()

    /// Counts the agents this device has been pointed at. A reply that set out before the count
    /// moved came from an agent the device no longer runs, and is not taken as the new one's.
    @ObservationIgnored private(set) var agentGeneration = 0

    /// The last time the app asked the far side about an operation it had lost track of.
    @ObservationIgnored fileprivate var lastReconcileAttempt: Date = .distantPast

    /// How an action waits before polling its operation again after a poll that did not get
    /// through. The tests hand in one that returns at once.
    @ObservationIgnored var pollPause: @MainActor (Duration) async -> Void = { try? await Task.sleep(for: $0) }

    /// How often an operation with an unknown outcome is chased.
    static let reconcileEvery: TimeInterval = 60

    // MARK: - Wiring

    /// Called whenever something a viewer draws has changed.
    var onStateChange: (@MainActor () -> Void)?
    /// How a question reaches the screen. The app owns the one place a question is shown, because
    /// only one of them can be answered at a time whichever device raised it.
    var ask: (@MainActor (PendingDialog) -> Void)?
    /// Where operations are written down. A device without one still works and keeps no history.
    var operations: OperationStore?
    /// Told when something the user explicitly asked for has finished, so it can be announced.
    var onOperationFinished: (@MainActor (OperationRecord) -> Void)?
    /// The setup this device is running on. Every agent gets a copy of it, the local one included:
    /// the file is the source of truth, and no device is an exception to that.
    var controllerConfig: (@MainActor () -> ControllerDocument?)?
    /// Adopt a document read off this device's agent, in place of the applied document with the
    /// hash given, nil for none: a compare-and-swap (ConfigStore.adopt).
    var adoptSetup: (@MainActor (Data, String, String?) -> ConfigStore.AdoptOutcome)?
    /// Take a document a person asked to take from this device's agent, in place of the applied
    /// document with the hash given. With no setup applied here that is enrolment: every launch
    /// value in it is shown, and nothing is written until the person confirms. With one applied it
    /// is adopting a different setup, which approves nothing.
    var takeSetup: (@MainActor (Data, ControllerIdentity, String, String?) -> SetupTaking)?
    /// The bytes of the base a merge of the two documents needs (section 9.8), when this device
    /// kept them: mine's hash and lineage, then theirs'. Without it a merge can still be done, but
    /// every difference needs a decision instead of only the ones both sides touched.
    var setupBase: (@MainActor (String, [String], String, [String]) -> Data?)?
    /// Raise the divergence sheet.
    var onSetupDiverged: (@MainActor (SetupDivergence) -> Void)?

    /// The line is a failed status read. Set by the model that read it, after saying so.
    func markLineAsRefreshFailure() {
        lineIsRefreshFailure = true
    }

    /// The device now runs a different agent, so what the old one said about itself goes: its
    /// doctor run, and all that is known about its copy of the setup, a refused push included,
    /// which would otherwise be held against the new agent for a minute. What the user has in
    /// flight stays, because it is theirs rather than the agent's: the action running, how the last
    /// one ended, the operation being watched, and a setup read or push that is still out.
    func agentDidChange() {
        agentGeneration += 1
        doctor = nil
        doctorRanAt = nil
        setup.agentDidChange()
    }
}

/// What the last action the user started ended in.
struct ActionOutcome: Equatable, Sendable {
    var text: String
    var isError: Bool
    /// The operation it came from, so the panel can offer to look at it.
    var operationId: String?
}

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

/// What running one wake action on a device came to.
struct WakeActionOutcome: Sendable {
    var sent: Bool
    var summary: String
    var failure: AgentFailure?
    var isDeclaredWakePacket: Bool
    /// True when the command may have run and nothing has said how it ended: a wake hold for the
    /// target now stands, and no other helper runs a command for it.
    var isAmbiguous: Bool
}

/// Whether installing an agent takes a device back rather than forward (section 13).
enum AgentDowngrade {
    /// What makes installing `version` over the agent that sent `status` a way back, or nil when it
    /// is not one: older than the agent running, or below its version floor.
    static func reason(installing version: String, over status: AgentStatus) -> String? {
        if let running = status.version, AgentVersion.isOlder(version, than: running) {
            return "Agent \(version) is older than the \(running) running there"
        }
        if let floor = status.agent?.floor, AgentVersion.isOlder(version, than: floor) {
            return "Agent \(version) is below the version floor \(floor) kept there"
        }
        return nil
    }

    /// Whether going back may be asked for at all: only of an agent that reports a floor, which
    /// every agent that knows `--allow-downgrade` does, and never over a restricted key, whose
    /// dispatcher refuses the flag. An agent from before the floor is never taken back from here.
    static func isOffered(by status: AgentStatus) -> Bool {
        status.agent?.reportsFloor == true && !status.isRestrictedSession
    }
}

// MARK: - What every device reads the same way

extension AgentDevice {
    var activity: String? { core.activity }

    var isWorking: Bool { core.activity != nil }

    var lastActionOutcome: ActionOutcome? { core.lastActionOutcome }

    var watchedOperation: String? { core.watchedOperation }

    var doctor: AgentDoctorResult? { core.doctor }

    var doctorRanAt: Date? { core.doctorRanAt }

    var setupHolder: String { name }

    var readsAfterEveryAction: Bool { false }

    /// Which contract the far side speaks, and therefore what may be sent to it.
    var dialect: AgentDialect { AgentDialect(status) }

    /// True when the agent is too old for the safety properties this app depends on, and the
    /// section should offer to install the bundled one.
    var needsAgentUpgrade: Bool {
        guard let status, status.ok != false else { return false }
        return !status.speaksRequiredContract
    }

    var services: [ServiceStatus] { status?.resolvedServices ?? [] }

    func service(id: String) -> ServiceStatus? { status?.service(id: id) }

    /// The named actions the agent offers, the same on this Mac as on any machine.
    var actions: [AgentActionInfo] { (status?.actions ?? []).filter { !$0.id.isEmpty } }

    func actions(for service: ServiceStatus) -> [AgentActionInfo] {
        (service.actions ?? []).filter { !$0.id.isEmpty }
    }

    /// Everything the agent could not read, in its own words.
    var notes: [String] { status?.allNotes ?? [] }

    /// The system-wide update policy the far side reports.
    var policy: AgentUpdatePolicy? { status?.updates }

    /// Requests the agent is holding until the device is idle.
    var queuedOperations: [AgentOperation] { status?.queuedOperations ?? [] }

    /// What is running over there right now.
    var runningOperations: [AgentOperation] { status?.runningOperations ?? [] }

    var recentOperations: [AgentOperation] { status?.recentOperations ?? [] }

    var busyReason: String? {
        guard let busy = status?.busy, busy.isBusy else { return nil }
        return busy.summary
    }

    /// Operations this app started here and lost the answer to.
    var unresolvedOperations: [OperationRecord] {
        (operations?.unresolved ?? []).filter { $0.machineId == historyMachineId }
    }

    /// The record for the operation the app is watching, so the section can draw its phase.
    var watchedRecord: OperationRecord? {
        guard let watchedOperation else { return nil }
        return operations?.record(id: watchedOperation)
    }

    /// The service to name on the command line, or nil for an agent that has never heard of the
    /// flag: its one service is the default anyway.
    func serviceArgument(_ service: ServiceStatus) -> String? {
        status?.reportsServices == true ? service.id : nil
    }

    // MARK: - Wiring

    var onStateChange: (@MainActor () -> Void)? {
        get { core.onStateChange }
        set { core.onStateChange = newValue }
    }

    var ask: (@MainActor (PendingDialog) -> Void)? {
        get { core.ask }
        set { core.ask = newValue }
    }

    var operations: OperationStore? {
        get { core.operations }
        set { core.operations = newValue }
    }

    var onOperationFinished: (@MainActor (OperationRecord) -> Void)? {
        get { core.onOperationFinished }
        set { core.onOperationFinished = newValue }
    }

    var controllerConfig: (@MainActor () -> ControllerDocument?)? {
        get { core.controllerConfig }
        set { core.controllerConfig = newValue }
    }

    var adoptSetup: (@MainActor (Data, String, String?) -> ConfigStore.AdoptOutcome)? {
        get { core.adoptSetup }
        set { core.adoptSetup = newValue }
    }

    var takeSetup: (@MainActor (Data, ControllerIdentity, String, String?) -> SetupTaking)? {
        get { core.takeSetup }
        set { core.takeSetup = newValue }
    }

    var setupBase: (@MainActor (String, [String], String, [String]) -> Data?)? {
        get { core.setupBase }
        set { core.setupBase = newValue }
    }

    var onSetupDiverged: (@MainActor (SetupDivergence) -> Void)? {
        get { core.onSetupDiverged }
        set { core.onSetupDiverged = newValue }
    }

    // MARK: - Saying what happened

    /// The one way a device says something about itself.
    func setStatus(_ line: String, isError: Bool = false, detail: String? = nil) {
        core.lineIsRefreshFailure = false
        core.line = line
        core.lineIsError = isError
        core.lineDetail = detail
        core.lineStamp = Date()
    }

    /// Re-reads the status after an action without disturbing the sentence the action just wrote.
    func refreshQuietly() async {
        let line = core.line
        let isError = core.lineIsError
        let detail = core.lineDetail
        await refreshStatus(duringAction: true)
        if let line { setStatus(line, isError: isError, detail: detail) }
    }

    // MARK: - Chasing what we lost track of

    /// Ask the far side what became of an operation whose outcome this app never learned.
    ///
    /// This is the whole reason mutations carry an id. Before, an update that timed out left nothing
    /// behind but a red sentence, and the only way to find out whether it had actually installed was
    /// to read the version afterwards and guess.
    func reconcileIfNeeded() {
        guard let operations, dialect.supportsOperations else { return }
        guard Date().timeIntervalSince(core.lastReconcileAttempt) > DeviceCore.reconcileEvery else { return }
        let owed = operations.unresolved.filter { $0.machineId == historyMachineId && $0.agentTracked }
        guard let next = owed.first else { return }
        core.lastReconcileAttempt = Date()

        // The status the poll just read may already carry it, which costs nothing at all.
        if let known = status?.operation(id: next.id) {
            operations.reconcile(next.id, with: known)
            onStateChange?()
            return
        }

        Task { @MainActor in
            do {
                let request = try AgentRequest.operation(id: next.id, wait: nil)
                let reply = try await self.send(request, decoding: AgentOperationResult.self)
                if let operation = reply.operation {
                    operations.reconcile(next.id, with: operation)
                } else if reply.isUnknownOperation {
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
        core.lastReconcileAttempt = .distantPast
        reconcileIfNeeded()
    }

    func dismissUnknownOutcome(_ id: String) {
        operations?.dismissReconciliation(id)
        onStateChange?()
    }

    // MARK: - Services

    func requestRestart(_ service: ServiceStatus) {
        let serviceName = service.displayName
        offerDisruptive(
            id: "restart-\(historyMachineId)-\(service.id)",
            title: "Restart \(serviceName)?",
            body: busyReason != nil
                ? "\(serviceName) stops and starts again. The agent checks activity first and holds the restart if work is busy or unconfirmed."
                : "\(serviceName) stops and starts again. Anything in flight is dropped.",
            confirmTitle: "Restart",
            // Never forced from here, however busy the last reading looked. Force is the agent's own
            // check being skipped, and it is only offered back after the agent has looked at the
            // device as it is now and said no.
            now: { [weak self] in self?.restart(service, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.restart(service, force: false, whenIdle: true) }
        )
    }

    func restart(_ service: ServiceStatus, force: Bool, whenIdle: Bool) {
        let serviceName = service.displayName
        let dialect = dialect
        let serviceId = serviceArgument(service)
        perform("Restarting \(serviceName)", kind: .restart, subject: serviceName) { operation in
            await self.dispatch(
                operation: operation,
                subject: serviceName,
                verb: "The restart of \(serviceName)",
                build: { intent in try AgentRequest.restart(service: serviceId, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in
                    switch result.action {
                    case "restarted":
                        self.setStatus("\(serviceName) restarted.", detail: result.message)
                        return .init(state: .succeeded, summary: "\(serviceName) restarted on \(self.name).",
                                     detail: result.message, refresh: true)
                    case "deferred":
                        return self.deferred(result, subject: serviceName, verb: "The restart", force: force) {
                            self.restart(service, force: true, whenIdle: false)
                        }
                    default:
                        return self.otherOutcome(result, subject: serviceName, verb: "The restart of \(serviceName)")
                    }
                }
            )
        }
    }

    func requestUpdateWhenIdle(_ service: ServiceStatus) {
        update(service, force: false, whenIdle: true)
    }

    func update(_ service: ServiceStatus, force: Bool, whenIdle: Bool) {
        let serviceName = service.displayName
        let dialect = dialect
        let serviceId = serviceArgument(service)
        perform("Updating \(serviceName)", kind: .update, subject: serviceName, targetVersion: service.latest) { operation in
            await self.dispatch(
                operation: operation,
                subject: serviceName,
                verb: "The update of \(serviceName)",
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
        let serviceName = service.displayName
        let from = result.from ?? "the installed version"
        let to = result.to ?? "the latest build"
        switch result.action {
        case "updated":
            setStatus("\(serviceName) updated from \(from) to \(to).", detail: result.message)
            return .init(state: .succeeded, summary: "\(serviceName) updated from \(from) to \(to).",
                         detail: result.message, refresh: true)
        case "noop":
            // Every "nothing happened" used to read as "already on the latest build", which is one of
            // at least five things it can mean and the least consequential of them. The agent's own
            // reason code decides the sentence now, and where there is none the app says so rather
            // than inventing one.
            let reason = result.reason
            let sentence = reason?.sentence(subject: serviceName, message: result.message)
                ?? result.message
                ?? "Nothing was installed, and the agent did not say why."
            setStatus(sentence, isError: !(reason?.isBenign ?? false), detail: result.message)
            return .init(state: .noop, summary: sentence, detail: result.message, refresh: true)
        case "deferred":
            return deferred(result, subject: serviceName, verb: "The update", force: force) { [weak self] in
                self?.update(service, force: true, whenIdle: false)
            }
        case "downloaded", "staged":
            // An app that is open cannot be replaced under the person using it. The agent fetches the
            // build and leaves it for the app to pick up when it next quits.
            let sentence = result.message ?? "The build was downloaded and is waiting for \(serviceName) to quit."
            setStatus(sentence)
            return .init(state: .succeeded, summary: sentence, refresh: true)
        case "rolled-back":
            setStatus("The update failed and the previous version was put back.", isError: true, detail: result.message)
            return .init(state: .failed, summary: "The update of \(serviceName) failed and the previous version was put back.",
                         detail: result.message, refresh: true)
        default:
            return otherOutcome(result, subject: serviceName, verb: "The update of \(serviceName)")
        }
    }

    /// One cycle across every service the device looks after, rather than only the first.
    func requestCycle() {
        ask?(PendingDialog(
            id: "cycle-\(historyMachineId)",
            title: "Run the maintenance cycle on \(name)?",
            message: "Update eligible services. Busy services and those outside their schedule are skipped.",
            confirmTitle: "Run the cycle",
            perform: { [weak self] in self?.runCycle() }
        ))
    }

    func runCycle() {
        let dialect = dialect
        perform("Running the maintenance cycle on \(name)", kind: .updateAll, subject: name) { operation in
            await self.dispatch(
                operation: operation,
                subject: self.name,
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
                    let sentence = "Maintenance cycle on \(self.name): \(parts.joined(separator: ", "))."
                    // The counts say how it went; the lines say which service it went that way for,
                    // and why, which is the part anyone acting on a failure needs.
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

    // MARK: - Configured actions

    func requestAction(_ action: AgentActionInfo) {
        guard let confirm = action.confirm, !confirm.isEmpty else {
            run(action, force: false, whenIdle: false)
            return
        }
        offerDisruptive(
            id: "run-\(historyMachineId)-\(action.id)",
            title: "\(action.displayName)?",
            body: confirm,
            confirmTitle: action.displayName,
            now: { [weak self] in self?.run(action, force: false, whenIdle: false) },
            whenIdle: { [weak self] in self?.run(action, force: false, whenIdle: true) }
        )
    }

    func run(_ action: AgentActionInfo, force: Bool, whenIdle: Bool) {
        let actionName = action.displayName
        let dialect = dialect
        perform(actionName, kind: .action, subject: actionName) { operation in
            await self.dispatch(
                operation: operation,
                subject: actionName,
                verb: actionName,
                build: { intent in try AgentRequest.run(action: action.id, intent: intent, dialect: dialect) },
                intent: .init(force: force, whenIdle: whenIdle, detach: dialect.supportsDetach, operationId: operation),
                interpret: { result, _ in
                    let output = result.output?.trimmingCharacters(in: .whitespacesAndNewlines)
                    switch result.action {
                    case "ran":
                        // The output has always come back and neither app ever showed it, which made
                        // "the action ran" the whole of what anyone learned from a button whose
                        // entire purpose is to run a command and read what it said.
                        self.setStatus("\(actionName) ran.", detail: output?.isEmpty == false ? output : result.message)
                        return .init(state: .succeeded, summary: "\(actionName) ran.",
                                     detail: result.message, output: output, refresh: true)
                    case "deferred":
                        return self.deferred(result, subject: actionName, verb: actionName, force: force) {
                            self.run(action, force: true, whenIdle: false)
                        }
                    default:
                        var resolution = self.otherOutcome(result, subject: actionName, verb: actionName)
                        resolution.output = output
                        return resolution
                    }
                }
            )
        }
    }

    /// Run a configured action here on another machine's behalf and retain its operation identity.
    func runWakeAction(_ actionId: String, target: String, targetId: String) async -> WakeActionOutcome {
        if !isCommandable { await refreshStatus(duringAction: false) }
        let declared = actions.first { $0.id == actionId }?.isDeclaredWakePacket ?? false
        guard isCommandable else {
            return .init(sent: false, summary: "not reachable", failure: nil, isDeclaredWakePacket: declared, isAmbiguous: false)
        }
        return await WakeAction.run(action: actionId, target: target, targetId: targetId, machineId: historyMachineId,
                                    machineName: name, declaredPacket: declared, dialect: dialect, operations: operations,
                                    send: { try await self.send($0, decoding: AgentActionResult.self) },
                                    read: { try await self.send($0, decoding: AgentOperationResult.self) })
    }

    // MARK: - Policy

    /// Turn the schedule on or off for the whole device or for one service. `saved` hears the value
    /// the agent says it now has.
    func saveAutoUpdate(_ enabled: Bool, service: ServiceStatus?, subject: String,
                        saved: @escaping @MainActor (Bool) -> Void = { _ in }) {
        let dialect = dialect
        let serviceId = service.flatMap { serviceArgument($0) }
        perform("Saving the update setting", kind: .autoUpdate, subject: subject) { _ in
            do {
                let request = try AgentRequest.autoUpdate(enabled, service: serviceId, dialect: dialect)
                let result = try await self.send(request, decoding: AgentActionResult.self)
                // A refusal is a reply too. Read as a saved setting, it put the switch where the
                // person moved it while the agent kept the old value.
                guard Outcome.of(result) == .succeeded else {
                    return self.otherOutcome(result, subject: subject, verb: "The update setting")
                }
                let value = result.updates?.automatic ?? result.autoUpdate ?? enabled
                saved(value)
                let sentence = value
                    ? "\(subject) updates on the schedule."
                    : "\(subject) is off the schedule. You can still update it whenever you ask."
                self.setStatus(sentence, detail: result.message)
                return .init(state: .succeeded, summary: sentence, detail: result.message, refresh: true)
            } catch let failure as AgentFailure {
                return self.reportFailure(failure, verb: "The update setting")
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "The update setting"
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
        let subject = service?.displayName ?? name
        let serviceId = service.flatMap { serviceArgument($0) }
        perform(description, kind: .policy, subject: subject) { _ in
            do {
                let request = try AgentRequest.policySet(service: serviceId, patch: patch)
                let result = try await self.send(request, decoding: AgentPolicyResult.self)
                guard result.ok != false else {
                    let sentence = result.message ?? "\(subject): the policy was not changed."
                    self.setStatus(sentence, isError: true)
                    return .init(state: .failed, summary: sentence)
                }
                let sentence = result.updates.map { "\(subject) is now \($0.scheduleSummary)." }
                    ?? "\(subject): the policy was saved."
                self.setStatus(sentence)
                return .init(state: .succeeded, summary: sentence, refresh: true)
            } catch let failure as AgentFailure {
                return self.reportFailure(failure, verb: "The policy change")
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
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
                let result = try await self.send(request, decoding: AgentActionResult.self)
                // Decided on the outcome, not on `ok`. A request that started while the cancel was
                // on its way answers `conflict` with `ok: true`: nothing was cancelled, and the work
                // it was meant to stop is running.
                let outcome = Outcome.of(result)
                let sentence = outcome == .succeeded
                    ? "\(operation.displayName) is no longer queued."
                    : (result.message ?? "The queued request could not be cancelled.")
                self.setStatus(sentence, isError: outcome != .succeeded)
                if outcome == .succeeded { self.operations?.finish(operation.id, state: .cancelled, summary: sentence) }
                return .init(state: OperationOutcome.state(outcome, action: result.action), summary: sentence, refresh: true,
                             unresolved: outcome == .unknown)
            } catch let failure as AgentFailure {
                return self.reportFailure(failure, verb: "The cancellation")
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
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

    func runDoctor(deep: Bool = false) {
        guard dialect.supportsDoctor else { return }
        let generation = core.agentGeneration
        perform(deep ? "Checking \(name) thoroughly" : "Checking \(name)",
                kind: .policy, subject: "diagnostics") { _ in
            do {
                let result = try await self.send(.doctor(deep: deep), decoding: AgentDoctorResult.self)
                // The checks are about the agent that ran them, and it may have been replaced
                // while they ran.
                if self.core.agentGeneration == generation {
                    self.core.doctor = result
                    self.core.doctorRanAt = Date()
                }
                let checks = result.checks ?? []
                let bad = checks.filter { $0.verdict == .failed }.count
                let warned = checks.filter { $0.verdict == .warning }.count
                let sentence = bad == 0 && warned == 0
                    ? "\(self.name) checked out: \(checks.count) checks, nothing wrong."
                    : "\(self.name): \(bad) failing, \(warned) worth a look, out of \(checks.count) checks."
                self.setStatus(sentence, isError: bad > 0)
                return .init(state: .succeeded, summary: sentence)
            } catch let failure as AgentFailure {
                self.setStatus(self.describe(failure), isError: true, detail: failure.detailText)
                return .init(state: .failed, summary: self.describe(failure), detail: failure.detailText)
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
            return try await send(request, decoding: AgentLogsResult.self).text
        } catch let failure as AgentFailure {
            return ["the agent log could not be read: \(describe(failure))"]
        } catch {
            return ["the agent log could not be read: \(error.localizedDescription)"]
        }
    }

    /// The agent's whole diagnostic bundle, as it sent it.
    func fetchBundle() async -> String {
        guard dialect.supportsDoctor else { return "This agent does not support diagnostic bundles." }
        do {
            return try await send(.bundle(), decoding: RawJSON.self).text
        } catch {
            return "The agent diagnostic bundle could not be read: \(error.localizedDescription)"
        }
    }

    /// Recent operations from the far side's own history, which outlives anything this app kept.
    func fetchHistory(limit: Int = 30) async -> [AgentOperation] {
        guard dialect.supportsHistory else { return [] }
        do {
            return try await send(.history(limit: limit), decoding: AgentHistoryResult.self).records
        } catch {
            return []
        }
    }

    // MARK: - Trusted agent deployment

    /// Why installing the bundled agent would take this device back rather than forward, or nil
    /// when it would not.
    var bundledAgentGoesBack: String? {
        guard let bundle = AgentBundle.shared, let status else { return nil }
        return AgentDowngrade.reason(installing: bundle.version, over: status)
    }

    /// Whether going back to the bundled agent may be asked for here at all.
    var canGoBackToBundledAgent: Bool {
        bundledAgentGoesBack != nil && status.map(AgentDowngrade.isOffered) == true
    }

    /// Going back is a decision of its own, taken in front of a question that says what it does.
    func requestBundledAgentDowngrade() {
        guard let bundle = AgentBundle.shared, let reason = bundledAgentGoesBack, canGoBackToBundledAgent else { return }
        let floor = status?.agent?.floor
        let lowers = floor.map { AgentVersion.isOlder(bundle.version, than: $0) } == true
            ? " It also lowers the version floor \(name) keeps to \(bundle.version)."
            : ""
        ask?(PendingDialog(
            id: "agent-downgrade-\(historyMachineId)",
            title: "Go back to agent \(bundle.version) on \(name)?",
            message: "\(reason). Going back replaces the agent with this older one, signed by the same release key.\(lowers) Do this only to undo a release that broke something.",
            confirmTitle: "Go back to \(bundle.version)",
            perform: { [weak self] in self?.installBundledAgent(allowDowngrade: true) }
        ))
    }

    /// Install the agent this build was made with, after verifying its signature here.
    ///
    /// Never as an upgrade when it is older than the agent running, or below the version floor: an
    /// agent 3.1.0 refuses that without `--allow-downgrade`, and this app used to offer it all the
    /// same. `allowDowngrade` comes only from the question above.
    func installBundledAgent(allowDowngrade: Bool = false) {
        guard let bundle = AgentBundle.shared, let status, status.ok == true, isCommandable else { return }
        let goesBack = AgentDowngrade.reason(installing: bundle.version, over: status) != nil
        guard goesBack == allowDowngrade, !allowDowngrade || AgentDowngrade.isOffered(by: status) else { return }
        if let previous = unresolvedOperations.first(where: { $0.kind == .appUpdate && $0.subject == "control agent" }) {
            setStatus("Agent installation \(previous.id) still has an unknown outcome. Check its result before starting another installation.", isError: true)
            reconcileNow()
            return
        }
        let isLegacy = AgentBootstrap.isAuthenticatedLegacy(status)
        guard isLegacy || status.speaksRequiredContract else { return }
        perform("Installing agent \(bundle.version) on \(name)", kind: .appUpdate,
                subject: "control agent") { operation in
            do {
                let payload = try bundle.verifiedArchive()
                self.operations?.addPhase(operation, name: "verified", detail: "signature and sha256 checked")
                if isLegacy {
                    return await self.installOverLegacyAgent(payload, operation: operation, version: bundle.version)
                }
                return await self.dispatch(operation: operation, subject: "control agent", verb: "Installing the agent",
                                           build: { _ in
                                               try AgentRequest.selfUpdateFromStdin(operation: operation, allowDowngrade: allowDowngrade,
                                                                                    agent: status.agent).sending(payload)
                                           },
                                           intent: .init(operationId: operation)) { result, _ in
                    guard result.didInstallAgent else {
                        return self.otherOutcome(result, subject: "control agent", verb: "Installing the agent")
                    }
                    let sentence = "Agent \(bundle.version) is installed on \(self.name)."
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
    func dispatch(
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
            var driver = OperationDriver(
                send: { [weak self] request in
                    guard let self else { throw CancellationError() }
                    return try await self.send(request, decoding: AgentActionResult.self)
                },
                readOperation: { [weak self] request in
                    guard let self else { throw CancellationError() }
                    return try await self.send(request, decoding: AgentOperationResult.self)
                },
                onProgress: { [weak self] op in
                    self?.operations?.note(operation, phase: op.phase, progress: op.progress?.description)
                    if let phase = op.phase {
                        self?.setStatus("\(verb): \(phase)\(op.progress?.note.map { " — \($0)" } ?? "").")
                    }
                    self?.onStateChange?()
                }
            )
            driver.pause = core.pollPause

            core.watchedOperation = operation
            defer { core.watchedOperation = nil }

            let conclusion = try await driver.perform(request, operationId: intent.operationId, dialect: dialect)

            if conclusion.replayed {
                // The far side already had this id and handed the record back unchanged. That is the
                // whole point of carrying one: the retry after a dropped link did not run it twice.
                setStatus("\(verb) had already been sent, so \(name) returned the record it already had rather than doing it again.")
            }

            if conclusion.stillRunning {
                let sentence = "\(verb) is still running on \(name). It carries on there whether this window is open or not."
                setStatus(sentence)
                return .init(state: .running, summary: sentence, unresolved: true)
            }

            let result = conclusion.result

            if result.isConflict {
                let what = result.conflict?.summary ?? "another operation"
                let sentence = "\(verb) was refused: \(what) is already running on \(name)."
                setStatus(sentence, isError: true, detail: result.message)
                return .init(state: .conflict, summary: sentence, detail: result.message, refresh: true)
            }

            if result.isQueued {
                let expires = ISO8601DateFormatter.lenient.date(from: result.expiresAt ?? result.op?.expiresAt)
                let when = expires.map { ", expiring \($0.relativeDescription())" } ?? ""
                let sentence = "\(verb) is queued and runs the next time \(name) is idle\(when)."
                setStatus(sentence)
                return .init(state: .queued, summary: sentence, detail: result.message,
                             refresh: true, expiresAt: expires)
            }

            // The outcome is what this app decides on, and an action word is no evidence when the
            // agent itself does not know how the work went: a producer that knows a failed update
            // may have changed the machine says `unknown` whatever the action beside it says. The
            // record is left owed and read again by its id, never sent again.
            if Outcome.of(result) == .unknown {
                // The agent's own words first: a reason code's sentence was written for an ending
                // that is known, and "failed" is exactly what this is not.
                let said = result.message ?? result.reason?.sentence(subject: subject, message: nil)
                let sentence = "\(verb) may or may not have taken effect: \(said ?? "\(name) could not say.")"
                setStatus(sentence, isError: true, detail: result.message)
                return .init(state: .unknown, summary: sentence, detail: result.message,
                             refresh: true, unresolved: true)
            }

            return interpret(result, conclusion)
        } catch let invalid as AgentRequest.Invalid {
            setStatus(invalid.message, isError: true)
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

    /// The agent looked at the device as it is now and said no.
    func deferred(
        _ result: AgentActionResult,
        subject: String,
        verb: String,
        force: Bool,
        forceAgain: @escaping @MainActor () -> Void
    ) -> ActionResolution {
        let reason = result.reason
        let sentence = reason?.sentence(subject: subject, message: result.message)
            ?? result.message
            ?? "\(name) is busy."
        setStatus("\(verb) was held back: \(sentence) Nothing was interrupted.")

        // Force is offered, never taken, and only after the agent has refused on the strength of a
        // fresh look. It skips the busy check and nothing else: a policy refusal is not something
        // force overrides, because a manual request already ignores the schedule. Without the offer
        // a stuck busy probe would leave a device that could not be updated by any means.
        let overridable = reason == .busy || reason == .busyUnknown || reason == nil
        if !force, overridable {
            ask?(PendingDialog(
                id: "force-\(historyMachineId)-\(subject)",
                title: "\(verb) \(subject) anyway?",
                message: "Activity is busy or unconfirmed. Continuing skips the busy check and may lose work in progress.",
                confirmTitle: "Do it anyway",
                perform: forceAgain
            ))
        }
        return .init(state: .deferred, summary: "\(verb) was held back: \(sentence)",
                     detail: result.message, awaitingDecision: !force && overridable)
    }

    /// Everything the happy paths did not cover, said in the agent's own terms.
    func otherOutcome(_ result: AgentActionResult, subject: String, verb: String) -> ActionResolution {
        let reason = result.reason
        let sentence = reason?.sentence(subject: subject, message: result.message)
            ?? result.message
            ?? "\(verb) did not go through."
        let state = OperationOutcome.state(for: result, stillRunning: false)
        setStatus(sentence, isError: state.isProblem, detail: result.message)
        return .init(state: state, summary: sentence, detail: result.message, refresh: true)
    }

    /// The one place a mutation failure becomes words, so the timeout rule is written once.
    func reportFailure(_ failure: AgentFailure, verb: String) -> ActionResolution {
        switch failure.dispatch {
        case .never:
            let sentence = "\(verb) did not happen: \(describe(failure))"
            setStatus(sentence, isError: true, detail: failure.detailText)
            return .init(state: .failed, summary: sentence, detail: failure.detailText)
        case .unknown, .acknowledged:
            // The one that used to be guessed at. A timeout or a dropped link says nothing about
            // whether the far side did the work, and this app has no business claiming either way.
            // The record stays owed and the next status settles it.
            let sentence = "\(verb) may or may not have happened: \(describe(failure))"
            setStatus(sentence, isError: true, detail: failure.detailText)
            return .init(state: .unknown, summary: sentence, detail: failure.detailText,
                         refresh: true, unresolved: true)
        }
    }

    // MARK: - Plumbing

    /// A disruptive request, with the third answer the app used to be missing: neither interrupt the
    /// work nor give up, but wait for the device to be idle.
    func offerDisruptive(
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

    /// Start one action: write it down, run it, and write down what it ended in.
    ///
    /// Returns false, having started nothing, while another action holds the device.
    @discardableResult
    func perform(
        _ label: String,
        kind: OperationKind,
        subject: String,
        targetVersion: String? = nil,
        _ work: @escaping @MainActor (_ operationId: String) async -> ActionResolution
    ) -> Bool {
        guard core.activity == nil else { return false }
        core.activity = label
        core.lastActionOutcome = nil
        setStatus(label + ".")

        let system = recordedSystem
        let record = OperationRecord(
            id: AgentToken.newOperationId(),
            machineId: historyMachineId,
            machineName: name,
            systemId: system.id,
            systemName: system.name,
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
            // Some endings have nothing of their own to say: a cancelled agent install, or one
            // refused before anything was sent. The line would go on saying the action was under
            // way, so it says what the action ended in instead.
            if self.core.line == label + "." {
                self.setStatus(resolution.summary, isError: resolution.state.isProblem, detail: resolution.detail)
            }
            if resolution.state == .queued {
                self.operations?.markQueued(record.id, expiresAt: resolution.expiresAt, summary: resolution.summary)
            } else {
                self.operations?.finish(
                    record.id,
                    state: resolution.state,
                    summary: resolution.summary,
                    detail: resolution.detail,
                    output: resolution.output,
                    route: self.recordedRoute,
                    forceUnresolved: resolution.unresolved
                )
            }
            // A question on screen keeps the action open, so nothing is read under it either way.
            if resolution.refresh || (self.readsAfterEveryAction && !resolution.awaitingDecision) {
                await self.refreshQuietly()
            }
            self.core.activity = nil
            self.core.lastActionOutcome = ActionOutcome(
                text: resolution.summary,
                isError: resolution.state.isProblem,
                operationId: record.id
            )
            if let finished = self.operations?.record(id: record.id) {
                self.onOperationFinished?(finished)
            }
            self.onStateChange?()
        }
        return true
    }
}
