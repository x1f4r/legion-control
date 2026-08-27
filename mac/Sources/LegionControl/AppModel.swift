import Foundation
import Observation

/// Everything the app knows, which is one model per configured machine plus, when the config asks
/// for it, the device the app itself runs on.
///
/// This object owns nothing about any particular machine. It holds the config, rebuilds the models
/// when the config changes, fans the poll out across them, and keeps the one place a question is
/// shown, because only one question can be answered at a time whichever machine raised it.
@MainActor
@Observable
final class AppModel {
    private(set) var machines: [MachineModel] = []
    /// The device the app runs on, when the config enables it. Kept beside the others rather than
    /// folded into them: the same agent, but not the same machine, and nothing about one should ever
    /// be read off another.
    private(set) var mac: MacModel?

    /// The question waiting for an answer, whoever raised it.
    var dialog: PendingDialog?

    let config: ConfigStore

    /// Legion Control's own updates. Owned here rather than by the section that draws it, so the
    /// window and the panel read the same answer and one look serves both.
    let appUpdates = AppUpdateModel()

    private var pollTask: Task<Void, Never>?

    /// Called whenever something the menu bar draws has changed. The status item redraws from this
    /// rather than keeping a timer of its own.
    var onStateChange: (@MainActor () -> Void)?

    init(config: ConfigStore = ConfigStore()) {
        self.config = config
        config.onChange = { [weak self] in self?.rebuild() }
        appUpdates.repo = { [weak self] in self?.config.config?.updateRepo ?? ControllerConfig.AppUpdatesConfig.defaultRepo }
        rebuild()
    }

    // MARK: - Following the config

    /// Builds the models the config asks for, keeping the ones whose machine has not changed.
    ///
    /// Keeping them matters more than it looks: a rebuild throws away the link state, the last
    /// reading and anything in flight, and the config file is written by hand while the app is open.
    /// An edit to one machine must not reset the others.
    private func rebuild() {
        let existing = Dictionary(machines.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        machines = config.machines.map { machine in
            if let model = existing[machine.id], model.machine == machine { return model }
            let model = MachineModel(machine: machine)
            model.onStateChange = { [weak self] in self?.onStateChange?() }
            model.ask = { [weak self] dialog in self?.dialog = dialog }
            model.controllerConfig = { [weak self] in self?.config.document }
            return model
        }

        if let local = config.local {
            if mac?.config != local {
                let model = MacModel(config: local)
                model.onStateChange = { [weak self] in self?.onStateChange?() }
                model.ask = { [weak self] dialog in self?.dialog = dialog }
                model.controllerConfig = { [weak self] in self?.config.document }
                mac = model
            }
        } else {
            mac = nil
        }

        // A question raised by a machine that has just been edited out of the config has nothing
        // left to act on.
        if dialog != nil, machines.isEmpty, mac == nil { dialog = nil }

        onStateChange?()
        if pollTask != nil {
            Task { @MainActor in await refreshEverything(userInitiated: false) }
        }
    }

    func machine(id: String) -> MachineModel? { machines.first { $0.id == id } }

    /// Whether there is anything at all to draw. False means the window shows its setup page.
    var isConfigured: Bool { !machines.isEmpty || mac != nil }

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

    /// What the last action the user started ended in. Only ever about something the user did, so a
    /// poll finding a machine asleep never lands here.
    var lastActionOutcome: MachineModel.ActionOutcome? {
        machines
            .filter { $0.lastActionOutcome != nil }
            .max { $0.statusStamp < $1.statusStamp }?
            .lastActionOutcome
    }

    /// The freshest reading anywhere, for the "checked N minutes ago" line.
    var lastChecked: Date? {
        ([mac?.lastChecked] + machines.map(\.lastChecked)).compactMap { $0 }.max()
    }

    /// The symbol the menu bar draws. With one machine this is that machine's state, which is the
    /// whole of what the icon has ever meant; with several it can only say whether anything is up.
    var menuBarSymbol: String {
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
        guard let first = machines.first else { return mac == nil ? "No machines configured" : "This Mac only" }
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

    // MARK: - Polling
    //
    // Only a viewer polls, and there are two of them: the window and the panel under the menu bar
    // icon. Every reading costs a process, so when neither is on screen there is no timer, no task
    // and no work of any kind: the app sits at zero until one of them comes back or an action
    // finishes.

    var isPolling: Bool { pollTask != nil }

    func startPolling() {
        guard pollTask == nil else { return }
        pollTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                // Cheap, and it means an edit to the config lands within one poll even if the
                // directory watcher missed it.
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

    /// The window or the panel came on screen. Answer with what we have, then go and get a fresh
    /// reading.
    func viewerAppeared() {
        config.reloadIfChanged()
        // One HTTPS request every six hours at the most, and only ever while something is looking.
        // This is the only place it is asked for, because opening a viewer is the only moment at
        // which anyone could read the answer.
        appUpdates.checkIfStale()
        startPolling()
    }

    /// The last viewer went away. Everything stops here.
    func viewerDisappeared() {
        stopPolling()
    }

    /// Every machine at once.
    ///
    /// They are independent processes, and each one spends nearly all of its time waiting on the
    /// process it launched, so reading them one after another would make a window with three
    /// machines in it three times as slow to fill in for no reason.
    func refreshEverything(userInitiated: Bool) async {
        var tasks: [Task<Void, Never>] = []
        for machine in machines {
            tasks.append(Task { @MainActor in await machine.refresh(userInitiated: userInitiated) })
        }
        if let mac {
            tasks.append(Task { @MainActor in await mac.refresh() })
        }
        for task in tasks { await task.value }
    }

    /// What the menu and the window reopen use. Cheap to call from anywhere: each model drops the
    /// request if a reading is already in flight or if one landed a moment ago.
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

    // MARK: - Setting up

    /// Writes the documented example config, but never over anything that is already there.
    @discardableResult
    func writeExampleConfig() -> String? {
        config.writeExample()
    }

    /// Opens the config in whatever the user edits text with. `open -t` is the system's own answer
    /// to that question, so the app does not have to have an opinion.
    func openConfigInEditor() {
        let path = config.path
        Task.detached {
            _ = await Shell.run(executable: "/usr/bin/open", arguments: ["-t", path], timeout: 20)
        }
    }
}
