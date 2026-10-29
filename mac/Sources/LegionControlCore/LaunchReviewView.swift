import SwiftUI

/// The commands from the shared setup this device has not approved, beside the ones it runs
/// meanwhile.
///
/// Every peer may edit the setup, and lineage only shows that a document claims an ancestor, so a
/// command that arrived by replication is shown here before this device runs it with its own key.
/// Nothing else waits: names, addresses, sites and wake helpers have already applied on their own.
/// The sheet stays plain on purpose. It shows exactly what would run, says who the document claims
/// wrote it and that nothing checked that, and offers the answers there are.
struct LaunchReviewView: View {
    let model: AppModel

    @State private var problem: String?

    /// Machines in the order the setup lists them, then whatever is about this device itself.
    /// Grouped by id and titled by name, so two machines that happen to share a name stay apart.
    private func groups(_ entries: [LaunchReviewEntry]) -> [(id: String, title: String, entries: [LaunchReviewEntry])] {
        var groups: [(id: String, title: String, entries: [LaunchReviewEntry])] = []
        for entry in entries {
            let id = entry.location.machineId.map { "machine:" + $0 } ?? "device"
            if let index = groups.firstIndex(where: { $0.id == id }) {
                groups[index].entries.append(entry)
            } else {
                let title = entry.location.machineId.map { id in model.config.config?.machine(id: id)?.name ?? id } ?? "This device"
                groups.append((id, title, [entry]))
            }
        }
        return groups
    }

    var body: some View {
        // Approve all records the rows this pass draws, not whatever the model holds when it is
        // pressed: a document adopted before the sheet redraws would otherwise be approved unseen.
        let shown = model.launchReview
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: "Approve commands from the shared setup")

            Text(LaunchText.provenance(model.config.config?.identity))
                .fixedSize(horizontal: false, vertical: true)
            QuietNote(text: "Approving lets this device run this command over ssh with its own key.")
                .padding(.top, 6)

            ForEach(groups(shown), id: \.id) { group in
                SectionHeading(title: group.title)
                VStack(alignment: .leading, spacing: 22) {
                    ForEach(group.entries) { entry in row(entry) }
                }
            }

            if let problem {
                Text(problem)
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 16)
            }

            FlowRow(spacing: 10) {
                if shown.filter(\.approvable).count > 1 {
                    Button("Approve all") { model.approveLaunch(shown) }
                }
                // The answer that changes nothing is the one Return gives.
                Button("Not now") { model.deferLaunchReview(model.config.hash) }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.top, 24)
        }
    }

    private func row(_ entry: LaunchReviewEntry) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(title(entry)).font(.headline)
                Text(entry.kind == .changed ? "changed" : "new")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer(minLength: 8)
                if entry.approvable {
                    Button("Approve") { model.approveLaunch([entry]) }
                }
                if entry.approved != nil {
                    Button("Revert") { problem = model.revertLaunch(entry) }
                }
            }

            HStack(alignment: .top, spacing: 18) {
                side(title: "Approved here", value: entry.approved, scope: entry.scope)
                side(title: "In the setup now", value: entry.offered, scope: entry.scope)
            }

            if !entry.approvable {
                QuietNote(text: LaunchText.whyNotApprovable(entry))
            }
        }
    }

    private func title(_ entry: LaunchReviewEntry) -> String {
        switch entry.location {
        case .system(let machine, let system): model.config.config?.machine(id: machine)?.system(id: system)?.name ?? system
        case .identityFile: "Identity file"
        case .repo: "Update repository"
        case .local: "Local agent"
        }
    }

    private func side(title: String, value: LaunchValue?, scope: LaunchScope) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            LaunchValueText(value: value, scope: scope)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// What a setup taken from a device's agent would have this device run, before any of it is applied.
///
/// Enrolment approves a whole document a peer wrote, which nothing else does, so every launch value
/// in it is on screen here: each machine and system with its argv and grammar, any identity file,
/// a repository other than the built-in one, and a local agent. A value nothing could approve is
/// marked where it is drawn, because confirming does not approve it either. Cancel applies nothing.
struct LaunchEnrolmentView: View {
    let model: AppModel
    let enrolment: LaunchEnrolment

    @State private var problem: String?

    private var projection: LaunchProjection { enrolment.projection }

    var body: some View {
        let held = Dictionary(enrolment.unapprovable.map { ($0.location, $0) }, uniquingKeysWith: { first, _ in first })
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: "Use the setup from \(enrolment.holder)?")

            Text(LaunchText.provenance(enrolment.identity))
                .fixedSize(horizontal: false, vertical: true)
            QuietNote(text: held.isEmpty
                      ? "Confirming applies this setup and lets this device run each command below over ssh with its own key. Cancel applies nothing."
                      : "Confirming applies this setup and lets this device run each command below over ssh with its own key, except the ones marked as not approved. Cancel applies nothing.")
                .padding(.top, 6)

            ForEach(projection.machines, id: \.id) { machine in
                let named = enrolment.config?.machine(id: machine.id)
                SectionHeading(title: named?.name ?? machine.id)
                VStack(alignment: .leading, spacing: 14) {
                    ForEach(machine.systems, id: \.id) { system in
                        let location = LaunchLocation.system(machine: machine.id, system: system.id)
                        row(named?.system(id: system.id)?.name ?? system.id, system.tuple.map(LaunchValue.tuple),
                            at: location, held: held[location])
                    }
                    if let path = machine.identityFile {
                        let location = LaunchLocation.identityFile(machine: machine.id)
                        row("Identity file", .text(path), at: location, held: held[location])
                    }
                }
            }

