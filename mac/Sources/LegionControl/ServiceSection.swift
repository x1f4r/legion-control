import SwiftUI

/// Everything about one service on one machine: what is installed, whether it is answering, what it
/// is doing, and the two ways to change that. The per system automatic update switches live here
/// too, because they are a property of the software rather than of the machine.
struct ServiceSection: View {
    let model: MachineModel
    let serviceId: String

    private var service: ServiceStatus? { model.service(id: serviceId) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: service?.displayName ?? serviceId,
                        note: model.currentSystem.map { "on \($0.name)" })

            if let service {
                VStack(alignment: .leading, spacing: 12) {
                    DetailRow(label: "Installed") {
                        VersionText(value: service.installed, placeholder: "not installed")
                    }
                    DetailRow(label: latestLabel(service)) {
                        VersionText(value: service.latest, placeholder: "could not be checked")
                    }
                    DetailRow(label: "Version") { versionVerdict(service) }
                    DetailRow(label: "Service") { serverVerdict(service) }
                    DetailRow(label: "Doing now") { activityVerdict(service) }
                    DetailRow(label: "Relay") { relayVerdict(service) }

                    if let threads = service.busy?.threads, !threads.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            // Position, not thread id: the agent may leave the id out, and the whole
                            // list is replaced on every poll anyway.
                            ForEach(Array(threads.enumerated()), id: \.offset) { _, thread in
                                HStack(alignment: .firstTextBaseline, spacing: 8) {
                                    Text(thread.title ?? "untitled")
                                    Text(thread.state ?? "")
                                        .foregroundStyle(.secondary)
                                }
                                .font(.callout)
                            }
                        }
                        .padding(.leading, 158)
                    }

