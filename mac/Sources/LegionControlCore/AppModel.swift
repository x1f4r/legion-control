import Foundation
import Observation

/// Everything the app knows: one model per machine in the setup, plus the device this runs on.
///
/// This object owns nothing about any particular machine. It holds the shared setup and this
/// device's private bindings, rebuilds the models when either changes, fans the poll out across
/// them, keeps the one place a question is shown, and owns the operation history the machines all
/// write into.
///
/// It is also where "every device is a peer" is actually true. There is no authority flag and no
/// follower mode: this device edits the setup when the user edits it, publishes it to any machine
/// holding an ancestor of it, adopts a machine's copy when that copy is ahead, and raises a question
/// when neither is true.
@MainActor
@Observable
final class AppModel {
    private(set) var machines: [MachineModel] = []
    /// The device the app runs on, when it looks after anything. Kept beside the others rather than
    /// folded into them: the same agent, but not the same machine.
    private(set) var mac: MacModel?

    /// The question waiting for an answer, whoever raised it.
    var dialog: PendingDialog?
    /// The setup disagreement waiting to be settled, when there is one.
    var divergence: MachineModel.SetupDivergence?

    let config: ConfigStore
    /// This device's own settings, which are never published to anyone.
    let bindings: BindingsStore

    /// Legion Control's own updates.
    let appUpdates: AppUpdateModel

    /// Everything anyone has asked for, kept across launches.
    let operations: OperationStore

    private var pollTask: Task<Void, Never>?

    /// Called whenever something the menu bar draws has changed.
    var onStateChange: (@MainActor () -> Void)?

    init(
        config: ConfigStore = ConfigStore(),
        bindings: BindingsStore = BindingsStore(),
        operations: OperationStore = OperationStore(),
        appUpdates: AppUpdateModel = AppUpdateModel()
    ) {
        self.config = config
        self.bindings = bindings
        self.operations = operations
        self.appUpdates = appUpdates
        appUpdates.onStateChange = { [weak self] in self?.onStateChange?() }
        config.onChange = { [weak self] in
            guard let self else { return }
            _ = self.config.reconcileExternalEdit(deviceName: self.bindings.bindings.effectiveDeviceName)
            self.rebuild()
        }
        bindings.onChange = { [weak self] in self?.rebuild() }
        appUpdates.repo = { [weak self] in self?.config.config?.updateRepo ?? ControllerConfig.AppUpdatesConfig.defaultRepo }
        appUpdates.operations = operations
        _ = config.reconcileExternalEdit(deviceName: bindings.bindings.effectiveDeviceName)
        rebuild()
    }

    // MARK: - Where this device is

    /// Recomputed when a viewer appears rather than on every draw: enumerating interfaces is cheap
    /// but not free, and a laptop does not change network between two rows of a table.
    private(set) var placement = SiteAwareness.Placement(matching: [], confirmed: nil)

    func refreshPlacement() {
        placement = SiteAwareness.placement(
            sites: config.config?.allSites ?? [],
            addresses: SiteAwareness.localAddresses(),
            confirmed: bindings.bindings.currentSite
        )
    }

    /// The sites the setup names, for the picker.
    var sites: [Site] { config.config?.allSites ?? [] }

    /// Say outright which site this device is at, because prefixes cannot tell two identical private
    /// networks apart and guessing wrong means broadcasting into the wrong house.
    func confirmSite(_ id: String?) {
        bindings.update { $0.currentSite = id }
        refreshPlacement()
        onStateChange?()
    }

    // MARK: - Following the setup

