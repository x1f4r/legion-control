import SwiftUI

struct ControlSheet<Content: View>: View {
    var title: String
    @ViewBuilder var content: Content
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text(title).font(.headline)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }.padding(16)
            Divider()
            ScrollView {
                content.frame(maxWidth: .infinity, alignment: .leading).padding(16)
            }
        }
        .frame(minWidth: 360, idealWidth: 600, maxWidth: 720, minHeight: 300, idealHeight: 520, maxHeight: 700)
    }
}

struct CompactServiceRow<Actions: View>: View {
    let service: ServiceStatus
    var updateEnabled: Bool
    var update: () -> Void
    var details: () -> Void
    @ViewBuilder var actions: Actions

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 12) {
                name.frame(minWidth: 140, maxWidth: .infinity, alignment: .leading)
                Text(state).foregroundStyle(.secondary).frame(width: 104, alignment: .leading)
                version.frame(width: 144, alignment: .leading)
                controls.frame(width: 112, alignment: .trailing)
            }
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 3) {
                    name
                    Text(state).font(.caption).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, alignment: .leading)
                controls
            }
        }
        .padding(.vertical, 11)
        .overlay(alignment: .bottom) { Divider() }
    }

    private var name: some View {
        Button(action: details) { Text(service.displayName).fontWeight(.medium).lineLimit(1) }
            .buttonStyle(.plain).help("Details for \(service.displayName)")
    }

    private var version: some View {
        Text(service.installed ?? "Version unknown")
            .font(.callout.monospaced()).foregroundStyle(.secondary)
            .lineLimit(1).truncationMode(.middle)
            .help(service.installed ?? "Version unknown")
    }

    private var controls: some View {
        HStack(spacing: 8) {
            if service.canUpdate != false, service.canBeUpdated {
                Button("Update", action: update).disabled(!updateEnabled)
                    .buttonStyle(.borderedProminent)
                    .accessibilityLabel("Update \(service.displayName)")
            }
            Menu {
                actions
                Divider()
                Button("Details…", action: details)
            } label: {
                Image(systemName: "ellipsis").frame(width: 18)
            }
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel("Actions for \(service.displayName)")
            .help("Actions for \(service.displayName)")
        }
    }

    private var state: String { CompactServicePresentation.state(for: service) }

}

extension Notification.Name {
    static let legionNavigate = Notification.Name("LegionControl.navigate")
}

enum CompactServicePresentation {
    static func state(for service: ServiceStatus) -> String {
        if let busy = service.busy {
            if busy.isUnknown { return "Activity unknown" }
            if !busy.isMonitored { return "Not monitored" }
            if busy.isBusy { return "Working" }
        }
        if service.stagedVersion != nil { return "Ready to apply" }
        if service.pendingVersion != nil || service.pendingRestart == true { return "Queued" }
        if service.health?.ok == false || service.healthy == false { return "Needs attention" }
        if service.canUpdate != false, service.upToDate == false { return "Update available" }
        if service.process?.running == false || service.running == false { return "Stopped" }
        if service.process?.running == true || service.running == true { return "Running" }
        return "Not checked"
    }
}

@MainActor
final class SheetActionRelay {
    private var action: (@MainActor () -> Void)?
    func queue(_ action: @escaping @MainActor () -> Void) { self.action = action }
    func didDismiss() {
        let pending = action
        action = nil
        pending?()
    }
}
