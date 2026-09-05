import SwiftUI

/// Stable navigation identifiers, including service links saved by earlier releases.
enum SidebarItem: Hashable, Identifiable {
    case machine(String)
    case service(machine: String, service: String)
    case local
    case setup

    var id: String {
        switch self {
        case .machine(let id): "machine:\(id)"
        case .service(let machine, let service): "service:\(machine)/\(service)"
        case .local: "local"
        case .setup: "setup"
        }
    }

    /// A stable identifier lets the window reopen where it was left.
    init?(id: String) {
        if id == "local" { self = .local; return }
        if id == "setup" { self = .setup; return }
        if id.hasPrefix("machine:") { self = .machine(String(id.dropFirst(8))); return }
        if id.hasPrefix("service:") {
            let rest = id.dropFirst(8)
            guard let slash = rest.firstIndex(of: "/") else { return nil }
            self = .service(machine: String(rest[rest.startIndex..<slash]),
                            service: String(rest[rest.index(after: slash)...]))
            return
        }
        return nil
    }
}

struct ContentView: View {
    /// Owned by the app, not by the view: the window can be put away and brought back, and the menu
    /// bar reads the same state while it is gone.
    let model: AppModel

    /// Where the window was left. Remembered next to the window frame, and for the same reason: a
    /// utility window that forgets where you were is a window you have to re-navigate every time.
    @State private var storedSection = AppPreferences.string(forKey: "selectedSection") ?? ""

    @State private var columnVisibility: NavigationSplitViewVisibility = .all

    /// Every row there is, in the order they are drawn.
    private var items: [SidebarItem] {
        var items: [SidebarItem] = []
        for machine in model.machines {
            items.append(.machine(machine.id))
        }
        if model.mac != nil { items.append(.local) }
        items.append(.setup)
        return items
    }

    /// The stored row when it still exists, and the first one otherwise. A machine that has gone
    /// from the config, or a service that has gone from a status, must not leave the detail pane
    /// pointing at nothing.
    private var section: SidebarItem? {
        let all = items
        if let stored = SidebarItem(id: storedSection), all.contains(stored) { return stored }
        if case .service(let machine, _) = SidebarItem(id: storedSection), all.contains(.machine(machine)) {
            return .machine(machine)
        }
        return all.first
    }

    private var selection: Binding<SidebarItem?> {
        Binding(
            get: { section },
            set: { newValue in
                guard let newValue, newValue != section else { return }
                withAnimation(.easeOut(duration: 0.15)) { storedSection = newValue.id }
                AppPreferences.set(newValue.id, forKey: "selectedSection")
            }
        )
    }