            if projection.repo != nil || projection.local != nil {
                SectionHeading(title: "This device")
                VStack(alignment: .leading, spacing: 14) {
                    if let repo = projection.repo {
                        row("Update repository", .text(repo), at: .repo, held: held[.repo])
                    }
                    if let local = projection.local {
                        row("Local agent", .local(agent: local.agent), at: .local, held: held[.local])
                    }
                }
            }

            if let problem {
                Text(problem)
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 16)
            }

            FlowRow(spacing: 10) {
                Button("Confirm") { problem = model.confirmEnrolment() }
                    .buttonStyle(.borderedProminent)
                Button("Cancel") { model.cancelEnrolment() }
                    .keyboardShortcut(.cancelAction)
            }
            .padding(.top, 24)
        }
    }

    /// One launch value, with why confirming leaves it unapproved when nothing could approve it.
    /// Only a system's value can be absent here, and an absent one runs nothing.
    private func row(_ label: String, _ value: LaunchValue?, at location: LaunchLocation,
                     held: LaunchReviewEntry?) -> some View {
        DetailRow(label: label) {
            VStack(alignment: .leading, spacing: 4) {
                LaunchValueText(value: value, scope: location.scope, absent: "no command, so it is not driven")
                if let held {
                    QuietNote(text: LaunchText.whyNotEnrolled(held))
                }
            }
        }
    }
}

/// One launch value as both sheets draw it: an argv one item per line with its grammar under it,
/// or the path or repository, with every character that could hide what it says written out.
private struct LaunchValueText: View {
    let value: LaunchValue?
    let scope: LaunchScope
    var absent: String = "nothing approved here"

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            switch value {
            case .tuple(let tuple):
                ForEach(Array(tuple.agent.enumerated()), id: \.offset) { _, item in
                    Text(LaunchValues.visible(item)).font(.system(.callout, design: .monospaced))
                }
                Text(LaunchText.grammar(tuple.grammar)).font(.caption).foregroundStyle(.secondary)
            case .text(let text):
                Text(LaunchValues.visible(text)).font(.system(.callout, design: .monospaced))
            case .local(let agent):
                Text(agent.map(LaunchValues.visible) ?? "the default path, \(LocalConfig.defaultAgentPath)")
                    .font(.system(.callout, design: .monospaced))
            case nil:
                Text(absent).foregroundStyle(.tertiary)
            }
        }
        .textSelection(.enabled)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// The line that says commands from the shared setup are waiting on this device, and the way to
/// them, for the setup pages. It draws nothing while nothing is waiting and the record is sound.
struct LaunchWaitingNotice: View {
    let model: AppModel

    static func isNeeded(_ model: AppModel) -> Bool {
        !model.launchReview.isEmpty || model.approvals.problem != nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !model.launchReview.isEmpty {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    StatusText(symbol: "exclamationmark.shield", text: sentence, tint: .orange)
                    Button("Review…") { model.reviewLaunch() }
                        .controlSize(.small)
                }
            }
            if let problem = model.approvals.problem {
                QuietNote(text: problem)
            }
        }
    }

    private var sentence: String {
        let count = model.launchReview.count
        return count == 1
            ? "A command from the shared setup is waiting for approval on this device."
            : "\(count) commands from the shared setup are waiting for approval on this device."
    }
}

/// The sentences both sheets share.
enum LaunchText {
    /// Who the document says wrote it. Said as a claim, because it is one: any device that can
    /// publish the setup can put any name here.
    static func provenance(_ identity: ControllerIdentity?) -> String {
        let device = identity?.device.flatMap { $0.isEmpty ? nil : LaunchValues.visible($0) } ?? "an unnamed device"
        let source = identity?.source.flatMap { $0.isEmpty ? nil : " (\(LaunchValues.visible($0)))" } ?? ""
        return "Revision \(identity?.revisionNumber ?? 0), says it was written by \(device)\(source). Not verified."
    }

    /// How a line in this grammar is written, in a word or two.
    static func grammar(_ grammar: String) -> String {
        if grammar.hasPrefix("refused:") {
            return "never sent: " + CommandLineRefusal(String(grammar.dropFirst("refused:".count))).message
        }
        return grammar == CommandGrammar.dispatch.rawValue ? "dispatch, for a restricted key" : grammar
    }

    /// Why a value can only be reverted or left.
    static func whyNotApprovable(_ entry: LaunchReviewEntry) -> String {
        "\(reason(entry)) It can only be reverted or left."
    }

    /// The same, in the enrolment summary: confirming does not approve it, so it is not run, and it
    /// waits in the review afterwards like anything else nothing could approve.
    static func whyNotEnrolled(_ entry: LaunchReviewEntry) -> String {
        "\(reason(entry)) Confirming does not approve it, so this device does not run it."
    }

    /// What about a value keeps anyone from approving it.
    private static func reason(_ entry: LaunchReviewEntry) -> String {
        switch entry.offered {
        case .tuple(let tuple) where tuple.commandGrammar == nil:
            let code = tuple.grammar.hasPrefix("refused:") ? String(tuple.grammar.dropFirst("refused:".count)) : tuple.grammar
            return "This command is never sent anywhere. \(CommandLineRefusal(code).message)"
        case .text where entry.scope == .repo:
            return "This is not a GitHub owner/name."
        default:
            return "It holds a control, invisible or direction-changing character, written out above, which would make what you read differ from what runs."
        }
    }
}