    /// Builds the models the setup asks for, keeping the ones whose machine has not changed.
    ///
    /// Keeping them matters more than it looks: a rebuild throws away the link state, the last
    /// reading and anything in flight, and the config file is written by hand while the app is open.
    /// An edit to one machine must not reset the others.
    private func rebuild() {
        appUpdates.repositoryDidChange()
        let binding = bindings.bindings
        let selfMachineId = binding.canControlSelfLocally ? binding.selfBinding?.machine : nil

        let existing = Dictionary(machines.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        machines = config.machines
            // A machine this device *is*, and can drive by spawning an agent here, is not reached
            // over ssh at all: it is the local model below. Without a local agent binding it stays
            // an ordinary machine, dialled at whatever address the user configured, because
            // inventing a loopback endpoint would be inventing a route nobody asked for.
            .filter { $0.id != selfMachineId }
            .map { machine in
                if let model = existing[machine.id], model.machine == machine { return model }
                return makeMachineModel(machine)
            }

        rebuildLocalModel(selfMachineId: selfMachineId)

        // A question raised by a machine that has just been edited out of the setup has nothing left
        // to act on.
        if dialog != nil, machines.isEmpty, mac == nil { dialog = nil }
        if let divergence, machine(id: divergence.machineId) == nil, divergence.machineId != mac?.historyMachineId {
            self.divergence = nil
        }

        refreshPlacement()
        onStateChange?()
        if pollTask != nil {
            Task { @MainActor in await refreshEverything(userInitiated: false) }
        }
    }

    private func makeMachineModel(_ machine: Machine) -> MachineModel {
        let model = MachineModel(machine: machine)
        wire(model)
        return model
    }

    private func wire(_ model: MachineModel) {
        model.onStateChange = { [weak self] in self?.onStateChange?() }
        model.ask = { [weak self] dialog in self?.dialog = dialog }
        model.controllerConfig = { [weak self] in self?.config.document }
        model.operations = operations
        model.onOperationFinished = { [weak self] record in self?.announce(record) }
        model.bindings = { [weak self] in self?.bindings.bindings ?? .empty }
        model.setup = { [weak self] in self?.config.config }
        model.peer = { [weak self] id in self?.machine(id: id) }
        model.localPeer = { [weak self] id in
            guard let local = self?.mac, local.boundMachineId == id else { return nil }
            return local
        }
        model.migrateAgentCommand = { [weak self, weak model] system, argv in
            guard let self, let model else { return }
            _ = self.editSetup("updated the agent launcher on \(model.name)") { root in
                try ControllerEditor.upsertSystem(["id": system, "agent": argv], onMachine: model.id, in: &root)
            }
        }
        model.placement = { [weak self] in self?.placement ?? SiteAwareness.Placement(matching: [], confirmed: nil) }
        model.adoptSetup = { [weak self] bytes, description in
            self?.config.adopt(bytes, describedAs: description)
        }
        model.setupBase = { [weak self] mine, theirs in
            self?.config.commonAncestor(mine: mine, theirs: theirs)
        }
        model.onSetupDiverged = { [weak self] divergence in self?.divergence = divergence }
    }

    /// The local model: either this device's own agent as configured by `local`, or the machine this
    /// device is bound to when the bindings say so.
    private func rebuildLocalModel(selfMachineId: String?) {
        if let selfMachineId, let machine = config.config?.machine(id: selfMachineId),
           let argv = bindings.bindings.localAgent?.argv, !argv.isEmpty {
            let local = LocalConfig(enabled: true, name: bindings.bindings.effectiveDeviceName, agent: argv.last ?? "")
            if mac?.config.name != local.name || mac?.boundMachineId != selfMachineId || mac?.boundAgentArgv != argv {
                let model = MacModel(config: local, agent: MacAgent(argv: argv))
                model.boundMachineId = selfMachineId
                model.boundAgentArgv = argv
                wireLocal(model)
                mac = model
            }
            mac?.boundMachine = machine
            return
        }

        if !bindings.isPresent, let local = config.local {
            if mac?.config != local || mac?.boundMachineId != nil {
                let model = MacModel(config: local)
                wireLocal(model)
                mac = model
            }
        } else {
            mac = nil
        }
    }

    private func wireLocal(_ model: MacModel) {
        model.migrateAgentCommand = { [weak self, weak model] argv in
            guard let self, model?.boundMachineId != nil else { return }
            _ = self.bindings.update { $0.localAgent = .init(argv: argv) }
        }
        model.onStateChange = { [weak self] in self?.onStateChange?() }
        model.ask = { [weak self] dialog in self?.dialog = dialog }
        model.controllerConfig = { [weak self] in self?.config.document }
        model.operations = operations
        model.onOperationFinished = { [weak self] record in self?.announce(record) }
        model.adoptSetup = { [weak self] bytes, description in
            self?.config.adopt(bytes, describedAs: description)
        }
        model.setupBase = { [weak self] mine, theirs in
            self?.config.commonAncestor(mine: mine, theirs: theirs)
        }
        model.onSetupDiverged = { [weak self] divergence in self?.divergence = divergence }
    }

    func replaceSetupOnPeer(_ divergence: MachineModel.SetupDivergence) {
        self.divergence = nil
        if let machine = machine(id: divergence.machineId) {
            machine.replaceSetup()
        } else if mac?.historyMachineId == divergence.machineId {
            mac?.replaceSetup()
        }
    }

    func machine(id: String) -> MachineModel? { machines.first { $0.id == id } }

    /// Whether there is anything at all to draw. False means the window shows its setup page.
    var isConfigured: Bool { !machines.isEmpty || mac != nil }

    // MARK: - Editing the setup

    /// Apply a change to the shared document.
    ///
    /// Every device may do this. The edit lands here immediately — an offline edit is a first-class
    /// edit — and reaches each machine on the next poll, as a fast-forward from whatever it holds.
    @discardableResult
    func editSetup(_ description: String, _ change: (inout [String: Any]) throws -> Void) -> String? {
        let problem = config.applyEdit(
            describedAs: description,
            deviceName: bindings.bindings.effectiveDeviceName,
            change: change
        )
        if problem == nil { refreshPlacement() }
        onStateChange?()
        return problem
    }

    /// Give a hand written or pasted document an identity, so it can take part in reconciliation.
    @discardableResult
    func adoptOwnDocument() -> String? {
        guard let raw = config.rawBytes, config.config?.identity.id == nil else { return nil }
        do {
            let edited = try ControllerEditor.adopting(raw, deviceName: bindings.bindings.effectiveDeviceName)
            let problem = config.adopt(edited.bytes, describedAs: "gave this setup an identity")
            onStateChange?()
            return problem
        } catch let failure as ControllerEditor.EditFailure {
            return failure.message
        } catch {
            return error.localizedDescription
        }
    }

    /// Settle a divergence by merging the two branches.
    @discardableResult
    func resolveDivergence(_ divergence: MachineModel.SetupDivergence,
                           choices: [String: SetupMerge.Choice]) -> String? {
        do {
            let identity = divergence.mine.identity.merged(
                with: divergence.theirsIdentity,
                mineHash: divergence.mine.hash,
                theirsHash: Canonical.sha256(divergence.theirsBytes),
                device: bindings.bindings.effectiveDeviceName
            )
            let merged = try SetupMerge.merge(
                mine: divergence.mine.bytes,
                theirs: divergence.theirsBytes,
                base: divergence.baseBytes,
                differences: divergence.differences,
                choices: choices,
                identity: identity
            )
            // Both branches are kept, so whichever machine is holding either of them accepts the
            // merge as a fast-forward.
            config.keepRevision(divergence.mine.bytes)
            config.keepRevision(divergence.theirsBytes)
            let problem = config.adopt(merged, describedAs: "merged with \(divergence.machineName)")
            if problem == nil { self.divergence = nil }
            onStateChange?()
            return problem
        } catch let failure as SetupMerge.MergeFailure {
            return failure.message
        } catch let failure as Canonical.NotUTF8 {
            _ = failure
            return "The merged document is not valid UTF-8, so nothing was written."
        } catch {
            return error.localizedDescription
        }
    }

    /// Settle a divergence by keeping this device's branch.
    ///
    /// Not a replace: the new revision lists the machine's document as one of its parents, so the
    /// machine accepts it as an ordinary fast-forward and no `--replace` is ever sent.
    @discardableResult
    func keepMine(_ divergence: MachineModel.SetupDivergence) -> String? {
        let choices = Dictionary(uniqueKeysWithValues: divergence.differences.map { ($0.id, SetupMerge.Choice.mine) })
        return resolveDivergence(divergence, choices: choices)
    }

    /// Settle a divergence by taking the machine's branch.
    @discardableResult
    func takeTheirs(_ divergence: MachineModel.SetupDivergence) -> String? {
        config.keepRevision(divergence.mine.bytes)
        let problem = config.adopt(divergence.theirsBytes, describedAs: "took the setup from \(divergence.machineName)")
        if problem == nil { self.divergence = nil }
        onStateChange?()
        return problem
    }

    // MARK: - Aggregates

    var isWorking: Bool { machines.contains(where: \.isWorking) || mac?.isWorking == true }

    var isRefreshing: Bool { machines.contains(where: \.isRefreshing) || mac?.isRefreshing == true }

    /// The machine that spoke last. The window has one status bar for the whole app, so it shows
    /// whatever happened most recently rather than picking a favourite.
    private var mostRecent: MachineModel? {
        machines.max { $0.statusStamp < $1.statusStamp }
    }

    var statusLine: String {
        guard let recent = mostRecent else { return mac?.note ?? "Ready." }
        if let mac, mac.noteStamp > recent.statusStamp, let note = mac.note { return note }
        return recent.statusLine
    }

    var statusIsError: Bool {
        guard let recent = mostRecent else { return mac?.noteIsError ?? false }
        if let mac, mac.noteStamp > recent.statusStamp, mac.note != nil { return mac.noteIsError }
        return recent.statusIsError
    }

    var statusDetail: String? {
        guard let recent = mostRecent else { return nil }
        if let mac, mac.noteStamp > recent.statusStamp, mac.note != nil { return nil }
        return recent.statusDetail
    }

    /// What the last action the user started ended in.
    var lastActionOutcome: MachineModel.ActionOutcome? {
        machines
            .filter { $0.lastActionOutcome != nil }
            .max { $0.statusStamp < $1.statusStamp }?
            .lastActionOutcome
    }

    /// Operations this app started and never learned the outcome of. The one thing that asks for
    /// attention on its own, because an update that may or may not have installed is a question the
    /// user has to be able to see and settle.
    var unresolvedOperations: [OperationRecord] { operations.unresolved }

    /// Anything the agents could not read, machine by machine.
    var agentNotes: [(machine: String, notes: [String])] {
        var out: [(String, [String])] = []
        for machine in machines where !machine.notes.isEmpty {
            out.append((machine.name, machine.notes))
        }
        if let mac, !mac.notes.isEmpty { out.append((mac.name, mac.notes)) }
        return out
    }

    /// Warnings about the setup itself, which are not reasons to refuse it.
    var setupWarnings: [String] { config.config?.warnings() ?? [] }

    /// The freshest reading anywhere, for the "checked N minutes ago" line.
    var lastChecked: Date? {
        ([mac?.lastChecked] + machines.map(\.lastChecked)).compactMap { $0 }.max()
    }

    /// The symbol the menu bar draws.
    var menuBarSymbol: String {
        if !operations.unresolved.isEmpty { return "questionmark.circle" }
        if appUpdates.availableVersion != nil { return "arrow.down.circle" }
        guard let first = machines.first else { return mac == nil ? "circle.dotted" : "laptopcomputer" }
        if machines.count == 1 { return first.symbolName }
        if machines.contains(where: { $0.rebootInProgress != nil && !$0.isAwake }) {
            return "arrow.triangle.2.circlepath"
        }
        if machines.contains(where: { $0.currentSystem != nil }) { return "desktopcomputer" }
        if machines.contains(where: \.isAwake) { return "exclamationmark.triangle" }
        return "moon.zzz"
    }

    var menuBarDescription: String {
        if let version = appUpdates.availableVersion { return "Version \(version) is available. \(fleetMenuBarDescription)" }
        return fleetMenuBarDescription
    }

    private var fleetMenuBarDescription: String {
        guard let first = machines.first else { return mac == nil ? "No machines configured" : "This device only" }
        if machines.count == 1 { return first.stateDescription }
        let awake = machines.filter(\.isAwake).count
        return "\(awake) of \(machines.count) machines awake"
    }

    /// How old the reading is, in plain words. Nothing polls while no viewer is on screen, so the
    /// menu has to be honest about showing something that was true some minutes ago.
    static func freshness(of date: Date?, now: Date = Date()) -> String {
        guard let date else { return "not checked yet" }
        let seconds = Int(now.timeIntervalSince(date).rounded())
        switch seconds {
        case ..<20: return "checked just now"
        case ..<90: return "checked \(seconds) seconds ago"
        case ..<5400:
            let minutes = Int((Double(seconds) / 60).rounded())
            return "checked \(minutes) minute\(minutes == 1 ? "" : "s") ago"
        default:
            let hours = Int((Double(seconds) / 3600).rounded())
            return "checked \(hours) hour\(hours == 1 ? "" : "s") ago"
        }
    }

    // MARK: - Telling the user something finished

    /// Notify only for what the user explicitly asked for, and only for the outcomes worth
    /// interrupting them about. A status poll finding a machine asleep is not news; an update they
    /// started ten minutes ago finishing, failing, or ending in an outcome nobody knows, is.
    private func announce(_ record: OperationRecord) {
        guard record.kind.isDisruptive else { return }
        switch record.state {
        case .succeeded, .failed, .unknown:
            Notifications.post(record)
        case .running, .noop, .deferred, .queued, .conflict, .expired, .cancelled:
            break
        }
    }

    // MARK: - Polling
    //
    // Only a viewer polls, and there are two of them: the window and the panel under the menu bar
    // icon. Every reading costs a process, so when neither is on screen there is no timer, no task
    // and no fleet polling. Release checks use their own infrequent HTTPS cadence.

    var isPolling: Bool { pollTask != nil }

    func startPolling() {
        guard pollTask == nil else { return }
        pollTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                self?.config.reloadIfChanged()
                await self?.refreshEverything(userInitiated: false)
                try? await Task.sleep(for: .seconds(15))
            }
        }
    }

    func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    /// The window or the panel came on screen.
    func viewerAppeared() {
        config.reloadIfChanged()
        refreshPlacement()
        // Foreground entry rechecks stale release metadata independently of fleet polling.
        appUpdates.checkIfStale()
        startPolling()
    }

    /// The last viewer went away. Fleet polling stops here.
    func viewerDisappeared() {
        stopPolling()
    }

    /// Every machine at once. They are independent processes and each spends nearly all its time
    /// waiting, so reading them one after another would make a window with three machines in it
    /// three times as slow to fill in for no reason.
    func refreshEverything(userInitiated: Bool) async {
        bindings.reloadIfChanged()
        var tasks: [Task<Void, Never>] = []
        for machine in machines {
            tasks.append(Task { @MainActor in await machine.refresh(userInitiated: userInitiated) })
        }
        if let mac {
            tasks.append(Task { @MainActor in await mac.refresh() })
        }
        for task in tasks { await task.value }
    }

    func refreshIfNeeded(minimumAge: TimeInterval = 2) async {
        var tasks: [Task<Void, Never>] = []
        for machine in machines {
            tasks.append(Task { @MainActor in await machine.refreshIfNeeded(minimumAge: minimumAge) })
        }
        if let mac {
            tasks.append(Task { @MainActor in await mac.refreshIfNeeded(minimumAge: minimumAge) })
        }
        for task in tasks { await task.value }
    }

    // MARK: - Answering a question

    /// The "yes" to whichever question is up. Shared by the window's alert and the panel's inline
    /// question, so the two can never disagree about what a confirmation does.
    func confirm(_ dialog: PendingDialog) {
        self.dialog = nil
        dialog.perform()
    }

    /// The third answer, when the question has one: neither now nor never, but when the machine is
    /// next idle.
    func chooseAlternative(_ dialog: PendingDialog) {
        self.dialog = nil
        dialog.alternative?()
    }

    // MARK: - Diagnostics

    /// Everything a bug report needs, as one piece of text.
    func diagnosticsBundle() async -> String {
        var lines: [String] = []
        lines.append("Legion Control diagnostics")
        lines.append("generated \(ISO8601DateFormatter.lenient.string(from: Date()))")
        lines.append("app \(appUpdates.installedVersion)")
        lines.append("config \(config.path)")
        lines.append("bindings \(bindings.path)")
        if let identity = config.config?.identity {
            lines.append("setup \(identity.id ?? "unnamed") revision \(identity.revisionNumber), last written by \(identity.authorDescription)")
            lines.append("lineage \(identity.ancestors.map { String($0.prefix(12)) }.joined(separator: " ← "))")
        }
        lines.append("this device \(bindings.bindings.effectiveDeviceName)")
        lines.append("site \(placement.description(sites: sites))")
        if let problem = config.problem { lines.append("config problem: \(problem)") }
        if let problem = bindings.problem { lines.append("bindings problem: \(problem)") }
        if let problem = operations.persistenceProblem { lines.append("history problem: \(problem)") }
        for warning in setupWarnings { lines.append("setup warning: \(warning)") }
        lines.append("")

        for machine in machines {
            lines.append("== \(machine.name) (\(machine.id))")
            lines.append("link: \(machine.stateDescription)")
            lines.append("agent: \(machine.status?.version ?? "not read"), \(machine.dialect.description)")
            lines.append("route: \(machine.lastRoute?.label ?? "none recorded")")
            lines.append("setup: \(machine.setupDecision)")
            for note in machine.notes { lines.append("note: \(note)") }
            for service in machine.services {
                lines.append("service \(service.id): installed=\(service.installed ?? "-") latest=\(service.latest ?? "-") running=\(service.running.map(String.init) ?? "-") healthy=\(service.healthy.map(String.init) ?? "-") busy=\(service.busy?.summary ?? "-")")
            }
            for line in await machine.fetchLog() { lines.append("log: \(line)") }
            lines.append(await machine.fetchBundle())
            lines.append("")
        }

        if let mac {
            lines.append("== \(mac.name) (local)")
            lines.append("agent: \(mac.status?.version ?? "not read"), \(mac.dialect.description)")
            if let failure = mac.failure { lines.append("failure: \(failure.localMessage)") }
            for note in mac.notes { lines.append("note: \(note)") }
            for line in await mac.fetchLog() { lines.append("log: \(line)") }
            lines.append(await mac.fetchBundle())
            lines.append("")
        }

        lines.append(operations.exportText())
        return lines.joined(separator: "\n")
    }

    // MARK: - Setting up

    /// Writes the documented example config, but never over anything that is already there.
    @discardableResult
    func writeExampleConfig() -> String? {
        config.writeExample()
    }

    /// Opens the config in whatever the user edits text with. `open -t` is the system's own answer to
    /// that question, so the app does not have to have an opinion.
    func openConfigInEditor() {
        let path = config.path
        Task.detached {
            _ = await Shell.run(executable: "/usr/bin/open", arguments: ["-t", path], timeout: 20)
        }
    }

    func openBindingsInEditor() {
        let path = bindings.path
        Task.detached {
            _ = await Shell.run(executable: "/usr/bin/open", arguments: ["-t", path], timeout: 20)
        }
    }

    func movePrivateSettingsOut() -> String? {
        if config.local != nil, !bindings.bindings.canControlSelfLocally {
            return "First choose this device's machine and local agent command under Setup → This device. The existing local configuration has been retained."
        }
        if let problem = bindings.update({ value in
            for machine in config.machines {
                guard let key = machine.ssh?.identityFile, !key.isEmpty else { continue }
                var specific = value.machines?[machine.id] ?? .init()
                if specific.identityFile == nil { specific.identityFile = key }
                if value.machines == nil { value.machines = [:] }
                value.machines?[machine.id] = specific
            }
        }) { return problem }
        return editSetup("moved this device's private settings out of the shared setup") { root in
            try ControllerEditor.stripPrivateKeys(in: &root)
        }
    }
}
