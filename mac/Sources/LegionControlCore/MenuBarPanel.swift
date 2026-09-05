import SwiftUI

/// The little that belongs to the panel itself rather than to any machine: the login item, which is
/// read from launchd when the panel opens rather than on every draw, whether the window is on
/// screen, and the two exits that leave the panel.
@MainActor
@Observable
final class MenuBarPanelState {
    var startsAtLogin = LoginItem.isEnabled
    var needsLoginApproval = LoginItem.needsApproval
    var loginProblem: String?
    var isWindowVisible = false

    @ObservationIgnored var openWindow: @MainActor () -> Void = {}
    @ObservationIgnored var quit: @MainActor () -> Void = {}

    /// System Settings can turn the login item off behind our back, so it is read again every time
    /// the panel comes up rather than trusted from launch.
    func rereadLogin() {
        startsAtLogin = LoginItem.isEnabled
        needsLoginApproval = LoginItem.needsApproval
    }

    func setStartsAtLogin(_ wanted: Bool) {
        loginProblem = LoginItem.set(wanted)
        rereadLogin()
    }
}

/// Everything under the menu bar icon.
///
/// Laid out as one column of short sections separated by hairlines, in the order the questions
/// come: what state is a machine in, what can be done to the box, what is each of its services
/// doing, then the same for the device this runs on, and then the app itself. Each section only
/// shows the controls that make sense right now, so a sleeping machine offers Wake and nothing
/// else, and a running one offers Sleep and the switch to another system. Nothing here is a box, a
/// badge or a capsule.
struct MenuBarPanel: View {
    let model: AppModel
    let panel: MenuBarPanelState

    /// Width chosen so a full version string fits on one line beside its label.
    private static let width: CGFloat = 344

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let dialog = model.dialog {
                Question(dialog: dialog, model: model)
                PanelRule()
            }

            if !model.unresolvedOperations.isEmpty {
                UnknownOutcomes(model: model)
                PanelRule()
            }

            Group {
                if model.isConfigured {
                    ForEach(Array(model.machines.enumerated()), id: \.element.id) { index, machine in
                        MachineBlock(model: model, machine: machine, showsRefresh: index == 0)
                    }
                    if let mac = model.mac {
                        MacBlock(model: model, mac: mac, showsRefresh: model.machines.isEmpty)
                    }
                } else {
                    unconfigured
                    PanelRule()
                }
                lastResult
            }
            // A question at the top is the only thing that wants an answer while it is there. The
            // rest stays readable, since the state is what the decision is about, but not pressable.
            .disabled(model.dialog != nil)
            .opacity(model.dialog == nil ? 1 : 0.45)

            footer
        }
        .frame(width: Self.width)
        .animation(.easeOut(duration: 0.18), value: model.dialog?.id)
        .animation(.easeOut(duration: 0.18), value: model.menuBarSymbol)
        .animation(.easeOut(duration: 0.18), value: model.lastActionOutcome)
    }

    // MARK: - Nothing configured

    private var unconfigured: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("No machines configured.")
                .font(.callout)
            Button("Open the window") { panel.openWindow() }
                .buttonStyle(.accessoryBar)
                .font(.callout)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - The last thing that happened

    /// What the last action did, and nothing else. A poll that found a machine asleep is already
    /// said by its header, and repeating it here as a warning was the one line that contradicted
    /// the rest of the panel once the machine came back.
    @ViewBuilder
    private var lastResult: some View {
        if let outcome = model.lastActionOutcome {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: outcome.isError ? "exclamationmark.triangle" : "checkmark.circle")
                    .foregroundStyle(outcome.isError ? AnyShapeStyle(.orange) : AnyShapeStyle(.tertiary))
                    .imageScale(.small)
                Text(outcome.text)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            PanelRule()
        }
    }

    // MARK: - Footer

    private var footer: some View {
        VStack(alignment: .leading, spacing: 6) {
            appUpdate

            HStack(spacing: 4) {
                Toggle(isOn: Binding(
                    get: { panel.startsAtLogin },
                    set: { panel.setStartsAtLogin($0) }
                )) {
                    Text("Start at login")
                        .font(.callout)
                }
                .toggleStyle(.switch)
                .controlSize(.mini)

                Spacer(minLength: 8)

                Button(panel.isWindowVisible ? "Show window" : "Open window") {
                    panel.openWindow()
                }
                Button("Quit") {
                    panel.quit()
                }
            }
            .buttonStyle(.accessoryBar)
            .font(.callout)

            if let problem = panel.loginProblem {
                PanelNote(text: problem, isError: true)
            } else if panel.needsLoginApproval {
                PanelNote(text: "Waiting for approval in System Settings, under General and then Login Items.")
            }
        }
        .padding(.horizontal, 16)
        .padding(.top, 8)
        .padding(.bottom, 10)
    }

    /// One line, and only when there is a newer build. An app that says "up to date" every time you
    /// open the panel is telling you something you never asked, in the place where the panel is
    /// meant to be at its quietest.
    @ViewBuilder
    private var appUpdate: some View {
        if let version = model.appUpdates.availableVersion {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text("Legion Control \(version) is available")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Spacer(minLength: 8)
                Button(model.appUpdates.phase == .idle ? "Install" : model.appUpdates.summary) {
                    model.appUpdates.install()
                }
                .buttonStyle(.accessoryBar)
                .font(.callout)
                .disabled(model.appUpdates.phase.isWorking)
            }
            .padding(.bottom, 2)
        }
    }
}

