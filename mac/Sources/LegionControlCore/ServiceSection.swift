import SwiftUI

/// Everything about one service on one machine: what is installed, whether it is answering, what it
/// is doing, when it is allowed to update itself, and the ways to change all of that.
struct ServiceSection: View {
    let model: MachineModel
    let serviceId: String

    private var service: ServiceStatus? { model.service(id: serviceId) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: service?.displayName ?? serviceId,
                        note: model.currentSystem.map { "on \($0.name)" })

            if let service {
                state(service)
                actions(service)
                if service.canUpdate != false { MaintenanceRows(
                    policy: service.updates,
                    systemPolicy: model.policy,
                    subject: service.displayName,
                    isEnabled: !model.isWorking && model.commandableSystem != nil,
                    supportsPolicy: model.dialect.supportsPolicy,
                    setAutomatic: { model.setPolicy(automatic: .some($0), service: service,
                                                    describedAs: "Saving the update setting") },
                    pause: { model.pause(for: $0, service: service) },
                    resume: { model.resume(service: service) },
                    setWindows: { model.setWindows($0, service: service) },
                    inherit: { model.inheritPolicy(service: service) }
                ) }
                notes(service)
            } else {
                Text("Not available while \(model.name) is unreachable.")
                    .foregroundStyle(.secondary)
            }
        }
    }

    // MARK: - State

    private func state(_ service: ServiceStatus) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            DetailRow(label: "Installed") {
                VersionText(value: service.installed, placeholder: "not installed")
            }
            if service.canUpdate == false {
                DetailRow("Updates", "Managed by application")
            } else {
                DetailRow(label: latestLabel(service)) {
                    VersionText(value: service.latest, placeholder: "could not be checked")
                }
                DetailRow(label: "Version") { versionVerdict(service) }
            }
            DetailRow(label: "Process") { processVerdict(service) }
            DetailRow(label: "Answering") { healthVerdict(service) }
            DetailRow(label: "Doing now") { activityVerdict(service) }
            DetailRow(label: "Reachable outside") { endpointVerdict(service) }

            if let threads = service.busy?.threads, !threads.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    // Position, not thread id: the agent may leave the id out, and the whole list is
                    // replaced on every poll anyway.
                    ForEach(Array(threads.enumerated()), id: \.offset) { _, thread in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Text(thread.title ?? "untitled")
                            Text(thread.state ?? "")
                                .foregroundStyle(.secondary)
                        }
                        .font(.callout)
                    }
                    if let truncated = service.busy?.threadsTruncated, truncated > 0 {
                        Text("and \(truncated) more")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }
                .padding(.leading, 158)
            }

            if let last = service.lastOperation {
                DetailRow(label: "Last operation") { lastOperationText(last) }
            } else if let last = service.lastUpdate {
                DetailRow(label: "Last update") { lastUpdateText(last) }
            }
        }
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
        } else if let pending = service.pendingVersion {
            // The agent found a newer build and held the install back. Nothing is installed yet, so
            // this must not say that it is.
            StatusText(symbol: "clock.arrow.circlepath", text: "\(pending) is queued for the next idle window", tint: .orange)
        } else if service.pendingRestart == true {
            StatusText(symbol: "clock.arrow.circlepath", text: "A newer build is queued for the next idle window", tint: .orange)
        } else {
            StatusText(symbol: "arrow.down.circle", text: "A newer build is available", tint: .orange)
        }
    }

    /// Whether the process exists, which is not the same question as whether it answers.
    @ViewBuilder
    private func processVerdict(_ service: ServiceStatus) -> some View {
        if let process = service.process {
            if process.running == true {
                let started = ISO8601DateFormatter.lenient.date(from: process.startedAt)
                StatusText(symbol: "checkmark.circle",
                           text: started.map { "Running since \($0.formatted(date: .abbreviated, time: .shortened))" }
                            ?? "Running",
                           tint: .green)
            } else if let error = process.error, !error.isEmpty {
                StatusText(symbol: "exclamationmark.triangle", text: error, tint: .orange)
            } else {
                StatusText(symbol: "xmark.circle", text: process.state ?? "Not running", tint: .red)
            }
        } else if service.running == true {
            StatusText(symbol: "checkmark.circle", text: "Running", tint: .green)
        } else if service.running == false {
            StatusText(symbol: "xmark.circle", text: "Stopped", tint: .red)
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func healthVerdict(_ service: ServiceStatus) -> some View {
        if let health = service.health {
            if health.ok == true {
                StatusText(symbol: "checkmark.circle",
                           text: service.port.map { "Healthy on port \($0)" } ?? "Healthy",
                           tint: .green)
            } else if let error = health.error, !error.isEmpty {
                StatusText(symbol: "exclamationmark.triangle", text: error, tint: .orange)
            } else {
                StatusText(symbol: "exclamationmark.triangle",
                           text: health.status.map { "Answered \($0)" } ?? "Not answering",
                           tint: .orange)
            }
        } else if service.healthy == true {
            StatusText(symbol: "checkmark.circle",
                       text: service.port.map { "Healthy on port \($0)" } ?? "Healthy", tint: .green)
        } else if service.running == true {
            StatusText(symbol: "exclamationmark.triangle", text: "Running but not answering", tint: .orange)
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    /// The busy state, with the three answers it really has.
    ///
    /// "Not busy" used to cover three different things: idle, nobody is watching, and the probe
    /// failed. Only the first is a reason to go ahead with something disruptive, and conflating them
    /// is how work gets interrupted by a check that never ran.
    @ViewBuilder
    private func activityVerdict(_ service: ServiceStatus) -> some View {
        if let busy = service.busy {
            switch busy.verdict {
            case .busy:
                StatusText(symbol: "circle.dotted", text: busy.summary, tint: .orange)
            case .idle:
                StatusText(symbol: "pause.circle", text: evidenceText(busy, "Idle"))
            case .unknown:
                StatusText(symbol: "questionmark.circle",
                           text: evidenceText(busy, "Busy state unknown, so disruptive work waits"),
                           tint: .orange)
            case .unmonitored:
                StatusText(symbol: "eye.slash",
                           text: "Nothing watches this service, so disruptive work waits",
                           tint: .orange)
            }
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    private func evidenceText(_ busy: BusyStatus, _ base: String) -> String {
        guard let evidence = busy.evidence, !evidence.isEmpty, evidence != "none" else { return base }
        return "\(base) (\(evidence))"
    }

    /// External reachability, which relay-process-is-up never proved.
    @ViewBuilder
    private func endpointVerdict(_ service: ServiceStatus) -> some View {
        if let endpoint = service.endpoint {
            if endpoint.configured != true {
                Text("not configured").foregroundStyle(.secondary)
            } else if endpoint.reachable == true {
                StatusText(symbol: "checkmark.circle", text: endpoint.url ?? "Reachable", tint: .green)
            } else if endpoint.reachable == false {
                StatusText(symbol: "xmark.circle", text: endpoint.error ?? "Not reachable", tint: .red)
            } else {
                Text("not checked; run the deep check").foregroundStyle(.secondary)
            }
        } else if let relay = service.relay {
            if relay.configured != true {
                Text("no relay configured").foregroundStyle(.secondary)
            } else if relay.running == true {
                StatusText(symbol: "checkmark.circle", text: "Relay running, reachability not checked", tint: .secondary)
            } else {
                StatusText(symbol: "xmark.circle", text: "Relay configured but not running", tint: .red)
            }
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    private func lastOperationText(_ last: AgentLastOperation) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text([last.kind, last.action].compactMap { $0 }.joined(separator: " · "))
                if let when = ISO8601DateFormatter.lenient.date(from: last.at) {
                    Text(when.formatted(date: .abbreviated, time: .shortened)).foregroundStyle(.tertiary)
                }
            }
            .font(.callout)
            .foregroundStyle(.secondary)
            if let reason = AgentReason(code: last.reasonCode) {
                Text(reason.sentence(subject: service?.displayName ?? serviceId, message: nil))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func lastUpdateText(_ last: LastUpdate) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(last.result ?? "none recorded")
                if let when = ISO8601DateFormatter.lenient.date(from: last.at) {
                    Text(when.formatted(date: .abbreviated, time: .shortened)).foregroundStyle(.tertiary)
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

    // MARK: - Actions

    private func actions(_ service: ServiceStatus) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "Actions")

            FlowRow(spacing: 10) {
                // Prominent only when a newer build has actually been detected. The rest of the time
                // this is a button that would do nothing, so it neither invites a press nor accepts
                // one.
                if service.canUpdate != false { PrimaryActionButton(
                    title: "Update now",
                    isHighlighted: canUpdate(service),
                    isEnabled: !model.isWorking && canUpdate(service)
                ) { model.requestUpdate(service) }

                if model.dialect.supportsQueue {
                    Button("Update when idle") { model.requestUpdateWhenIdle(service) }
                        .disabled(model.isWorking || !canUpdate(service))
                }

                }
                Button("Restart") { model.requestRestart(service) }
                    .disabled(model.isWorking || model.commandableSystem == nil || service.canRestart == false)

                if model.dialect.supportsQueue {
                    Button("Restart when idle") { model.restart(service, force: false, whenIdle: true) }
                        .disabled(model.isWorking || model.commandableSystem == nil || service.canRestart == false)
                }

                ForEach(model.actions(for: service)) { action in
                    Button(action.displayName) { model.requestAction(action) }
                        .disabled(model.isWorking || model.commandableSystem == nil)
                }
            }

            QuietNote(text: updateNote(service))
                .padding(.top, 12)
        }
    }

    private func canUpdate(_ service: ServiceStatus) -> Bool {
        guard model.commandableSystem != nil else { return false }
        return service.canBeUpdated
    }

    private func updateNote(_ service: ServiceStatus) -> String {
        guard model.commandableSystem != nil else { return "Nothing can be installed while \(model.name) is unreachable." }
        if let reason = service.updateUnavailableReason { return reason }
        if let staged = service.stagedVersion {
            return "Build \(staged) is downloaded and ready. Applying it quits \(service.displayName) and starts it again."
        }
        let version = service.latest.map { "Version \($0)" } ?? "A newer build"
        return "\(version) is available. Asking for it now ignores the schedule below: that only governs what happens on its own."
    }

    // MARK: - Notes

    @ViewBuilder
    private func notes(_ service: ServiceStatus) -> some View {
        if let notes = service.notes, !notes.isEmpty {
            SectionHeading(title: "Could not be read")
            VStack(alignment: .leading, spacing: 6) {
                ForEach(notes, id: \.self) { note in
                    Text(note)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }
}

/// The update policy rows, shared by the service section and the local device section.
///
/// This is finding 06 as a piece of UI. Three separate things used to be one switch: whether the
/// schedule may act, whether a person may ask, and whether either may interrupt work. Turning the
/// switch off turned off all three, which meant a machine with automatic updates disabled could not
/// be updated at all. Now the switch governs the schedule and nothing else, and the buttons above
/// say so.
struct MaintenanceRows: View {
    var policy: AgentUpdatePolicy?
    var systemPolicy: AgentUpdatePolicy?
    var subject: String
    var isEnabled: Bool
    var supportsPolicy: Bool
    var setAutomatic: (Bool) -> Void
    var pause: (TimeInterval) -> Void
    var resume: () -> Void
    var setWindows: ([MaintenanceWindow]) -> Void
    var inherit: () -> Void

    @State private var editingWindow = false
    @State private var draftFrom = "02:00"
    @State private var draftTo = "06:00"
    @State private var draftDays = Set(MaintenanceWindow.allDays)

    private var effective: AgentUpdatePolicy? { policy ?? systemPolicy }

    var body: some View {
        if supportsPolicy {
            VStack(alignment: .leading, spacing: 0) {
                SectionHeading(title: "Maintenance")

                QuietNote(text: "This is about the schedule only. Asking for an update yourself always works, whatever these say.")
                    .padding(.bottom, 12)

                VStack(alignment: .leading, spacing: 12) {
                    Toggle(isOn: Binding(
                        get: { effective?.automatic ?? false },
                        set: { setAutomatic($0) }
                    )) {
                        Text("Update \(subject) on the schedule")
                    }
                    .toggleStyle(.switch)
                    .disabled(!isEnabled)

                    DetailRow(label: "Right now") { eligibility }
                    DetailRow(label: "Paused") { pausedRow }
                    DetailRow(label: "Window") { windowRow }
                    if policy?.inherited == true {
                        DetailRow("Follows", "the system-wide setting")
                    }
                }
                .padding(.bottom, 14)

                FlowRow(spacing: 10) {
                    if effective?.isPaused == true {
                        Button("Resume now") { resume() }.disabled(!isEnabled)
                    } else {
                        Button("Pause 4 hours") { pause(4 * 60 * 60) }.disabled(!isEnabled)
                        Button("Pause a day") { pause(24 * 60 * 60) }.disabled(!isEnabled)
                    }
                    Button(editingWindow ? "Cancel" : "Set a window") { editingWindow.toggle() }
                        .disabled(!isEnabled)
                    if effective?.maintenanceWindows?.isEmpty == false {
                        Button("Any time") { setWindows([]) }.disabled(!isEnabled)
                    }
                    if policy?.inherited == false {
                        Button("Follow the system") { inherit() }.disabled(!isEnabled)
                    }
                }

                if editingWindow { windowEditor }
            }
        }
    }

    @ViewBuilder
    private var eligibility: some View {
        if let policy = effective {
            if policy.eligibleNow == true {
                StatusText(symbol: "checkmark.circle", text: "The schedule may update it now", tint: .green)
            } else if let reason = policy.deferred {
                StatusText(symbol: "clock", text: reason.sentence(subject: subject, message: nil), tint: .secondary)
            } else if policy.automatic == false {
                StatusText(symbol: "minus.circle", text: "Off the schedule", tint: .secondary)
            } else if policy.inWindowNow == false {
                StatusText(symbol: "clock", text: "Outside the window", tint: .secondary)
            } else {
                Text("not known").foregroundStyle(.secondary)
            }
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var pausedRow: some View {
        if let until = effective?.pausedUntilDate, until > Date() {
            StatusText(symbol: "pause.circle",
                       text: "until \(until.formatted(date: .abbreviated, time: .shortened)) (\(until.relativeDescription()))",
                       tint: .orange)
        } else {
            Text("not paused").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var windowRow: some View {
        let windows = effective?.maintenanceWindows ?? []
        if windows.isEmpty {
            Text("any time").foregroundStyle(.secondary)
        } else {
            VStack(alignment: .leading, spacing: 3) {
                ForEach(windows) { window in
                    Text(window.summary).font(.callout)
                }
                if let next = effective?.nextWindow,
                   let date = ISO8601DateFormatter.lenient.date(from: next) {
                    Text("next \(date.formatted(date: .abbreviated, time: .shortened))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private var windowEditor: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                TextField("From", text: $draftFrom).frame(width: 80)
                Text("to").foregroundStyle(.secondary)
                TextField("To", text: $draftTo).frame(width: 80)
                Text("the machine's own local time").font(.callout).foregroundStyle(.secondary)
            }
            .textFieldStyle(.roundedBorder)

            FlowRow(spacing: 8) {
                ForEach(MaintenanceWindow.allDays, id: \.self) { day in
                    Toggle(isOn: Binding(
                        get: { draftDays.contains(day) },
                        set: { on in if on { draftDays.insert(day) } else { draftDays.remove(day) } }
                    )) {
                        Text(day.capitalized).font(.callout)
                    }
                    .toggleStyle(.checkbox)
                }
            }

            let draft = MaintenanceWindow(days: MaintenanceWindow.allDays.filter { draftDays.contains($0) },
                                          from: draftFrom, to: draftTo)
            if draft.crossesMidnight {
                QuietNote(text: "This window runs overnight, from \(draftFrom) one day to \(draftTo) the next.")
            }
            HStack(spacing: 10) {
                Button("Save the window") {
                    setWindows([draft])
                    editingWindow = false
                }
                .buttonStyle(.borderedProminent)
                .disabled(!draft.looksValid || !isEnabled)
                if !draft.looksValid {
                    QuietNote(text: "Both ends have to be HH:MM, and they cannot be the same time.")
                }
            }
        }
        .padding(.top, 14)
    }
}
