import SwiftUI

/// The machine itself: which system is running on it, what can be done to the box rather than to the
/// software on it, what is happening there now, and what is wrong with it.
///
/// This is the landing section because it answers the question the window exists for. Nothing about
/// any one service appears here.
struct MachineSection: View {
    let app: AppModel
    let model: MachineModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: model.name, note: AppModel.freshness(of: model.lastChecked))

            VStack(alignment: .leading, spacing: 12) {
                DetailRow(label: "Running now") { runningNow }
                DetailRow(label: "Machine") {
                    Text(model.status?.hostname ?? "not known")
                        .foregroundStyle(model.status?.hostname == nil ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
                }
                DetailRow(label: "Control agent") { agentVerdict }
                DetailRow(label: "Setup") { setupVerdict }
                DetailRow(label: "On the network") {
                    Text(network)
                        .font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let agentConfig = model.status?.config, agentConfig.ok == false || agentConfig.source != "file" {
                    DetailRow(label: "Its own config") {
                        StatusText(symbol: agentConfig.ok == false ? "exclamationmark.triangle" : "info.circle",
                                   text: agentConfig.summary,
                                   tint: agentConfig.ok == false ? .orange : .secondary)
                    }
                }
            }

            hostKeyBlock
            unresolvedBlock
            operationsBlock
            powerSection
            actionsSection
            notesSection
            diagnosticsSection
            MetricsRows(metrics: model.status?.metrics)
        }
    }

    // MARK: - What is running

    @ViewBuilder
    private var runningNow: some View {
        if let reboot = model.rebootInProgress, !model.isAwake {
            StatusText(symbol: "arrow.triangle.2.circlepath", text: "Restarting into \(reboot.target.name)", tint: .orange)
        } else if let system = model.currentSystem {
            // The one thing you open this window to find out, so it gets weight the other rows do not.
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: system.symbolName)
                    .foregroundStyle(.green)
                    .imageScale(.large)
                Text(system.name)
                    .font(.title3.weight(.semibold))
            }
            .contentTransition(.opacity)
            .animation(.easeOut(duration: 0.2), value: system)
        } else if case .reachableWithoutAgent(let system) = model.link {
            StatusText(symbol: system.symbolName, text: "\(system.name), but the control agent is not installed", tint: .orange)
        } else if case .offline(let failure) = model.link {
            StatusText(symbol: failure.meansAsleepOrOff ? "moon.zzz" : "exclamationmark.triangle",
                       text: failure.meansAsleepOrOff ? "Asleep or unreachable" : failure.message(machine: model.name),
                       tint: .orange)
        } else {
            StatusText(symbol: "ellipsis", text: "Checking")
        }
    }

    @ViewBuilder
    private var agentVerdict: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            VersionText(value: model.status?.version, placeholder: "not reachable")
            if model.status != nil {
                Text(model.dialect.description)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            if model.status?.isRestrictedSession == true {
                Text("restricted key")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// Whether this machine is holding the same setup this device is.
    ///
    /// Every device is a peer, so this row has three interesting states rather than one: in step,
    /// moving in one direction or the other by itself, or stopped because the two have genuinely
    /// diverged and only a person can say what the setup should be.
    @ViewBuilder
    private var setupVerdict: some View {
        switch model.setupSharing {
        case .upToDate:
            StatusText(symbol: "checkmark.circle", text: setupRevisionText, tint: .green)
        case .justShared:
            StatusText(symbol: "checkmark.circle", text: "shared just now", tint: .green)
        case .publishing:
            StatusText(symbol: "arrow.up.circle", text: "sending this device's newer revision", tint: .secondary)
        case .fetching:
            StatusText(symbol: "arrow.down.circle", text: "taking this machine's newer revision", tint: .secondary)
        case .unsupported:
            Text("not shared: this agent cannot carry the setup").foregroundStyle(.secondary)
        case .failed(let sentence):
            StatusText(symbol: "exclamationmark.triangle", text: sentence, tint: .orange)
        case .conflict(let sentence):
            VStack(alignment: .leading, spacing: 8) {
                StatusText(symbol: "exclamationmark.triangle", text: sentence, tint: .orange)
                Button("Compare and merge") { model.openSetupDivergence() }
                    .controlSize(.small)
            }
        case .unknown:
            Text("not known").foregroundStyle(.secondary)
        }
    }

    private var setupRevisionText: String {
        guard let identity = app.config.config?.identity else { return "shared, up to date" }
        return "revision \(identity.revisionNumber), same on both"
    }

    /// Where this machine is reached, and what a wake packet would be aimed at.
    private var network: String {
        var parts: [String] = []
        if let route = model.lastRoute { parts.append(route.label) }
        else if let target = model.machine.sshTarget { parts.append(target.display) }
        if let site = model.machine.site.flatMap({ app.config.config?.site(id: $0) }) { parts.append(site.name) }
        if let wake = model.machine.wake { parts.append(wake.mac) }
        return parts.isEmpty ? "not known" : parts.joined(separator: ", ")
    }

    // MARK: - Host keys

    /// An unknown host key gets a decision, never a silent accept.
    @ViewBuilder
    private var hostKeyBlock: some View {
        if let prompt = model.hostKeyPrompt {
            SectionHeading(title: "Host key")

            QuietNote(text: """
                SSH refused an unapproved key. Verify every displayed fingerprint independently \
                before assigning it to an operating system. Existing pins are retained.
                """)
                .padding(.bottom, 12)

            if prompt.fingerprints.isEmpty {
                Button("Read the fingerprint") { model.readHostKeyFingerprint() }
            } else {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(prompt.fingerprints, id: \.self) { line in
                        Text(line)
                            .font(.system(.caption, design: .monospaced))
                            .textSelection(.enabled)
                    }
                }
                .padding(.bottom, 12)

                if let context = prompt.context {
                    Text(context.address).font(.system(.caption, design: .monospaced))
                }
                if prompt.initialSetup {
                    Text("Local trust configuration: confirm which operating systems use this address.")
                    ForEach(prompt.groups) { group in
                        Toggle(group.name, isOn: Binding(
                            get: { model.confirmedHostSystems.contains(group.id) },
                            set: { if $0 { model.confirmedHostSystems.insert(group.id) } else { model.confirmedHostSystems.remove(group.id) } }
                        ))
                    }
                    ForEach(prompt.unassigned) { key in
                        Text("Existing pin: \(key.fingerprint)").font(.system(.caption, design: .monospaced))
                        Picker("Existing pin belongs to", selection: Binding(
                            get: { model.legacyHostAssignments[key.id] ?? "" },
                            set: { model.legacyHostAssignments[key.id] = $0 }
                        )) {
                            Text("Choose operating system").tag("")
                            ForEach(prompt.groups.filter { model.confirmedHostSystems.contains($0.id) }) { group in
                                Text(group.name).tag(group.id)
                            }
                        }
                    }
                    Toggle("I confirm this local operating-system list and the existing fingerprint assignments", isOn: Binding(
                        get: { model.hostSystemsConfirmed }, set: { model.hostSystemsConfirmed = $0 }
                    ))
                }
                if prompt.canApprove {
                    Picker("Offered keys belong to", selection: Binding(
                        get: { model.selectedHostSystem }, set: { model.selectedHostSystem = $0 }
                    )) {
                        ForEach(prompt.groups.filter {
                            prompt.initialSetup ? model.confirmedHostSystems.contains($0.id) : ($0.keys.isEmpty || $0.keys == prompt.offered)
                        }) { group in Text(group.name).tag(group.id) }
                    }
                }
                HStack(spacing: 10) {
                    if prompt.canApprove {
                        Button("Approve displayed keys") { model.trustHostKey() }
                            .buttonStyle(.borderedProminent)
                            .disabled(prompt.initialSetup && !model.hostSystemsConfirmed)
                    }
                    Button("Not now") { model.dismissHostKeyPrompt() }
                }
            }

            if let problem = prompt.problem {
                QuietNote(text: problem).padding(.top, 10)
            }
        }
    }

    // MARK: - Outcomes nobody knows

    @ViewBuilder
    private var unresolvedBlock: some View {
        let unresolved = model.unresolvedOperations
        if !unresolved.isEmpty {
            SectionHeading(title: "Outcome not known")

            QuietNote(text: """
                These were sent and the answer never came back. The machine may have done them, or \
                may not. Nothing is assumed either way; the agent keeps its own record and this app \
                asks for it whenever the machine answers again.
                """)
                .padding(.bottom, 12)

            VStack(alignment: .leading, spacing: 10) {
                ForEach(unresolved) { record in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Image(systemName: "questionmark.circle").foregroundStyle(.orange)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(record.title).font(.callout)
                            Text(record.summary)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer(minLength: 8)
                        Button("Check now") { model.reconcileNow() }
                            .controlSize(.small)
                        Button("Dismiss") { model.dismissUnknownOutcome(record.id) }
                            .controlSize(.small)
                    }
                }
            }
        }
    }

    // MARK: - Operations

    @ViewBuilder
    private var operationsBlock: some View {
        let running = model.runningOperations
        let queued = model.queuedOperations
        let recent = model.recentOperations
        if !running.isEmpty || !queued.isEmpty || !recent.isEmpty || model.watchedRecord != nil {
            if !running.isEmpty || !queued.isEmpty || model.watchedRecord?.state == .running {
                SectionHeading(title: "Operations")
            }

            VStack(alignment: .leading, spacing: 12) {
                if let watched = model.watchedRecord, watched.state == .running {
                    VStack(alignment: .leading, spacing: 4) {
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text(watched.title).font(.callout)
                        }
                        if let phase = watched.agentPhase {
                            Text([phase, watched.agentProgress].compactMap { $0 }.joined(separator: " — "))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }

                ForEach(running) { operation in
                    operationRow(operation, symbol: "circle.dotted", tint: .orange)
                }

                ForEach(queued) { operation in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Image(systemName: "clock.arrow.circlepath").foregroundStyle(.secondary)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(operation.displayName).font(.callout)
                            Text(expiryText(operation))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 8)
                        Button("Cancel") { model.cancelQueued(operation) }
                            .controlSize(.small)
                            .disabled(model.isWorking)
                    }
                }

                if !recent.isEmpty {
                    DisclosureGroup("Recent changes") {
                        VStack(alignment: .leading, spacing: 12) {
                            ForEach(recent) { operation in
                                operationRow(operation,
                                             symbol: operation.result?.ok == false ? "exclamationmark.triangle" : "checkmark.circle",
                                             tint: operation.result?.ok == false ? .orange : .secondary)
                            }
                        }.padding(.top, 8)
                    }.padding(.top, 14)
                }
            }
        }
    }

    private func operationRow(_ operation: AgentOperation, symbol: String, tint: Color) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: symbol).foregroundStyle(tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(operation.displayName).font(.callout)
                Text(operationDetail(operation))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                DisclosureGroup("Details") {
                    Text(operationFullDetail(operation))
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }.font(.caption)
            }
            Spacer(minLength: 0)
        }
    }

    private func operationDetail(_ operation: AgentOperation) -> String {
        var parts: [String] = []
        if let action = operation.resolvedAction { parts.append(action) }
        if let reason = operation.resolvedReason {
            parts.append(reason.sentence(subject: operation.service ?? operation.actionId ?? model.name,
                                         message: operation.result?.message))
        } else if let message = operation.result?.message {
            parts.append(message)
        }
        if let when = ISO8601DateFormatter.lenient.date(from: operation.updatedAt ?? operation.requestedAt) {
            parts.append(when.formatted(date: .omitted, time: .shortened))
        }
        return parts.joined(separator: " · ")
    }

    private func operationFullDetail(_ operation: AgentOperation) -> String {
        var parts = ["Operation: \(operation.id)", "State: \(operation.state ?? "unknown")"]
        if let phase = operation.phase { parts.append("Phase: \(phase)") }
        if let note = operation.progress?.description { parts.append(note) }
        if let output = operation.result?.output { parts.append(output) }
        parts.append(operationDetail(operation))
        return parts.joined(separator: "\n")
    }

    private func expiryText(_ operation: AgentOperation) -> String {
        guard let expires = ISO8601DateFormatter.lenient.date(from: operation.expiresAt) else {
            return "waiting for the machine to be idle"
        }
        return "waiting for the machine to be idle · expires \(expires.relativeDescription())"
    }

    // MARK: - Power

    private var powerSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "Power")

            FlowRow(spacing: 10) {
                if model.canWake {
                    Button(model.isWaking ? "Waking" : "Wake") { model.wake() }
                        .disabled(model.isWorking || model.isAwake || model.wakePlan.isEmpty)
                    ForEach(model.machine.systems) { system in
                        if !model.isAwake {
                            Button("Wake into \(system.name)") { model.wake(into: system) }
                                .disabled(model.isWorking || model.wakePlan.isEmpty)
                        }
                    }
                }

                Button("Sleep") { model.requestSleep() }
                    .disabled(model.isWorking || model.commandableSystem == nil)

                ForEach(model.bootTargets) { target in
                    Button("Boot into \(target.name)") { model.requestBoot(into: target) }
                        .disabled(model.isWorking || model.commandableSystem == nil)
                }
            }

            if !powerNote.isEmpty { QuietNote(text: powerNote).padding(.top, 12) }

            wakePathBlock
        }
    }

    /// What waking this machine would actually do from here, and what stands in the way.
    ///
    /// Written out rather than hidden behind a disabled button, because "the Wake button does
    /// nothing" is the least useful thing an app can say about a machine in another building.
    @ViewBuilder
    private var wakePathBlock: some View {
        if model.canWake, !model.isAwake {
            let plan = model.wakePlan
            VStack(alignment: .leading, spacing: 6) {
                Text(plan.placement.description(sites: app.sites))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                ForEach(Array(plan.steps.enumerated()), id: \.offset) { index, step in
                    switch step {
                    case .direct(let broadcasts, _):
                        Text("\(index + 1). Send the packet from this device to \(broadcasts.joined(separator: ", ")).")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    case .helper(let helper, let machineName):
                        Text("\(index + 1). Ask \(machineName) to run \(helper.action).")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }

                ForEach(plan.unavailable) { unavailable in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("\(unavailable.what): \(unavailable.reason)")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        if let first = unavailable.wakeableFirst,
                           let helper = helperModel(named: first) {
                            Button("Wake \(first) first") { helper.wake() }
                                .controlSize(.small)
                        }
                    }
                }
            }
            .padding(.top, 10)
        }
    }

    private func helperModel(named name: String) -> MachineModel? {
        app.machines.first { $0.name == name }
    }

    private var powerNote: String {
        if !model.isAwake {
            if model.canWake { return "" }
            return "\(model.name) is asleep or unreachable, and has no wake address configured."
        }
        if model.commandableSystem == nil {
            return model.canWake
                ? "The system is awake but the control agent is not answering, so only Wake is available here."
                : "The system is awake but the control agent is not answering, so there is nothing to press."
        }
        return ""
    }

    // MARK: - Actions

    @ViewBuilder
    private var actionsSection: some View {
        if !model.actions.isEmpty {
            SectionHeading(title: "Actions")

            // Wrapped rather than laid out in one row: an agent is free to offer as many of these as
            // it likes, and a row that runs off the edge of the pane is a button you cannot press.
            FlowRow(spacing: 10) {
                ForEach(model.actions) { action in
                    Button(action.displayName) { model.requestAction(action) }
                        .disabled(model.isWorking || model.commandableSystem == nil)
                }
            }

        }
    }

    // MARK: - What could not be read

    @ViewBuilder
    private var notesSection: some View {
        if !model.notes.isEmpty {
            SectionHeading(title: "Could not be read")

            VStack(alignment: .leading, spacing: 6) {
                ForEach(model.notes, id: \.self) { note in
                    Text(note)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    // MARK: - Diagnostics

    @ViewBuilder
    private var diagnosticsSection: some View {
        SectionHeading(title: "Diagnostics")
        DiagnosticActions(enabled: !model.isWorking && model.dialect.supportsDoctor,
                          check: { model.runDoctor() }, deepCheck: { model.runDoctor(deep: true) },
                          log: { await model.fetchLog() }, bundle: { await model.fetchBundle() })
            .padding(.bottom, 12)

        if model.needsAgentUpgrade {
            QuietNote(text: "Agent upgrade required for operations, policy and queue controls.")
                .padding(.bottom, 12)
        }

        FlowRow(spacing: 10) {
            ServiceSetupButton(enabled: !model.isWorking && model.dialect.supportsDoctor,
                               restricted: model.commandableSystem?.isRestricted == true || model.status?.isRestrictedSession == true,
                               send: { try await model.serviceConfig($0) })
            if AgentBundle.shared != nil {
                Button("Install agent \(AgentBundle.shared?.version ?? "")") { model.installBundledAgent() }
                    .disabled(model.isWorking || model.status?.ok != true || model.commandableSystem?.isRestricted == true || model.status?.isRestrictedSession == true)
            } else {
                Button("Install agent") { }
                    .disabled(true)
                    .help(AgentBundle.unavailableReason)
            }
        }

        if AgentBundle.shared == nil {
            QuietNote(text: AgentBundle.unavailableReason).padding(.top, 12)
        }

        if let doctor = model.doctor {
            doctorResults(doctor)
        }
    }

    private func doctorResults(_ doctor: AgentDoctorResult) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            if let ranAt = model.doctorRanAt {
                Text("Checked \(ranAt.formatted(date: .abbreviated, time: .shortened)).")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            ForEach(doctor.checks ?? []) { check in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: symbol(for: check.verdict))
                        .foregroundStyle(tint(for: check.verdict))
                    VStack(alignment: .leading, spacing: 2) {
                        Text(check.summary ?? check.displayName).font(.callout)
                        if let detail = check.detail, !detail.isEmpty {
                            Text(detail)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        if let fix = check.fix, !fix.isEmpty, check.verdict != .ok {
                            Text(fix)
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(.secondary)
                                .textSelection(.enabled)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                    Spacer(minLength: 0)
                }
            }
        }
        .padding(.top, 14)
    }

    private func symbol(for verdict: AgentCheck.Verdict) -> String {
        switch verdict {
        case .ok: "checkmark.circle"
        case .warning: "exclamationmark.circle"
        case .failed: "exclamationmark.triangle"
        case .unknown: "questionmark.circle"
        }
    }

    private func tint(for verdict: AgentCheck.Verdict) -> Color {
        switch verdict {
        case .ok: .green
        case .warning: .orange
        case .failed: .red
        case .unknown: .secondary
        }
    }
}

/// A row of controls that wraps onto the next line instead of running off the edge.
struct FlowRow: Layout {
    var spacing: CGFloat = 10

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        let rows = layout(subviews: subviews, width: width)
        let height = rows.last.map { $0.y + $0.height } ?? 0
        return CGSize(width: proposal.width ?? rows.map { $0.width }.max() ?? 0, height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for row in layout(subviews: subviews, width: bounds.width) {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(
                    at: CGPoint(x: x, y: bounds.minY + row.y),
                    proposal: ProposedViewSize(size)
                )
                x += size.width + spacing
            }
        }
    }

    private struct Row {
        var indices: [Int] = []
        var y: CGFloat = 0
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    private func layout(subviews: Subviews, width: CGFloat) -> [Row] {
        var rows: [Row] = []
        var current = Row()
        var y: CGFloat = 0
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.unspecified)
            let next = current.width.isZero ? size.width : current.width + spacing + size.width
            if !current.indices.isEmpty, next > width {
                rows.append(current)
                y += current.height + spacing
                current = Row(indices: [], y: y, width: 0, height: 0)
            }
            current.indices.append(index)
            current.width = current.width.isZero ? size.width : current.width + spacing + size.width
            current.height = max(current.height, size.height)
        }
        if !current.indices.isEmpty { rows.append(current) }
        return rows
    }
}