// MARK: - One machine

/// A header, what can be done to the box, and one block per service the machine reports.
private struct MachineBlock: View {
    let model: AppModel
    let machine: MachineModel
    /// Only the first block carries the refresh control. There is one reading for the whole app and
    /// one button for it; repeating it under every machine would say there were several.
    let showsRefresh: Bool

    var body: some View {
        header
        PanelRule()
        powerSection
        PanelRule()
        if machine.currentSystem != nil, !machine.services.isEmpty {
            ForEach(Array(machine.services.enumerated()), id: \.element.id) { index, service in
                ServiceBlock(
                    machine: machine,
                    service: service,
                    // With one service the switches sit under it, exactly where they have always
                    // been. With several they would be repeated, so they get a block of their own.
                    showsAutoUpdate: machine.services.count == 1
                )
                if index < machine.services.count - 1 { PanelRule() }
            }
            if machine.services.count > 1, machine.services.contains(where: { $0.canUpdate != false }) {
                PanelRule()
                PanelSection(title: "Automatic updates") {
                    AutoUpdateRow(machine: machine)
                }
            }
            PanelRule()
        } else {
            PanelSection(title: "Services") {
                Text(machine.isAwake
                    ? "Not readable without the control agent."
                    : "Not readable while \(machine.name) is unreachable.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            PanelRule()
        }
        if !machine.actions.isEmpty {
            PanelSection(title: "Actions") {
                FlowRow(spacing: 8) {
                    ForEach(machine.actions) { action in
                        Button(action.displayName) { machine.requestAction(action) }
                    }
                }
                .controlSize(.small)
                .disabled(machine.isWorking || machine.commandableSystem == nil)
            }
            PanelRule()
        }
    }

    // MARK: - Header

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: machine.symbolName)
                .font(.system(size: 22, weight: .regular))
                .foregroundStyle(headerTint)
                .frame(width: 30, height: 30)
                .contentTransition(.symbolEffect(.replace))

            VStack(alignment: .leading, spacing: 2) {
                Text(headline)
                    .font(.headline)
                    .contentTransition(.opacity)
                // The freshness line ticks on its own while the panel is up. Ten seconds is
                // finer than the wording ever changes, and costs nothing.
                TimelineView(.periodic(from: .now, by: 10)) { context in
                    Text(subline(now: context.date))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }

            Spacer(minLength: 8)

            if showsRefresh {
                if model.isRefreshing {
                    ProgressView()
                        .controlSize(.small)
                        .frame(width: 24, height: 24)
                } else {
                    Button {
                        Task { await model.refreshEverything(userInitiated: false) }
                    } label: {
                        Image(systemName: "arrow.clockwise")
                            .frame(width: 24, height: 24)
                    }
                    .buttonStyle(.accessoryBar)
                    .disabled(model.isWorking)
                    .help("Read everything again")
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.top, 14)
        .padding(.bottom, 12)
    }

    private var headerTint: Color {
        if machine.rebootInProgress != nil, !machine.isAwake { return .orange }
        switch machine.link {
        case .online: return .green
        case .reachableWithoutAgent: return .orange
        case .offline, .unknown: return .secondary
        }
    }

    private var headline: String {
        if let reboot = machine.rebootInProgress, !machine.isAwake { return "Restarting into \(reboot.target.name)" }
        switch machine.link {
        case .online(let system): return "\(system.name) is running"
        case .reachableWithoutAgent(let system): return "\(system.name) is awake, no control agent"
        case .offline: return "Asleep or unreachable"
        case .unknown: return "Checking \(machine.name)"
        }
    }

    /// What is happening right now if something is, otherwise how old the reading is. Nothing polls
    /// while the panel is closed, so this has to be honest about a reading that is minutes old.
    private func subline(now: Date) -> String {
        if let activity = machine.activity { return activity }
        if machine.isRefreshing { return "checking now" }
        var parts = [AppModel.freshness(of: machine.lastChecked, now: now)]
        parts.insert(machine.status?.hostname ?? machine.name, at: 0)
        return parts.joined(separator: " · ")
    }

    // MARK: - Power

    private var powerSection: some View {
        PanelSection(title: "Power") {
            HStack(spacing: 8) {
                if machine.commandableSystem != nil {
                    Button("Sleep") { machine.requestSleep() }
                    ForEach(machine.bootTargets) { target in
                        Button("Boot into \(target.name)") { machine.requestBoot(into: target) }
                    }
                } else if !machine.isAwake, machine.canWake {
                    Button(machine.isWaking ? "Waking" : "Wake") { machine.wake() }
                        .disabled(machine.isWaking || machine.wakePlan.isEmpty)
                }
            }
            .controlSize(.small)
            .disabled(machine.isWorking)

            if let note = powerNote {
                PanelNote(text: note)
            }
        }
    }

    /// Only when there is nothing to press: a reboot in flight, a machine that is up without its
    /// agent, or one that is down and cannot be woken. A button explains itself; a note under a
    /// button is words for the sake of words.
    private var powerNote: String? {
        if let reboot = machine.rebootInProgress, !machine.isAwake {
            return "Back in \(reboot.target.name) in about a minute."
        }
        if machine.isAwake, machine.commandableSystem == nil {
            return "The control agent is not installed."
        }
        if !machine.isAwake, !machine.canWake {
            return "No wake address configured, so there is nothing to send."
        }
        if !machine.isAwake, machine.canWake {
            let plan = machine.wakePlan
            // A disabled button with no explanation is the least useful thing this app could say
            // about a machine in another building.
            if plan.isEmpty { return plan.nothingToTry }
            if case .helper(_, let name)? = plan.steps.first { return "Through \(name), from here." }
        }
        return nil
    }
}

// MARK: - One service on a machine

private struct ServiceBlock: View {
    let machine: MachineModel
    let service: ServiceStatus
    let showsAutoUpdate: Bool

    var body: some View {
        PanelSection(title: title) {
            PanelGrid {
                GridRow {
                    PanelKey("Installed")
                    PanelVersion(service.installed, placeholder: "not installed")
                }
                if service.canUpdate != false {
                    GridRow {
                        PanelKey("Latest")
                        versionVerdict
                    }
                } else {
                    GridRow {
                        PanelKey("Updates")
                        Text("Managed by application").foregroundStyle(.secondary)
                    }
                }
                GridRow {
                    PanelKey("Service")
                    serverVerdict
                }
                if service.canUpdate != false {
                    GridRow {
                        PanelKey("Doing now")
                        busyVerdict
                    }
                }
            }

            FlowRow(spacing: 8) {
                if service.canUpdate != false { PrimaryActionButton(
                    title: "Update now",
                    isHighlighted: service.canBeUpdated,
                    isEnabled: !machine.isWorking && service.canBeUpdated
                ) { machine.requestUpdate(service) }
                    .help(service.updateUnavailableReason ?? "Installs the newer build and starts it again. Asking here ignores the schedule.")

                }
                Button("Restart") { machine.requestRestart(service) }
                    .disabled(machine.isWorking || service.canRestart == false)
                    .help("Stops and starts \(service.displayName). Held back while work is running.")

                if machine.dialect.supportsQueue, service.canUpdate != false {
                    Button("When idle") { machine.requestUpdateWhenIdle(service) }
                        .disabled(machine.isWorking || !service.canBeUpdated)
                        .help("Queue the update and let the machine run it the next time nothing is going on.")
                }
            }
            .controlSize(.small)
            .padding(.top, 2)

            if showsAutoUpdate, service.canUpdate != false {
                AutoUpdateRow(machine: machine)
                    .padding(.top, 2)
            }
        }
    }

    private var title: String {
        guard let system = machine.currentSystem else { return service.displayName }
        return "\(service.displayName) on \(system.name)"
    }

    /// Three answers, not two. "Not busy" used to cover idle, unmonitored and probe-failed alike,
    /// and only the first of those is a reason to go ahead with something disruptive.
    @ViewBuilder
    private var busyVerdict: some View {
        if let busy = service.busy {
            switch busy.verdict {
            case .busy: PanelVerdict(symbol: "circle.dotted", text: busy.summary, tint: .orange)
            case .idle: PanelVerdict(symbol: "pause.circle", text: "idle")
            case .unknown: PanelVerdict(symbol: "questionmark.circle", text: "busy state unknown", tint: .orange)
            case .unmonitored: PanelVerdict(symbol: "eye.slash", text: "not monitored", tint: .orange)
            }
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var versionVerdict: some View {
        if service.installed == nil {
            PanelVerdict(symbol: "questionmark.circle", text: "not installed here")
        } else if let staged = service.stagedVersion {
            PanelVerdict(symbol: "arrow.down.circle", text: staged, tint: .orange, monospaced: true)
                .help("Downloaded and waiting for a quit.")
        } else if service.latest == nil {
            PanelVerdict(symbol: "wifi.exclamationmark", text: "not checked", tint: .orange)
        } else if service.upToDate == true {
            PanelVerdict(symbol: "checkmark.circle", text: "up to date", tint: .green)
        } else if service.pendingRestart == true {
            PanelVerdict(symbol: "clock.arrow.circlepath", text: service.latest ?? "newer build", tint: .orange, monospaced: true)
                .help("Queued for the next idle window.")
        } else {
            PanelVerdict(symbol: "arrow.down.circle", text: service.latest ?? "newer build", tint: .orange, monospaced: true)
                .help("A newer build is available.")
        }
    }

    @ViewBuilder
    private var serverVerdict: some View {
        if service.canUpdate == false {
            Text(service.running.map { $0 ? "Running" : "Stopped" } ?? "Not known")
                .foregroundStyle(.secondary)
        } else if service.healthy == true {
            PanelVerdict(symbol: "checkmark.circle",
                         text: service.port.map { "healthy on port \($0)" } ?? "healthy",
                         tint: .green)
        } else if service.running == true {
            PanelVerdict(symbol: "exclamationmark.triangle",
                         text: service.port.map { "running, not answering on \($0)" } ?? "running, not answering",
                         tint: .orange)
        } else if service.running == false {
            PanelVerdict(symbol: "xmark.circle", text: "stopped", tint: .red)
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }
}

/// One switch per system on the machine. Only the system that is awake can be changed; the others
/// show what they said the last time they were up, and the tool tip says when that was.
private struct AutoUpdateRow: View {
    let machine: MachineModel

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            PanelKey("Auto-update")
                .fixedSize()
            Spacer(minLength: 0)
            ForEach(machine.machine.systems) { system in
                toggle(for: system)
            }
        }
    }

    private func toggle(for system: SystemConfig) -> some View {
        let isLive = machine.currentSystem?.id == system.id
        let value = machine.autoUpdateValue(for: system)
        return Toggle(isOn: Binding(
            get: { value ?? false },
            set: { machine.setAutoUpdate($0, on: system) }
        )) {
            Text(system.name)
                .font(.callout)
                .foregroundStyle(isLive ? AnyShapeStyle(.primary) : AnyShapeStyle(.secondary))
        }
        .toggleStyle(.switch)
        .controlSize(.mini)
        .fixedSize()
        .disabled(!isLive || machine.isWorking)
        .help(help(for: system, isLive: isLive, value: value))
    }

    private func help(for system: SystemConfig, isLive: Bool, value: Bool?) -> String {
        if isLive {
            return value == true
                ? "\(system.name) installs updates on its own."
                : "\(system.name) only updates when you ask."
        }
        guard let value, let checked = machine.autoUpdateCheckedAt(for: system) else {
            return "Not known yet. Boot \(system.name) once to read it."
        }
        return "Last known: \(value ? "on" : "off"), read \(checked.formatted(date: .abbreviated, time: .shortened)). Boot \(system.name) to change it."
    }
}

// MARK: - This Mac

private struct MacBlock: View {
    let model: AppModel
    let mac: MacModel
    /// The refresh control lands here only when there is no machine above to carry it.
    let showsRefresh: Bool

    var body: some View {
        if showsRefresh {
            HStack {
                Text(mac.name).font(.headline)
                Spacer(minLength: 8)
                if model.isRefreshing {
                    ProgressView().controlSize(.small).frame(width: 24, height: 24)
                } else {
                    Button {
                        Task { await model.refreshEverything(userInitiated: false) }
                    } label: {
                        Image(systemName: "arrow.clockwise").frame(width: 24, height: 24)
                    }
                    .buttonStyle(.accessoryBar)
                    .disabled(model.isWorking)
                    .help("Read everything again")
                }
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 12)
            PanelRule()
        }

        if let failure = mac.failure {
            PanelSection(title: mac.name) {
                Text(failure.localMessage)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            PanelRule()
        } else if mac.services.isEmpty {
            PanelSection(title: mac.name) {
                Text("Reading \(mac.name).")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            PanelRule()
        } else {
            ForEach(Array(mac.services.enumerated()), id: \.element.id) { index, service in
                block(service, showsToggle: service.id == mac.services.first(where: { $0.canUpdate != false })?.id)
                PanelRule()
            }
        }
    }

    private func block(_ service: ServiceStatus, showsToggle: Bool) -> some View {
        PanelSection(title: "\(service.displayName) on \(mac.name)") {
            PanelGrid {
                GridRow {
                    PanelKey("Installed")
                    PanelVersion(service.installed, placeholder: "not installed")
                }
                if service.canUpdate != false {
                    GridRow {
                        PanelKey("Waiting")
                        stagedVerdict(service)
                    }
                } else {
                    GridRow {
                        PanelKey("Process")
                        Text(service.running.map { $0 ? "Running" : "Stopped" } ?? "Not known")
                            .foregroundStyle(.secondary)
                    }
                    GridRow {
                        PanelKey("Updates")
                        Text("Managed by application").foregroundStyle(.secondary)
                    }
                }
                if service.canUpdate != false {
                GridRow {
                    PanelKey("Doing now")
                    if let reason = service.busyReason {
                        PanelVerdict(symbol: "circle.dotted", text: reason, tint: .orange)
                    } else {
                        PanelVerdict(symbol: "pause.circle", text: "Idle")
                    }
                }
                }
            }

            FlowRow(spacing: 8) {
                if service.canUpdate != false { PrimaryActionButton(
                    title: "Update now",
                    isHighlighted: mac.updateUnavailableReason(service) == nil,
                    isEnabled: !mac.isWorking && mac.updateUnavailableReason(service) == nil
                ) { mac.requestUpdate(service) }
                    .help(mac.updateUnavailableReason(service)
                        ?? "Quits \(service.displayName) on this device and starts it again on the new build. Held back while work is running.")

                }
                // The parity the local side used to be missing entirely.
                Button("Restart") { mac.requestRestart(service) }
                    .disabled(mac.isWorking || mac.restartUnavailableReason(service) != nil)
                    .help(mac.restartUnavailableReason(service) ?? "Stops and starts \(service.displayName).")

                if mac.dialect.supportsQueue, service.canUpdate != false {
                    Button("When idle") { mac.requestUpdateWhenIdle(service) }
                        .disabled(mac.isWorking || mac.updateUnavailableReason(service) != nil)
                }

                if mac.isWorking {
                    ProgressView().controlSize(.mini)
                }
            }
            .controlSize(.small)
            .padding(.top, 2)

            HStack(alignment: .center, spacing: 14) {
                Spacer(minLength: 0)

                if showsToggle, service.canUpdate != false {
                    Toggle(isOn: Binding(
                        get: { mac.autoUpdate ?? false },
                        set: { mac.setAutoUpdate($0) }
                    )) {
                        Text("Apply on its own")
                            .font(.callout)
                    }
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .fixedSize()
                    .disabled(!mac.isReachable || mac.isWorking)
                    .help("Apply a waiting build on the schedule, once nothing is running. Asking yourself always works.")
                }
            }
            .padding(.top, 2)

            if showsToggle, let note = mac.note, !note.isEmpty {
                PanelNote(text: note, isError: mac.noteIsError)
            }
        }
    }

    @ViewBuilder
    private func stagedVerdict(_ service: ServiceStatus) -> some View {
        if let staged = service.stagedVersion {
            PanelVerdict(symbol: "arrow.down.circle", text: staged, tint: .orange, monospaced: true)
                .help("Downloaded and waiting for a quit.")
        } else if service.upToDate == false {
            PanelVerdict(symbol: "arrow.down.circle", text: service.latest ?? "a newer build", tint: .orange, monospaced: true)
                .help("A newer build exists and has not been fetched yet.")
        } else if service.latest == nil {
            PanelVerdict(symbol: "wifi.exclamationmark", text: "not checked", tint: .orange)
        } else {
            PanelVerdict(symbol: "checkmark.circle", text: "nothing", tint: .green)
        }
    }
}

// MARK: - The question

/// A confirmation drawn inside the panel instead of in a modal alert. The menu used to raise an
/// application modal panel for these, which meant a second window popping up in the middle of the
/// screen for a decision that was made two centimetres below the menu bar.
private struct Question: View {
    let dialog: PendingDialog
    let model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(dialog.title)
                .font(.headline)
            Text(dialog.message)
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 8) {
                Spacer(minLength: 0)
                Button("Cancel") { model.dialog = nil }
                    .keyboardShortcut(.cancelAction)
                // Neither interrupt the work nor give up: wait for the machine to be idle. The
                // question used to have only the two extremes.
                if dialog.hasAlternative {
                    Button(dialog.alternativeTitle ?? "When idle") { model.chooseAlternative(dialog) }
                }
                Button(dialog.confirmTitle) { model.confirm(dialog) }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
            }
            .controlSize(.small)
            .padding(.top, 4)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
    }
}

// MARK: - Building blocks

/// A small uppercase label over its content, the way a printed reference sheet groups its lines.
private struct PanelSection<Content: View>: View {
    var title: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title.uppercased())
                .font(.caption2.weight(.semibold))
                .tracking(0.6)
                .foregroundStyle(.secondary)
            content
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 11)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// The hairline between sections. Drawn full width, so it reads as a rule and not as a divider
/// inside a box.
private struct PanelRule: View {
    var body: some View {
        Divider()
    }
}

/// Label and value columns with a shared left edge for the values.
private struct PanelGrid<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 12, verticalSpacing: 5) {
            content
        }
        .font(.callout)
    }
}

