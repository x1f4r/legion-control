import SwiftUI

/// This device's own business: the services that run here rather than over ssh, this device's
/// private settings, and Legion Control itself.
struct MacSection: View {
    let model: AppModel
    let mac: MacModel

    // Read once and after every change rather than inside the body: the status comes from launchd,
    // and the body runs again on every poll while this section is open.
    @State private var startsAtLogin = LoginItem.isEnabled
    @State private var needsLoginApproval = LoginItem.needsApproval
    @State private var loginProblem: String?
    @State private var editProblem: String?
    @State private var exportingDiagnostics = false
    @State private var exportProblem: String?
    @State private var panel: String?
    @State private var selectedService: ServiceStatus?
    @State private var sheetActions = SheetActionRelay()


    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            ViewThatFits(in: .horizontal) {
                HStack(alignment: .center, spacing: 16) { heading; Spacer(); deviceActions }
                VStack(alignment: .leading, spacing: 12) { heading; deviceActions }
            }
            if mac.failure != nil {
                HStack {
                    Text("Unable to read this device").foregroundStyle(.orange)
                    Button("Details") { panel = "Diagnostics" }
                }
            }
            if case .conflict = mac.setupSharing { setupVerdict }
            unresolvedBlock
            queueBlock
            if mac.services.isEmpty {
                Text(mac.isReachable ? "No services configured" : "Waiting for this device…")
                    .foregroundStyle(.secondary)
            } else { localServices }
            MetricsRows(metrics: mac.status?.metrics)
            if let note = mac.note, !note.isEmpty {
                Text(note).font(.callout).foregroundStyle(mac.noteIsError ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                    .lineLimit(2).textSelection(.enabled)
            }
        }
        .onChange(of: model.dialog?.id) { _, id in
            if id != nil { selectedService = nil; panel = nil }
        }
        .sheet(item: $selectedService, onDismiss: { sheetActions.didDismiss() }) { service in
            ControlSheet(title: service.displayName) { serviceBlock(mac.services.first { $0.id == service.id } ?? service) }
        }
        .sheet(isPresented: Binding(get: { panel != nil }, set: { if !$0 { panel = nil } }), onDismiss: { sheetActions.didDismiss() }) {
            ControlSheet(title: panel ?? "Details") {
                switch panel {
                case "Settings": deviceSection; appSettings
                case "Schedule": schedule
                case "Activity":
                    Text(model.operations.exportText()).font(.callout.monospaced()).textSelection(.enabled)
                    Button("Export operation history") { exportHistory() }
                    Button("Clear history") { model.operations.clearHistory() }
                default:
                    if let failure = mac.failure {
                        Text(failure.localMessage).foregroundStyle(.orange)
                        if let detail = failure.detailText { Text(detail).font(.callout.monospaced()).textSelection(.enabled) }
                    }
                    notesSection
                    appDiagnostics
                }
            }
        }
        .onAppear {
            startsAtLogin = LoginItem.isEnabled
            needsLoginApproval = LoginItem.needsApproval
        }
    }

    private func afterSheet(_ action: @escaping @MainActor () -> Void) {
        guard selectedService != nil || panel != nil else { action(); return }
        sheetActions.queue(action)
        selectedService = nil
        panel = nil
    }

    private var heading: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(mac.name).font(.system(size: 22, weight: .semibold))
            Text(mac.isReachable ? "This device" : "Unavailable").font(.callout).foregroundStyle(.secondary)
        }
    }

    private var deviceActions: some View {
        FlowRow(spacing: 8) {
            if mac.boundMachineId != nil, mac.isReachable {
                Button("Sleep") { mac.requestSleep() }.disabled(mac.isWorking)
                if !mac.bootTargets.isEmpty {
                    Menu("Restart") {
                        ForEach(mac.bootTargets) { target in
                            Button("Boot into \(target.name)") { mac.requestBoot(into: target) }.disabled(mac.isWorking)
                        }
                    }.fixedSize()
                }
            }
            Menu("Actions") {
                ForEach(mac.actions) { action in
                    Button(action.displayName) { mac.requestAction(action) }.disabled(mac.isWorking || !mac.isReachable)
                }
                Divider()
                Button("Activity…") { panel = "Activity" }
                Button("Schedule…") { panel = "Schedule" }
                Button("Diagnostics…") { panel = "Diagnostics" }
                Button("Settings…") { panel = "Settings" }
            }.fixedSize()
            ServiceSetupButton(enabled: !mac.isWorking && mac.dialect.supportsDoctor,
                               restricted: mac.status?.isRestrictedSession == true,
                               send: { try await mac.serviceConfig($0) })
        }
    }

    // MARK: - Services here

    private var localServices: some View {
        VStack(spacing: 0) {
            ForEach(mac.services) { service in
                CompactServiceRow(service: service,
                                  updateEnabled: !mac.isWorking && mac.updateUnavailableReason(service) == nil,
                                  update: { mac.requestUpdate(service) }, details: { selectedService = service }) {
                    if service.canUpdate != false, mac.dialect.supportsQueue {
                        Button("Update when idle") { mac.requestUpdateWhenIdle(service) }
                            .disabled(mac.isWorking || mac.updateUnavailableReason(service) != nil)
                    }
                    Button("Restart") { mac.requestRestart(service) }
                        .disabled(mac.isWorking || mac.restartUnavailableReason(service) != nil)
                    if mac.dialect.supportsQueue {
                        Button("Restart when idle") { mac.restart(service, force: false, whenIdle: true) }
                            .disabled(mac.isWorking || mac.restartUnavailableReason(service) != nil)
                    }
                    ForEach(mac.actions(for: service)) { action in
                        Button(action.displayName) { mac.requestAction(action) }.disabled(mac.isWorking || !mac.isReachable)
                    }
                }
            }
        }
    }

    private var schedule: some View {
        VStack(alignment: .leading, spacing: 12) {
            MaintenanceRows(
                policy: mac.policy, systemPolicy: mac.policy, subject: mac.name,
                isEnabled: mac.isReachable && !mac.isWorking,
                supportsPolicy: mac.dialect.supportsPolicy && mac.services.contains { $0.canUpdate != false },
                setAutomatic: { mac.setAutoUpdate($0) }, pause: { mac.pause(for: $0, service: nil) },
                resume: { mac.resume(service: nil) }, setWindows: { mac.setWindows($0, service: nil) }, inherit: { }
            )
            if !mac.dialect.supportsPolicy {
                Toggle("Automatic updates", isOn: Binding(get: { mac.autoUpdate ?? false }, set: { mac.setAutoUpdate($0) }))
                    .disabled(!mac.isReachable || mac.isWorking)
            }
        }
    }

    /// One service here, with the same controls a remote machine's service has.
    ///
    /// This is the parity the review asked for. The local device used to have one Update button and
    /// nothing else, which made the machine you are sitting in front of the one you could do least
    /// to; it now restarts, runs configured actions and queues work exactly as any other machine
    /// does, with the same confirmations and the same refusals.
    private func serviceBlock(_ service: ServiceStatus) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: service.displayName)

            VStack(alignment: .leading, spacing: 12) {
                DetailRow(label: "Installed") {
                    VersionText(value: service.installed, placeholder: "not installed")
                }
                if service.canUpdate == false {
                    DetailRow("Updates", "Managed by application")
                } else {
                    DetailRow(label: "Newest") {
                        VersionText(value: service.latest, placeholder: "could not be checked")
                    }
                    DetailRow(label: "Update waiting") { stagedVerdict(service) }
                }
                DetailRow(label: "Doing now") { activityVerdict(service) }
                if let path = service.appPath {
                    DetailRow(label: "Application") {
                        Text(path)
                            .font(.system(.callout, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
            }
            .padding(.bottom, 16)

            FlowRow(spacing: 10) {
                if service.canUpdate != false { PrimaryActionButton(
                    title: "Update \(service.displayName) now",
                    isHighlighted: mac.updateUnavailableReason(service) == nil,
                    isEnabled: !mac.isWorking && mac.updateUnavailableReason(service) == nil
                ) { afterSheet { mac.requestUpdate(service) } }

                if mac.dialect.supportsQueue {
                    Button("Update when idle") { afterSheet { mac.requestUpdateWhenIdle(service) } }
                        .disabled(mac.isWorking || mac.updateUnavailableReason(service) != nil)
                }

                }
                Button("Restart") { afterSheet { mac.requestRestart(service) } }
                    .disabled(mac.isWorking || mac.restartUnavailableReason(service) != nil)
                    .help(mac.restartUnavailableReason(service) ?? "Stops and starts \(service.displayName). Held back while work is running.")

                if mac.dialect.supportsQueue {
                    Button("Restart when idle") { afterSheet { mac.restart(service, force: false, whenIdle: true) } }
                        .disabled(mac.isWorking || mac.restartUnavailableReason(service) != nil)
                }

                ForEach(mac.actions(for: service)) { action in
                    Button(action.displayName) { afterSheet { mac.requestAction(action) } }
                        .disabled(mac.isWorking || !mac.isReachable)
                }

                if mac.isWorking {
                    ProgressView().controlSize(.small)
                }
            }

            QuietNote(text: updateNote(service))
                .padding(.top, 12)
        }
    }

    @ViewBuilder
    private func stagedVerdict(_ service: ServiceStatus) -> some View {
        if let staged = service.stagedVersion {
            StatusText(symbol: "arrow.down.circle", text: "\(staged) is downloaded and waiting for a quit", tint: .orange)
        } else if service.upToDate == false {
            StatusText(symbol: "arrow.down.circle", text: "A newer build exists and has not been fetched yet", tint: .orange)
        } else {
            StatusText(symbol: "checkmark.circle", text: "Nothing staged", tint: .green)
        }
    }

    @ViewBuilder
    private func activityVerdict(_ service: ServiceStatus) -> some View {
        if let busy = service.busy {
            switch busy.isUnknown ? .unknown : !busy.isMonitored ? .unmonitored : busy.verdict {
            case .busy:
                StatusText(symbol: "circle.dotted", text: busy.summary, tint: .orange)
            case .idle:
                StatusText(symbol: "pause.circle", text: "Idle, an update would go in now")
            case .unknown:
                StatusText(symbol: "questionmark.circle", text: "Busy state unknown, so disruptive work waits", tint: .orange)
            case .unmonitored:
                StatusText(symbol: "eye.slash", text: "Nothing watches this service, so disruptive work waits", tint: .orange)
            }
        } else if mac.isReachable {
            StatusText(symbol: "pause.circle", text: "Idle, an update would go in now")
        } else {
            Text("not known").foregroundStyle(.secondary)
        }
    }

    private func updateNote(_ service: ServiceStatus) -> String {
        if let reason = mac.updateUnavailableReason(service) { return reason }
        if let staged = service.stagedVersion {
            return "Build \(staged) is downloaded and ready. Applying it quits \(service.displayName) and starts it again."
        }
        let version = service.latest.map { "Version \($0)" } ?? "A newer build"
        return "\(version) is available. This fetches it and applies it once nothing is running."
    }

    // MARK: - Operations here

    @ViewBuilder
    private var boundPowerSection: some View {
        if mac.boundMachineId != nil, mac.isReachable {
            SectionHeading(title: "Power")
            FlowRow(spacing: 10) {
                ForEach(mac.bootTargets) { target in
                    Button("Boot into \(target.name)") { mac.requestBoot(into: target) }
                        .disabled(mac.isWorking)
                }
                Button("Sleep this device") { mac.requestSleep() }
                    .disabled(mac.isWorking)
            }
            QuietNote(text: "This device is also the controller. After it comes back, Legion Control reads the durable operation record instead of inferring the outcome from uptime.")
                .padding(.top, 10)
        }
    }

    @ViewBuilder
    private var unresolvedBlock: some View {
        let unresolved = mac.unresolvedOperations
        if !unresolved.isEmpty {
            SectionHeading(title: "Outcome not known")

            VStack(alignment: .leading, spacing: 10) {
                ForEach(unresolved) { record in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Image(systemName: "questionmark.circle").foregroundStyle(.orange)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(record.title).font(.callout)
                            Text(record.summary).font(.caption).foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer(minLength: 8)
                        Button("Check now") { mac.reconcileNow() }.controlSize(.small)
                        Button("Dismiss") { mac.dismissUnknownOutcome(record.id) }.controlSize(.small)
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var queueBlock: some View {
        let queued = mac.queuedOperations
        if !queued.isEmpty {
            SectionHeading(title: "Waiting for idle")
            VStack(alignment: .leading, spacing: 10) {
                ForEach(queued) { operation in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Image(systemName: "clock.arrow.circlepath").foregroundStyle(.secondary)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(operation.displayName).font(.callout)
                            if let expires = ISO8601DateFormatter.lenient.date(from: operation.expiresAt) {
                                Text("expires \(expires.relativeDescription())")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        Spacer(minLength: 8)
                        Button("Cancel") { mac.cancelQueued(operation) }
                            .controlSize(.small)
                            .disabled(mac.isWorking)
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var actionsSection: some View {
        if !mac.actions.isEmpty {
            SectionHeading(title: "Actions")
            FlowRow(spacing: 10) {
                ForEach(mac.actions) { action in
                    Button(action.displayName) { mac.requestAction(action) }
                        .disabled(mac.isWorking || !mac.isReachable)
                }
            }
        }
    }

    @ViewBuilder
    private var notesSection: some View {
        if !mac.notes.isEmpty {
            SectionHeading(title: "Could not be read")
            VStack(alignment: .leading, spacing: 6) {
                ForEach(mac.notes, id: \.self) { note in
                    Text(note)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    // MARK: - This device

    /// The private settings, and the setup this device shares with everything else.
    ///
    /// The split is the point. The setup is the same document on every device and travels between
    /// them; the bindings are this device's alone — which machine it is, which key it offers, what
    /// its own ssh config calls things — and are never sent anywhere.
    private var deviceSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "This device")

            VStack(alignment: .leading, spacing: 12) {
                DetailRow("Name", model.bindings.bindings.effectiveDeviceName)
                DetailRow(label: "Setup") { setupVerdict }
                DetailRow(label: "Revision") { revisionRow }
                DetailRow(label: "Where") { siteRow }
                DetailRow(label: "Config file") {
                    Text(model.config.path)
                        .font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                DetailRow(label: "Private settings") {
                    Text(model.bindings.path)
                        .font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                if let selfBinding = model.bindings.bindings.selfBinding {
                    DetailRow(label: "This device is") {
                        Text(model.config.config?.machine(id: selfBinding.machine)?.name ?? selfBinding.machine)
                    }
                }
                if mac.boundMachineId != nil {
                    DetailRow(label: "Controlled") {
                        StatusText(symbol: "bolt.horizontal.circle", text: "locally, without ssh", tint: .green)
                    }
                }
            }
            .padding(.bottom, 14)

            FlowRow(spacing: 10) {
                Button("Edit the setup") { model.openConfigInEditor() }
                Button("Edit private settings") { model.openBindingsInEditor() }
                if model.config.config?.identity.id == nil, model.config.config != nil {
                    Button("Give this setup an identity") { editProblem = model.adoptOwnDocument() }
                }
                if hasPrivateKeysInSharedDocument {
                    Button("Move private settings out of the setup") {
                        editProblem = model.movePrivateSettingsOut()
                    }
                }
            }

            if let problem = model.config.problem {
                QuietNote(text: "The setup file was edited into something that could not be read, so the machines above are the ones from before it. \(problem)")
                    .padding(.top, 12)
            }
            if let problem = model.bindings.problem {
                QuietNote(text: problem).padding(.top, 12)
            }
            if let editProblem {
                QuietNote(text: editProblem).padding(.top, 12)
            }
            ForEach(model.setupWarnings, id: \.self) { warning in
                QuietNote(text: warning).padding(.top, 12)
            }

            if !model.config.storedRevisions().isEmpty {
                revisionsBlock
            }
        }
    }

    private var hasPrivateKeysInSharedDocument: Bool {
        guard let config = model.config.config else { return false }
        if config.local != nil { return true }
        return config.machines.contains { $0.ssh?.identityFile?.isEmpty == false }
    }

    @ViewBuilder
    private var setupVerdict: some View {
        switch mac.setupSharing {
        case .upToDate:
            StatusText(symbol: "checkmark.circle", text: "the local agent holds the same document", tint: .green)
        case .justShared:
            StatusText(symbol: "checkmark.circle", text: "shared just now", tint: .green)
        case .publishing:
            StatusText(symbol: "arrow.up.circle", text: "sending the newer revision to the local agent", tint: .secondary)
        case .fetching:
            StatusText(symbol: "arrow.down.circle", text: "taking the newer revision from the local agent", tint: .secondary)
        case .unsupported:
            Text("the local agent cannot carry the setup").foregroundStyle(.secondary)
        case .failed(let sentence):
            StatusText(symbol: "exclamationmark.triangle", text: sentence, tint: .orange)
        case .conflict(let sentence):
            VStack(alignment: .leading, spacing: 8) {
                StatusText(symbol: "exclamationmark.triangle", text: sentence, tint: .orange)
                HStack(spacing: 10) {
                    Button("Review the differences") { afterSheet { mac.openSetupDivergence() } }.controlSize(.small)
                }
            }
        case .unknown:
            Text("not known").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var revisionRow: some View {
        if let identity = model.config.config?.identity, identity.id != nil {
            VStack(alignment: .leading, spacing: 2) {
                Text("\(identity.revisionNumber) · \(identity.name ?? identity.id ?? "")")
                Text("last written by \(identity.authorDescription)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } else {
            Text("this document has no identity yet, so it cannot be reconciled with other devices")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Which network this device is on, and how sure that is.
    @ViewBuilder
    private var siteRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(model.placement.description(sites: model.sites))
                .fixedSize(horizontal: false, vertical: true)
            if !model.sites.isEmpty {
                Picker("", selection: Binding(
                    get: { model.bindings.bindings.currentSite ?? "" },
                    set: { model.confirmSite($0.isEmpty ? nil : $0) }
                )) {
                    Text("Work it out from the addresses").tag("")
                    ForEach(model.sites) { site in
                        Text(site.name).tag(site.id)
                    }
                }
                .labelsHidden()
                .pickerStyle(.menu)
                .fixedSize()
            }
        }
    }

    private var revisionsBlock: some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionHeading(title: "Kept revisions")
            QuietNote(text: "Every superseded document, by hash. They are what a merge compares against, and what a mistaken edit can be put back from.")
            ForEach(model.config.storedRevisions().prefix(8)) { revision in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(revision.name)
                        .font(.system(.callout, design: .monospaced))
                    Text(revision.savedAt.formatted(date: .abbreviated, time: .shortened))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer(minLength: 8)
                    Button("Put this back") {
                        editProblem = model.config.restore(revision,
                                                           deviceName: model.bindings.bindings.effectiveDeviceName)
                    }
                    .controlSize(.small)
                }
            }
        }
    }

    // MARK: - Legion Control itself

    private var appSettings: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "Legion Control")

            DetailRow(label: "Version") {
                VersionText(value: Self.appVersion, placeholder: "not known")
            }
            .padding(.bottom, 14)

            updateBlock
                .padding(.bottom, 14)

            Toggle(isOn: Binding(
                get: { startsAtLogin },
                set: { wanted in
                    loginProblem = LoginItem.set(wanted)
                    startsAtLogin = LoginItem.isEnabled
                    needsLoginApproval = LoginItem.needsApproval
                }
            )) {
                Text("Start Legion Control at login")
            }
            .toggleStyle(.switch)

            if let loginProblem {
                QuietNote(text: loginProblem).padding(.top, 8)
            } else if needsLoginApproval {
                QuietNote(text: "Waiting for approval in System Settings, under General and then Login Items.")
                    .padding(.top, 8)
            }

        }
    }

    private var appDiagnostics: some View {
        VStack(alignment: .leading, spacing: 12) {
            SectionHeading(title: "Diagnostics")
            DiagnosticActions(enabled: !mac.isWorking && mac.dialect.supportsDoctor,
                              check: { mac.runDoctor() }, deepCheck: { mac.runDoctor(deep: true) },
                              log: { await mac.fetchLog() }, bundle: { await mac.fetchBundle() })
                .padding(.bottom, 12)

            FlowRow(spacing: 10) {
                ServiceSetupButton(enabled: !mac.isWorking && mac.dialect.supportsDoctor,
                                   restricted: mac.status?.isRestrictedSession == true,
                                   send: { try await mac.serviceConfig($0) })
                Button("Install agent\(AgentBundle.shared.map { " " + $0.version } ?? "")") { afterSheet { mac.installBundledAgent() } }
                    .disabled(AgentBundle.shared == nil || mac.isWorking || mac.status?.ok != true || mac.status?.isRestrictedSession == true)
                    .help(AgentBundle.shared == nil ? AgentBundle.unavailableReason : "Upgrade the authenticated local agent.")
                Button("Export all diagnostics") { exportDiagnostics() }.disabled(exportingDiagnostics)
                Button("Export operation history") { exportHistory() }
                if !model.operations.records.isEmpty {
                    Button("Clear the history") { model.operations.clearHistory() }
                }
            }

            if exportingDiagnostics { Text("Collecting diagnostics…").font(.caption).foregroundStyle(.secondary) }
            if let exportProblem { Text(exportProblem).foregroundStyle(.orange).textSelection(.enabled) }

            if let problem = model.operations.persistenceProblem {
                QuietNote(text: problem).padding(.top, 12)
            }

            if let doctor = mac.doctor {
                localDoctor(doctor)
            }
        }
    }

    private func localDoctor(_ doctor: AgentDoctorResult) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(doctor.checks ?? []) { check in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: check.verdict == .ok ? "checkmark.circle" : "exclamationmark.triangle")
                        .foregroundStyle(check.verdict == .ok ? AnyShapeStyle(.green) : AnyShapeStyle(.orange))
                    VStack(alignment: .leading, spacing: 2) {
                        Text(check.summary ?? check.displayName).font(.callout)
                        if let fix = check.fix, !fix.isEmpty, check.verdict != .ok {
                            Text(fix).font(.system(.caption, design: .monospaced)).foregroundStyle(.secondary)
                        }
                    }
                    Spacer(minLength: 0)
                }
            }
        }
        .padding(.top, 14)
    }

    /// The same shape as a service block one section up, because it is the same question: what is
    /// installed, what is newer, and one button that closes the gap.
    private var updateBlock: some View {
        let updates = model.appUpdates
        return VStack(alignment: .leading, spacing: 12) {
            DetailRow(label: "Update") { updateVerdict }

            FlowRow(spacing: 10) {
                if let version = updates.availableVersion {
                    PrimaryActionButton(
                        title: updates.phase == .idle ? "Install \(version)" : updates.summary,
                        isHighlighted: updates.installBlockedReason == nil,
                        isEnabled: !updates.phase.isWorking && updates.installBlockedReason == nil
                    ) { afterSheet { updates.install() } }
                }

                Button("Check now") { updates.checkNow() }
                    .disabled(updates.phase.isWorking)

                if let rollback = updates.rollbackVersion {
                    Button("Go back to \(rollback)") { afterSheet { updates.rollBack() } }
                        .disabled(updates.phase.isWorking)
                }

                if updates.phase.isWorking {
                    ProgressView().controlSize(.small)
                }
            }

            if let blocked = updates.installBlockedReason {
                QuietNote(text: blocked)
            }
            if let note = updates.note {
                QuietNote(text: note)
            }
            DisclosureGroup("Update details") { QuietNote(text: cadenceNote) }
        }
    }

    /// Where it looks, how often, and how old the answer on screen is.
    private var cadenceNote: String {
        let repo = model.config.config?.updateRepo ?? ControllerConfig.AppUpdatesConfig.defaultRepo
        let age = AppModel.freshness(of: model.appUpdates.lastChecked)
        return "Releases of \(repo) are checked at startup, on foreground entry after 15 minutes, and every six hours while the app runs. Last check: \(age). Installation verifies the signed release and retains the previous version."

    }

    @ViewBuilder
    private var updateVerdict: some View {
        let updates = model.appUpdates
        if updates.phase.isWorking {
            StatusText(symbol: "arrow.down.circle", text: updates.summary, tint: .orange)
        } else if let version = updates.availableVersion {
            StatusText(symbol: "arrow.down.circle", text: "Version \(version) is available", tint: .orange)
        } else {
            switch updates.check {
            case .upToDate:
                StatusText(symbol: "checkmark.circle", text: "Up to date", tint: .green)
            case .failed:
                StatusText(symbol: "wifi.exclamationmark", text: "Not checked", tint: .orange)
            default:
                Text("not checked yet").foregroundStyle(.secondary)
            }
        }
    }

    // MARK: - Exports

    private func exportDiagnostics() {
        exportingDiagnostics = true
        Task { @MainActor in
            let text = await model.diagnosticsBundle()
            exportProblem = Self.save(text, suggestedName: "legion-control-diagnostics.txt")
            exportingDiagnostics = false
        }
    }

    private func exportHistory() {
        exportProblem = Self.save(model.operations.exportText(), suggestedName: "legion-control-operations.txt")
    }

    /// Writes the text somewhere the user chose. A save panel rather than a fixed path: a
    /// diagnostic bundle is something you attach to a message, not something you go looking for.
    @discardableResult static func save(_ text: String, suggestedName: String) -> String? {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = suggestedName
        panel.canCreateDirectories = true
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        do { try Data(text.utf8).write(to: url, options: .atomic); return nil }
        catch { return error.localizedDescription }
    }

    private static var appVersion: String? {
        guard let short = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String else { return nil }
        if let build = Bundle.main.infoDictionary?["CFBundleVersion"] as? String, build != short {
            return "\(short) (\(build))"
        }
        return short
    }
}