    var body: some View {
        VStack(spacing: 0) {
            if model.appUpdates.availableVersion != nil {
                AppUpdateNotice(updates: model.appUpdates)
                    .padding(.horizontal, 20).padding(.vertical, 10)
                Divider()
            }
            if model.isConfigured {
                configured
            } else {
                SetupView(model: model)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .legionNavigate)) { notification in
            guard let id = notification.object as? String, SidebarItem(id: id) != nil else { return }
            storedSection = id
            AppPreferences.set(id, forKey: "selectedSection")
        }
        .alert(
            Text(model.dialog?.title ?? ""),
            isPresented: Binding(get: { model.dialog != nil }, set: { if !$0 { model.dialog = nil } }),
            presenting: model.dialog
        ) { dialog in
            Button(dialog.confirmTitle, role: .destructive) { model.confirm(dialog) }
            // The third answer. "Now" and "cancel" were the only two, which meant the choice in
            // front of a busy machine was interrupt the work or give up.
            if dialog.hasAlternative {
                Button(dialog.alternativeTitle ?? "When idle") { model.chooseAlternative(dialog) }
            }
            Button("Cancel", role: .cancel) { }
        } message: { dialog in
            Text(dialog.message)
        }
        .sheet(isPresented: Binding(
            get: { model.divergence != nil },
            set: { if !$0 { model.divergence = nil } }
        )) {
            if let divergence = model.divergence {
                ScrollView {
                    DivergenceView(model: model, divergence: divergence)
                        .padding(.horizontal, 32)
                        .padding(.vertical, 30)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(width: 760, height: 640)
            }
        }
    }

    private var configured: some View {
        GeometryReader { geometry in
            Group {
            if geometry.size.width < 700 {
                VStack(spacing: 0) {
                    HStack {
                        Picker("Device", selection: selection) {
                            ForEach(items) { item in
                                Text(title(for: item)).tag(Optional(item))
                            }
                        }.labelsHidden().frame(maxWidth: 240)
                        Spacer()
                        refreshButton
                    }.padding(.horizontal, 16).padding(.vertical, 8)
                    Divider()
                    detail
                }
            } else {
                NavigationSplitView(columnVisibility: $columnVisibility) {
                    Sidebar(model: model, items: items, selection: selection)
                        .navigationSplitViewColumnWidth(min: 150, ideal: 180, max: 230)
                } detail: { detail }
                .toolbar { ToolbarItem(placement: .primaryAction) { refreshButton } }
            }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if model.isWorking || model.statusIsError || !["Ready.", "Status refreshed."].contains(model.statusLine) {
                    StatusBar(model: model)
                }
            }
        }
    }

    private var refreshButton: some View {
        Button { Task { await model.refreshEverything(userInitiated: true) } } label: {
            Label("Refresh", systemImage: "arrow.clockwise")
        }.disabled(model.isWorking).keyboardShortcut("r", modifiers: .command)
    }

    private func title(for item: SidebarItem) -> String {
        switch item {
        case .machine(let id): model.machine(id: id)?.name ?? id
        case .local: model.mac?.name ?? "This device"
        case .setup: "Setup"
        case .service(_, let id): id
        }
    }

    /// One section at a time, so a poll only ever redraws what is actually on screen. The identity
    /// changes with the selection, which is what turns the swap into a crossfade rather than a jump.
    private var detail: some View {
        ScrollView {
            sectionBody
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .id(section?.id ?? "")
        .transition(.opacity)
    }

    @ViewBuilder
    private var sectionBody: some View {
        switch section {
        case .machine(let id):
            if let machine = model.machine(id: id) {
                MachineSection(app: model, model: machine)
            }
        case .service(let machineId, let serviceId):
            if let machine = model.machine(id: machineId) {
                ServiceSection(model: machine, serviceId: serviceId)
            }
        case .local:
            if let mac = model.mac {
                MacSection(model: model, mac: mac)
            }
        case .setup:
            SetupEditorView(model: model)
        case nil:
            EmptyView()
        }
    }
}

// MARK: - Sidebar

/// Device navigation; service controls live beside their device.
private struct Sidebar: View {
    let model: AppModel
    let items: [SidebarItem]
    @Binding var selection: SidebarItem?

    var body: some View {
        List(selection: $selection) {
            ForEach(items) { item in
                row(item)
            }
        }
        .listStyle(.sidebar)
    }

    @ViewBuilder
    private func row(_ item: SidebarItem) -> some View {
        switch item {
        case .machine(let id):
            if let machine = model.machine(id: id) {
                row(item, symbol: machine.symbolName, title: machine.name, summary: machine.sidebarSummary)
            }
        case .service(let machineId, let serviceId):
            if let machine = model.machine(id: machineId), let service = machine.service(id: serviceId) {
                row(item,
                    symbol: "curlybraces",
                    title: service.displayName,
                    summary: machine.sidebarSummary(for: service))
            }
        case .local:
            if let mac = model.mac {
                row(item, symbol: "laptopcomputer", title: mac.name, summary: mac.sidebarSummary)
            }
        case .setup:
            row(item, symbol: "list.bullet.rectangle", title: "Setup", summary: setupSummary)
        }
    }

    /// The second line of the setup row: which revision this device is on, and whether anything is
    /// waiting to be settled.
    private var setupSummary: String {
        if model.divergence != nil { return "needs a decision" }
        guard let identity = model.config.config?.identity, identity.id != nil else { return "no identity yet" }
        return "revision \(identity.revisionNumber)"
    }

    private func row(_ item: SidebarItem, symbol: String, title: String, summary: String) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Image(systemName: symbol)
                .imageScale(.medium)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 4)
        .tag(item)
    }
}

// MARK: - Status bar

/// The one line across the bottom, plus the one control that belongs to the window rather than to
/// any one section: read everything again.
private struct StatusBar: View {
    let model: AppModel
    @State private var showingDetails = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Divider()
            HStack(alignment: .center, spacing: 10) {
                if model.isWorking {
                    ProgressView()
                        .controlSize(.small)
                        .padding(.horizontal, 2)
                }

                VStack(alignment: .leading, spacing: 3) {
                    Text(model.statusLine)
                        .lineLimit(1)
                        .textSelection(.enabled)
                        .contentTransition(.opacity)
                        .animation(.easeOut(duration: 0.15), value: model.statusLine)
                    if model.statusDetail?.isEmpty == false {
                        Button("Details") { showingDetails = true }.buttonStyle(.link)
                    }
                }

                Spacer(minLength: 12)


            }
            .font(.callout)
            .padding(.horizontal, 20)
            .padding(.vertical, 7)
        }
        .sheet(isPresented: $showingDetails) {
            ControlSheet(title: "Activity") {
                Text(model.statusLine).textSelection(.enabled)
                if let detail = model.statusDetail {
                    Text(detail).font(.callout.monospaced()).textSelection(.enabled)
                }
            }
        }
        .background(.background)
    }
}
