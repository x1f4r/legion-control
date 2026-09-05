import AppKit
import SwiftUI

@MainActor
@Observable
final class MenuBarPanelState {
    var startsAtLogin = LoginItem.isEnabled
    var needsLoginApproval = LoginItem.needsApproval
    var loginProblem: String?
    var isWindowVisible = false
    var maximumHeight: CGFloat = 560

    @ObservationIgnored var openWindow: @MainActor () -> Void = {}
    @ObservationIgnored var openSection: @MainActor (String) -> Void = { _ in }
    @ObservationIgnored var quit: @MainActor () -> Void = {}

    func rereadLogin() {
        startsAtLogin = LoginItem.isEnabled
        needsLoginApproval = LoginItem.needsApproval
    }

    func setStartsAtLogin(_ wanted: Bool) {
        loginProblem = LoginItem.set(wanted)
        rereadLogin()
    }
}

/// Quick device controls; detailed service management stays in the window.
struct MenuBarPanel: View {
    let model: AppModel
    let panel: MenuBarPanelState
    @State private var showsResult = false
    @State private var showsUpdate = false

    private var rowCount: Int { model.machines.count + (model.mac == nil ? 0 : 1) }
    private var noticeHeight: CGFloat {
        (model.appUpdates.availableVersion == nil ? 0 : 38)
        + (model.unresolvedOperations.isEmpty ? 0 : 34)
        + (model.lastActionOutcome?.isError == true ? 44 : 0)
    }
    private var listHeight: CGFloat {
        min(CGFloat(max(rowCount, 1)) * 54, max(0, panel.maximumHeight - 90 - noticeHeight))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if let dialog = model.dialog {
                PanelQuestion(dialog: dialog, model: model, maximumHeight: panel.maximumHeight - 90)
            } else {
                ScrollView {
                    VStack(spacing: 0) {
                        if model.isConfigured {
                            ForEach(model.machines) { machine in
                                RemoteDeviceRow(machine: machine, panel: panel)
                            }
                            if let mac = model.mac { LocalDeviceRow(mac: mac, panel: panel) }
                        } else {
                            Button("Set up a device…") { panel.openSection("setup") }
                                .frame(maxWidth: .infinity, minHeight: 54, alignment: .leading)
                                .padding(.horizontal, 12)
                                .buttonStyle(.plain)
                        }
                    }
                }
                .frame(height: listHeight)
                notices
            }
            Divider()
            footer
        }
        .font(.system(size: 13))
        .controlSize(.small)
        .frame(width: 320)
    }

    private var header: some View {
        HStack {
            Text("Legion Control").fontWeight(.semibold)
            Spacer()
            Button {
                Task { await model.refreshEverything(userInitiated: false) }
            } label: {
                if model.isRefreshing { ProgressView().controlSize(.mini).frame(width: 22, height: 22) }
                else { Image(systemName: "arrow.clockwise").frame(width: 22, height: 22) }
            }
            .buttonStyle(.borderless)
            .disabled(model.isRefreshing || model.isWorking || model.dialog != nil)
            .help("Refresh devices")
            .accessibilityLabel("Refresh devices")
        }
        .padding(.horizontal, 12)
        .frame(height: 42)
    }

    @ViewBuilder private var notices: some View {
        if !model.unresolvedOperations.isEmpty {
            Menu {
                ForEach(model.unresolvedOperations) { record in
                    Menu(record.title) {
                        Text(record.summary)
                        Button("Open device") { openOperation(record) }
                        Button("Check outcome") {
                            if let machine = model.machine(id: record.machineId) { machine.reconcileNow() }
                            else if let mac = model.mac, mac.historyMachineId == record.machineId { mac.reconcileNow() }
                        }
                    }
                }
            } label: {
                Label("Unresolved operations (\(model.unresolvedOperations.count))", systemImage: "questionmark.circle")
                    .foregroundStyle(.orange)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .menuStyle(.borderlessButton)
            .padding(.horizontal, 12)
            .frame(height: 34)
        }
        if let outcome = model.lastActionOutcome, outcome.isError {
            HStack(spacing: 6) {
                Image(systemName: "exclamationmark.triangle").foregroundStyle(.orange)
                Text(outcome.text).font(.caption).lineLimit(2)
                Spacer(minLength: 0)
                Button("Details") { showsResult = true }
                    .popover(isPresented: $showsResult) {
                        ScrollView { Text(outcome.text).textSelection(.enabled).padding(14) }
                            .frame(width: 300, height: 180)
                    }
            }
            .padding(.horizontal, 12)
            .frame(height: 44)
        }
        if let version = model.appUpdates.availableVersion {
            HStack {
                Text("Update \(version) available").lineLimit(1)
                Spacer(minLength: 4)
                Button("Review") { showsUpdate = true }
                    .popover(isPresented: $showsUpdate) {
                        VStack(alignment: .leading, spacing: 12) {
                            Text("Legion Control \(version)").font(.headline)
                            if let text = model.appUpdates.note {
                                ScrollView { Text(text).textSelection(.enabled) }.frame(maxHeight: 180)
                            }
                            if let reason = model.appUpdates.installBlockedReason {
                                Text(reason).font(.caption).foregroundStyle(.orange)
                            }
                            Button(model.appUpdates.phase.isWorking ? model.appUpdates.summary : "Install and restart") {
                                model.appUpdates.install()
                            }
                            .disabled(model.appUpdates.phase.isWorking || model.appUpdates.installBlockedReason != nil)
                        }.padding(14).frame(width: 292)
                    }
            }
            .padding(.horizontal, 12)
            .frame(height: 38)
        }
    }

    private func openOperation(_ record: OperationRecord) {
        panel.openSection(model.mac?.historyMachineId == record.machineId ? "local" : "machine:\(record.machineId)")
    }

    private var footer: some View {
        HStack {
            Button("Open app") { panel.openWindow() }
                .buttonStyle(.borderless)
            Spacer()
            Menu {
                Button("Settings…") { panel.openSection("setup") }
                Toggle("Start at login", isOn: Binding(get: { panel.startsAtLogin }, set: { panel.setStartsAtLogin($0) }))
                if let problem = panel.loginProblem { Text(problem) }
                else if panel.needsLoginApproval { Text("Approve in System Settings → Login Items") }
                Button("Check for updates") { model.appUpdates.checkNow() }
                    .disabled(model.appUpdates.phase.isWorking)
                Divider()
                Button("Quit Legion Control") { panel.quit() }.keyboardShortcut("q")
            } label: {
                Image(systemName: "ellipsis").frame(width: 24, height: 24)
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .help("App settings and actions")
            .accessibilityLabel("App settings and actions")
        }
        .padding(.horizontal, 12)
        .frame(height: 44)
    }
}

private struct RemoteDeviceRow: View {
    let machine: MachineModel
    let panel: MenuBarPanelState

    private var status: String {
        if machine.isWorking { return machine.isWaking ? "Waking…" : "Working…" }
        if machine.rebootInProgress != nil, !machine.isAwake { return "Restarting…" }
        switch machine.link {
        case .online(let system):
            if let busy = machine.status?.busy {
                if busy.isUnknown { return "Busy state unknown" }
                if !busy.isMonitored { return "Not monitored" }
                if busy.isBusy { return "Busy" }
            }
            return system.name
        case .reachableWithoutAgent: return "Agent unavailable"
        case .offline: return "Offline"
        case .unknown: return "Not checked"
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            Button { open() } label: {
                HStack(spacing: 8) {
                    Image(systemName: machine.symbolName).foregroundStyle(.secondary).frame(width: 20)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(machine.name).lineLimit(1)
                        Text(status).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer(minLength: 0)
                }
            }.buttonStyle(.plain)
            if machine.commandableSystem != nil {
                Button { machine.requestSleep() } label: { Image(systemName: "moon").frame(width: 24, height: 24) }
                    .help("Sleep \(machine.name)").accessibilityLabel("Sleep \(machine.name)")
                    .disabled(machine.isWorking)
            } else if !machine.isAwake, machine.canWake {
                Button { machine.wake() } label: { Image(systemName: "power").frame(width: 24, height: 24) }
                    .help(machine.wakePlan.isEmpty ? machine.wakePlan.nothingToTry : "Wake \(machine.name)")
                    .accessibilityLabel("Wake \(machine.name)")
                    .disabled(machine.isWorking || machine.wakePlan.isEmpty)
            }
            Menu { actions } label: { Image(systemName: "ellipsis").frame(width: 20, height: 24) }
                .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                .help("Actions for \(machine.name)").accessibilityLabel("Actions for \(machine.name)")
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, 12)
        .frame(height: 54)
        .contentShape(Rectangle())
        .contextMenu { actions }
    }

    private func open() { panel.openSection("machine:\(machine.id)") }

    @ViewBuilder private var actions: some View {
        Button("Open \(machine.name)") { open() }
        Divider()
        if machine.commandableSystem != nil {
            Button("Sleep") { machine.requestSleep() }.disabled(machine.isWorking)
            ForEach(machine.bootTargets) { target in
                Button("Restart into \(target.name)…") { machine.requestBoot(into: target) }.disabled(machine.isWorking)
            }
        } else if !machine.isAwake, machine.canWake {
            Button("Wake") { machine.wake() }.disabled(machine.isWorking || machine.wakePlan.isEmpty)
            if machine.wakePlan.isEmpty { Text(machine.wakePlan.nothingToTry) }
        }
        if !machine.actions.isEmpty {
            Divider()
            ForEach(machine.actions) { action in
                Button(action.displayName) { machine.requestAction(action) }
                    .disabled(machine.isWorking || machine.commandableSystem == nil)
            }
        }
    }
}

private struct LocalDeviceRow: View {
    let mac: MacModel
    let panel: MenuBarPanelState

    private var status: String {
        if mac.isWorking { return "Working…" }
        if mac.failure != nil { return "Agent unavailable" }
        if !mac.isReachable { return "Not checked" }
        if let busy = mac.status?.busy {
            if busy.isUnknown { return "Busy state unknown" }
            if !busy.isMonitored { return "Not monitored" }
            if busy.isBusy { return "Busy" }
        }
        return "This device"
    }

    var body: some View {
        HStack(spacing: 8) {
            Button { panel.openSection("local") } label: {
                HStack(spacing: 8) {
                    Image(systemName: "laptopcomputer").foregroundStyle(.secondary).frame(width: 20)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(mac.name).lineLimit(1)
                        Text(status).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer(minLength: 0)
                }
            }.buttonStyle(.plain)
            Button { mac.requestSleep() } label: { Image(systemName: "moon").frame(width: 24, height: 24) }
                .disabled(mac.isWorking || !mac.isReachable)
                .help("Sleep \(mac.name)").accessibilityLabel("Sleep \(mac.name)")
            Menu { actions } label: { Image(systemName: "ellipsis").frame(width: 20, height: 24) }
                .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                .help("Actions for \(mac.name)").accessibilityLabel("Actions for \(mac.name)")
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, 12)
        .frame(height: 54)
        .contentShape(Rectangle())
        .contextMenu { actions }
    }

    @ViewBuilder private var actions: some View {
        Button("Open \(mac.name)") { panel.openSection("local") }
        Divider()
        Button("Sleep") { mac.requestSleep() }.disabled(mac.isWorking || !mac.isReachable)
        ForEach(mac.bootTargets) { target in
            Button("Restart into \(target.name)…") { mac.requestBoot(into: target) }.disabled(mac.isWorking || !mac.isReachable)
        }
        if !mac.actions.isEmpty {
            Divider()
            ForEach(mac.actions) { action in
                Button(action.displayName) { mac.requestAction(action) }.disabled(mac.isWorking || !mac.isReachable)
            }
        }
    }
}

private struct PanelQuestion: View {
    let dialog: PendingDialog
    let model: AppModel
    let maximumHeight: CGFloat

    private var messageHeight: CGFloat {
        let text = NSAttributedString(string: dialog.message, attributes: [.font: NSFont.systemFont(ofSize: 13)])
        let measured = text.boundingRect(with: NSSize(width: 296, height: CGFloat.greatestFiniteMagnitude),
                                        options: [.usesLineFragmentOrigin, .usesFontLeading]).height
        return min(ceil(measured) + 4, max(60, maximumHeight - 120))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(dialog.title).font(.headline)
            ScrollView {
                Text(dialog.message).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            }.frame(height: messageHeight)
            FlowRow(spacing: 8) {
                Button("Cancel") { model.dialog = nil }.keyboardShortcut(.cancelAction)
                if dialog.hasAlternative {
                    Button(dialog.alternativeTitle ?? "When idle") { model.chooseAlternative(dialog) }
                }
                Button(dialog.confirmTitle) { model.confirm(dialog) }
                    .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            }
        }.padding(12)
    }
}
