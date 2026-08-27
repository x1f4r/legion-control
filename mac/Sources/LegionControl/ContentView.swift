import SwiftUI

/// What the sidebar can be pointing at. One row per machine, one per service that machine reports,
/// and one for the device the app runs on when the config asks for it.
enum SidebarItem: Hashable, Identifiable {
    case machine(String)
    case service(machine: String, service: String)
    case local

    var id: String {
        switch self {
        case .machine(let id): "machine:\(id)"
        case .service(let machine, let service): "service:\(machine)/\(service)"
        case .local: "local"
        }
    }

    /// Round trips through @AppStorage, so the window opens where it was left.
    init?(id: String) {
        if id == "local" { self = .local; return }
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
    @AppStorage("selectedSection") private var storedSection = ""

    @State private var columnVisibility: NavigationSplitViewVisibility = .all

    /// Every row there is, in the order they are drawn.
    private var items: [SidebarItem] {
        var items: [SidebarItem] = []
        for machine in model.machines {
            items.append(.machine(machine.id))
            for service in machine.services {
                items.append(.service(machine: machine.id, service: service.id))
            }
        }
        if model.mac != nil { items.append(.local) }
        return items
    }

    /// The stored row when it still exists, and the first one otherwise. A machine that has gone
    /// from the config, or a service that has gone from a status, must not leave the detail pane
    /// pointing at nothing.
    private var section: SidebarItem? {
        let all = items
        if let stored = SidebarItem(id: storedSection), all.contains(stored) { return stored }
        return all.first
    }

    private var selection: Binding<SidebarItem?> {
        Binding(
            get: { section },
            set: { newValue in
                guard let newValue, newValue != section else { return }
                withAnimation(.easeOut(duration: 0.15)) { storedSection = newValue.id }
            }
        )
    }

    var body: some View {
        Group {
            if model.isConfigured {
                configured
            } else {
                SetupView(model: model)
            }
        }
        .alert(
            Text(model.dialog?.title ?? ""),
            isPresented: Binding(get: { model.dialog != nil }, set: { if !$0 { model.dialog = nil } }),
            presenting: model.dialog
        ) { dialog in
            Button(dialog.confirmTitle, role: .destructive) { model.confirm(dialog) }
            Button("Cancel", role: .cancel) { }
        } message: { dialog in
            Text(dialog.message)
        }
    }

    private var configured: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            Sidebar(model: model, items: items, selection: selection)
                .navigationSplitViewColumnWidth(min: 176, ideal: 196, max: 260)
        } detail: {
            detail
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            StatusBar(model: model)
        }
    }

    /// One section at a time, so a poll only ever redraws what is actually on screen. The identity
    /// changes with the selection, which is what turns the swap into a crossfade rather than a jump.
    private var detail: some View {
        ScrollView {
            sectionBody
                .padding(.horizontal, 32)
                .padding(.top, 30)
                .padding(.bottom, 30)
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
                MachineSection(model: machine)
            }
        case .service(let machineId, let serviceId):
            if let machine = model.machine(id: machineId) {
                ServiceSection(model: machine, serviceId: serviceId)
            }
        case .local:
            if let mac = model.mac {
                MacSection(model: model, mac: mac)
            }
        case nil:
            EmptyView()
        }
    }
}

// MARK: - Sidebar

/// A row per machine and per service, each with a live second line. The second line is the whole
/// point: it means the sidebar answers the easy questions on its own, and you only go into a
/// section when you want the detail behind an answer you have already read.
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
        }
    }

    private func row(_ item: SidebarItem, symbol: String, title: String, summary: String) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Image(systemName: symbol)
                .imageScale(.medium)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    // The line changes under the pointer every fifteen seconds. A fade reads as an
                    // update; a hard swap reads as a flicker.
                    .contentTransition(.opacity)
                    .animation(.easeOut(duration: 0.2), value: summary)
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

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Divider()
            HStack(alignment: .center, spacing: 10) {
                if model.isWorking {
                    ProgressView()
                        .controlSize(.small)
                        .padding(.horizontal, 2)
                } else {
                    Image(systemName: model.statusIsError ? "exclamationmark.triangle" : "info.circle")
                        .foregroundStyle(model.statusIsError ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                }

                VStack(alignment: .leading, spacing: 3) {
                    Text(model.statusLine)
                        .textSelection(.enabled)
                        .contentTransition(.opacity)
                        .animation(.easeOut(duration: 0.15), value: model.statusLine)
                    if let detail = model.statusDetail, !detail.isEmpty {
                        Text(detail)
                            .font(.system(.caption, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                            .lineLimit(4)
                    }
                }

                Spacer(minLength: 12)

                Button("Refresh") {
                    Task { await model.refreshEverything(userInitiated: true) }
                }
                .keyboardShortcut("r", modifiers: .command)
                .disabled(model.isWorking || model.isRefreshing)
            }
            .font(.callout)
            .padding(.horizontal, 20)
            .padding(.vertical, 10)
        }
        .background(.background)
    }
}
