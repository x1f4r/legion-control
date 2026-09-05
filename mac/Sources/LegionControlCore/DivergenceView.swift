import SwiftUI

/// What to do when two devices have both changed the setup since they last agreed.
///
/// This is the screen the whole lineage mechanism exists to reach. Everything that can be settled
/// from the evidence already has been: a push that fast-forwards happens on its own, a fetch that
/// fast-forwards happens on its own, and only a genuine fork gets here. So the job of this view is
/// narrow and it should stay narrow — show what each side did, say which entries actually clash, and
/// let a person settle those.
struct DivergenceView: View {
    let model: AppModel
    let divergence: MachineModel.SetupDivergence

    @State private var choices: [String: SetupMerge.Choice] = [:]
    @State private var problem: String?

    private var mineIdentity: ControllerIdentity { divergence.mine.identity }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: divergence.isDifferentSetup ? "A different setup" : "The setup has forked",
                        note: divergence.machineName)

            Text(explanation)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.bottom, 18)

            VStack(alignment: .leading, spacing: 12) {
                DetailRow(label: "This device") {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("revision \(mineIdentity.revisionNumber)")
                        Text(mineIdentity.authorDescription).font(.caption).foregroundStyle(.secondary)
                    }
                }
                DetailRow(label: divergence.machineName) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("revision \(divergence.theirsIdentity.revisionNumber)")
                        Text(divergence.theirsIdentity.authorDescription).font(.caption).foregroundStyle(.secondary)
                    }
                }
                DetailRow(label: "Common ancestor") {
                    Text(divergence.baseBytes == nil
                         ? "not kept on this device, so every difference needs a decision"
                         : "found, so only the entries both sides changed need a decision")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            if divergence.differences.isEmpty {
                SectionHeading(title: "Differences")
                Text("The two documents differ but no entry this app compares has changed. That usually means only formatting or an unknown key moved.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                differencesSection
            }

            SectionHeading(title: "Settle it")

            FlowRow(spacing: 10) {
                if divergence.isDifferentSetup {
                    Button("Replace its copy with mine") { model.replaceSetupOnPeer(divergence) }
                        .buttonStyle(.borderedProminent)
                } else {
                    Button("Merge") { apply(choices) }
                        .buttonStyle(.borderedProminent)
                    Button("Keep mine") { problem = model.keepMine(divergence) }
                }
                Button("Take theirs") { problem = model.takeTheirs(divergence) }
                Button("Not now") { model.divergence = nil }
            }

            QuietNote(text: divergence.isDifferentSetup
                ? "Replacing is the explicit decision to put this setup id on that peer. Taking theirs keeps this device's current document in the revision history first."
                : "Merging and \"keep mine\" both produce a new revision that descends from both sides, so every machine accepts it without anything being overwritten. \"Take theirs\" keeps the version it replaces in the revision history.")
                .padding(.top, 12)

            if let problem {
                Text(problem)
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 12)
            }
        }
    }

    private var explanation: String {
        if divergence.isDifferentSetup {
            return """
                \(divergence.machineName) is carrying a setup with a different id, not an older copy \
                of this one. Nothing has been sent in either direction. Choosing here decides which \
                setup this fleet is on.
                """
        }
        return """
            Both this device and \(divergence.machineName) have changed the setup since they last \
            agreed, so neither is simply newer than the other and nothing has been sent in either \
            direction. Below is what each side did.
            """
    }

    private var differencesSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "Differences")

            let needing = divergence.differences.filter(\.needsDecision)
            if needing.isEmpty {
                QuietNote(text: "Each of these was changed on one side only, so a merge takes that side. Nothing here needs a decision.")
                    .padding(.bottom, 12)
            } else {
                QuietNote(text: "\(needing.count) of these were changed on both sides. Those are the ones to choose between; the rest merge on their own.")
                    .padding(.bottom, 12)
            }

            VStack(alignment: .leading, spacing: 18) {
                ForEach(divergence.differences) { difference in
                    row(difference)
                }
            }
        }
    }

    private func row(_ difference: SetupMerge.Difference) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(difference.label)
                    .font(.headline)
                Text(kindDescription(difference.kind))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer(minLength: 8)
                if difference.needsDecision {
                    Picker("", selection: Binding(
                        get: { choices[difference.id] ?? difference.defaultChoice },
                        set: { choices[difference.id] = $0 }
                    )) {
                        Text("Mine").tag(SetupMerge.Choice.mine)
                        Text(divergence.machineName).tag(SetupMerge.Choice.theirs)
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                }
            }

            HStack(alignment: .top, spacing: 18) {
                side(title: "This device", value: difference.mine)
                side(title: divergence.machineName, value: difference.theirs)
            }
        }
    }

    private func side(title: String, value: String?) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(value ?? "not present")
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(value == nil ? AnyShapeStyle(.tertiary) : AnyShapeStyle(.secondary))
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func kindDescription(_ kind: SetupMerge.Difference.Kind) -> String {
        switch kind {
        case .onlyMineChanged: "changed here"
        case .onlyTheirsChanged: "changed on \(divergence.machineName)"
        case .bothChanged: "changed on both"
        case .unknownBase: "no common ancestor to compare against"
        }
    }

    private func apply(_ choices: [String: SetupMerge.Choice]) {
        problem = model.resolveDivergence(divergence, choices: choices)
    }
}