private struct PanelKey: View {
    var text: String

    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(.callout)
            .foregroundStyle(.secondary)
            .gridColumnAlignment(.leading)
    }
}

/// A version string in a fixed-width face so it does not jitter between readings, cut in the
/// middle when it has to be cut, since the date at the end is the part that tells builds apart.
private struct PanelVersion: View {
    var value: String?
    var placeholder: String

    init(_ value: String?, placeholder: String) {
        self.value = value
        self.placeholder = placeholder
    }

    var body: some View {
        Text(value ?? placeholder)
            .font(.system(.callout, design: .monospaced))
            .foregroundStyle(value == nil ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
            .lineLimit(1)
            .truncationMode(.middle)
            .textSelection(.enabled)
    }
}

/// A symbol and a few words. Never a badge.
private struct PanelVerdict: View {
    var symbol: String
    var text: String
    var tint: Color = .secondary
    /// For version strings: the fixed-width face, and cut in the middle when it has to be cut,
    /// since the date at the end is the part that tells builds apart.
    var monospaced = false

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .imageScale(.small)
            Text(text)
                .font(monospaced ? .system(.callout, design: .monospaced) : .callout)
                .lineLimit(1)
                .truncationMode(monospaced ? .middle : .tail)
        }
    }
}

/// The quiet line under a row of controls. An aside, not a warning, unless it really is one.
private struct PanelNote: View {
    var text: String
    var isError = false

    var body: some View {
        Text(text)
            .font(.caption)
            .foregroundStyle(isError ? AnyShapeStyle(.orange) : AnyShapeStyle(.tertiary))
            .fixedSize(horizontal: false, vertical: true)
            .contentTransition(.opacity)
            .animation(.easeOut(duration: 0.15), value: text)
    }
}


/// Operations this app started and never learned the outcome of.
///
/// At the top of the panel and above everything else, because it is the one thing here that asks for
/// attention rather than reporting state: an update that may or may not have installed is a question
/// somebody has to settle, and it used to vanish without trace.
private struct UnknownOutcomes: View {
    let model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Outcome not known")
                .font(.caption2.weight(.semibold))
                .tracking(0.6)
                .foregroundStyle(.secondary)

            ForEach(model.unresolvedOperations.prefix(3)) { record in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Image(systemName: "questionmark.circle")
                        .foregroundStyle(.orange)
                        .imageScale(.small)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(record.title).font(.callout)
                        Text(record.summary)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer(minLength: 8)
                    Button("Check") { model.machine(id: record.machineId)?.reconcileNow() }
                        .buttonStyle(.accessoryBar)
                        .font(.callout)
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