                    if let last = service.lastUpdate {
                        DetailRow(label: "Last update") { lastUpdateText(last) }
                    }
                }
            } else {
                Text("Not available while \(model.name) is unreachable.")
                    .foregroundStyle(.secondary)
            }

            SectionHeading(title: "Actions")

            HStack(spacing: 10) {
                // Prominent only when a newer build has actually been detected. The rest of the
                // time this is a button that would do nothing, so it neither invites a press nor
                // accepts one.
                PrimaryActionButton(
                    title: "Update now",
                    isHighlighted: canUpdate,
                    isEnabled: !model.isWorking && canUpdate
                ) { if let service { model.requestUpdate(service) } }

                Button("Restart \(service?.displayName ?? "it")") {
                    if let service { model.requestRestart(service) }
                }
                .disabled(model.isWorking || model.commandableSystem == nil || service?.canRestart == false)
            }

            QuietNote(text: updateNote)
                .padding(.top, 12)

            SectionHeading(title: "Automatic updates")

            QuietNote(text: "Each system keeps its own setting, for everything it looks after. Only the system that is awake can be changed.")
                .padding(.bottom, 10)

            VStack(alignment: .leading, spacing: 10) {
                ForEach(model.machine.systems) { system in
                    autoUpdateRow(for: system)
                }
            }
        }
    }

    // MARK: - Verdicts

    private var canUpdate: Bool {
        guard model.commandableSystem != nil, let service else { return false }
        return service.canBeUpdated
    }

    /// "Latest" on its own is vague; a service that names its channel can say which one.
    private func latestLabel(_ service: ServiceStatus) -> String {
        guard let channel = service.channel, !channel.isEmpty else { return "Latest" }
        return "Latest \(channel)"
    }

    @ViewBuilder
    private func versionVerdict(_ service: ServiceStatus) -> some View {
        if service.installed == nil {
            StatusText(symbol: "questionmark.circle", text: "Not installed here")
        } else if let staged = service.stagedVersion {
            StatusText(symbol: "arrow.down.circle", text: "\(staged) is downloaded and waiting for a quit", tint: .orange)
        } else if service.latest == nil {
            StatusText(symbol: "wifi.exclamationmark", text: "The latest version could not be checked", tint: .orange)
        } else if service.upToDate == true {
            StatusText(symbol: "checkmark.circle", text: "Up to date", tint: .green)
        } else if service.pendingRestart == true {
            // The agent sets this when it found a newer build and held the install back because
            // something was running. Nothing is installed yet, so this must not say that it is.
            StatusText(symbol: "clock.arrow.circlepath", text: "A newer build is queued for the next idle window", tint: .orange)
        } else {
            StatusText(symbol: "arrow.down.circle", text: "A newer build is available", tint: .orange)
        }
    }

    @ViewBuilder
    private func serverVerdict(_ service: ServiceStatus) -> some View {
        if service.healthy == true {
            if let port = service.port {
                StatusText(symbol: "checkmark.circle", text: "Healthy on port \(port)", tint: .green)
            } else {
                StatusText(symbol: "checkmark.circle", text: "Healthy", tint: .green)
            }
        } else if service.running == true {
            if let port = service.port {
                StatusText(symbol: "exclamationmark.triangle", text: "Running but not answering on port \(port)", tint: .orange)
            } else {
                StatusText(symbol: "exclamationmark.triangle", text: "Running but not answering", tint: .orange)
            }
        } else if service.running == false {
            StatusText(symbol: "xmark.circle", text: "Stopped", tint: .red)
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func activityVerdict(_ service: ServiceStatus) -> some View {
        if let busy = service.busy {
            if busy.isBusy {
                StatusText(symbol: "circle.dotted", text: busy.summary, tint: .orange)
            } else {
                StatusText(symbol: "pause.circle", text: "Idle")
            }
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func relayVerdict(_ service: ServiceStatus) -> some View {
        if let relay = service.relay {
            if relay.configured != true {
                Text("not configured here").foregroundStyle(.secondary)
            } else if relay.running == true {
                StatusText(symbol: "checkmark.circle", text: "Running", tint: .green)
            } else {
                StatusText(symbol: "xmark.circle", text: "Configured but not running", tint: .red)
            }
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    private func lastUpdateText(_ last: LastUpdate) -> some View {
        // Two lines rather than one joined string: the outcome, both version numbers and a raw
        // timestamp in a single monospaced run has nowhere sensible to break and wraps in the middle
        // of a version number.
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(last.result ?? "none recorded")
                if let when = Self.shortTime(last.at) {
                    Text(when).foregroundStyle(.tertiary)
                }
            }
            .font(.callout)
            .foregroundStyle(.secondary)

            if let from = last.from, let to = last.to {
                Text("\(from) to \(to)")
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.tertiary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            } else if let message = last.message, last.result == nil {
                Text(message).font(.callout).foregroundStyle(.secondary)
            }
        }
    }

    /// The agent records ISO 8601 in UTC. Nobody reads that at a glance, so show a local clock time.
    private static func shortTime(_ iso: String?) -> String? {
        guard let iso, let date = ISO8601DateFormatter.withFractional.date(from: iso) else { return nil }
        return date.formatted(date: .abbreviated, time: .shortened)
    }

    private var updateNote: String {
        guard model.commandableSystem != nil else { return "Nothing can be installed while \(model.name) is unreachable." }
        guard let service else { return "The state could not be read." }
        if let reason = service.updateUnavailableReason { return reason }
        if let staged = service.stagedVersion {
            return "Build \(staged) is downloaded and ready. Applying it quits \(service.displayName) and starts it again."
        }
        let version = service.latest.map { "Version \($0)" } ?? "A newer build"
        return "\(version) is available. Installing it stops \(service.displayName), puts the new build in, and starts it again."
    }

    // MARK: - Automatic updates

    private func autoUpdateRow(for system: SystemConfig) -> some View {
        let isLive = model.currentSystem?.id == system.id
        let value = model.autoUpdateValue(for: system)
        return HStack(alignment: .firstTextBaseline, spacing: 18) {
            Toggle(isOn: Binding(
                get: { value ?? false },
                set: { model.setAutoUpdate($0, on: system) }
            )) {
                Text(system.name)
                    .frame(width: 108, alignment: .leading)
            }
            .toggleStyle(.switch)
            .disabled(!isLive || model.isWorking)

            Text(autoUpdateNote(for: system, isLive: isLive, value: value))
                .font(.callout)
                .foregroundStyle(.secondary)
            Spacer(minLength: 0)
        }
    }

    private func autoUpdateNote(for system: SystemConfig, isLive: Bool, value: Bool?) -> String {
        if isLive {
            return value == true ? "Updates install on their own." : "Updates only happen when you ask."
        }
        guard let value, let checked = model.autoUpdateCheckedAt(for: system) else {
            return "Not known yet. Boot \(system.name) once to read it."
        }
        let stamp = checked.formatted(date: .abbreviated, time: .shortened)
        return "Last known: \(value ? "on" : "off") (read on \(stamp))"
    }
}
