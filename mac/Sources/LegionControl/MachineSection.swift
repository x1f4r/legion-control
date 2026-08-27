import SwiftUI

/// The machine itself: which system is running on it, and the things you can do to the box rather
/// than to the software on it.
///
/// This is the landing section because it answers the question the window exists for. Nothing about
/// any one service appears here.
struct MachineSection: View {
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
                DetailRow(label: "Control agent") {
                    VersionText(value: model.status?.agentVersion, placeholder: "not reachable")
                }
                DetailRow(label: "Setup") { setupVerdict }
                DetailRow(label: "On the network") {
                    Text(network)
                        .font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            SectionHeading(title: "Power")

            HStack(spacing: 10) {
                if model.canWake {
                    Button("Wake") { model.wake() }
                        .disabled(model.isWorking || model.isAwake)
                }

                Button("Sleep") { model.requestSleep() }
                    .disabled(model.isWorking || model.commandableSystem == nil)

                ForEach(model.bootTargets) { target in
                    Button("Boot into \(target.name)") { model.requestBoot(into: target) }
                        .disabled(model.isWorking || model.commandableSystem == nil)
                }
            }

            QuietNote(text: powerNote)
                .padding(.top, 12)

            if !model.actions.isEmpty {
                SectionHeading(title: "Actions")

                // Wrapped rather than laid out in one row: an agent is free to offer as many of
                // these as it likes, and a row that runs off the edge of the pane is a button you
                // cannot press.
                FlowRow(spacing: 10) {
                    ForEach(model.actions) { action in
                        Button(action.displayName) { model.requestAction(action) }
                            .disabled(model.isWorking || model.commandableSystem == nil)
                    }
                }

                QuietNote(text: "These come from the machine's own config. Each one runs there and reports back.")
                    .padding(.top, 12)
            }
        }
    }

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
            // The name swaps to another system after a reboot, and a hard cut there reads as a
            // glitch rather than as news.
            .contentTransition(.opacity)
            .animation(.easeOut(duration: 0.2), value: system)
        } else if case .reachableWithoutAgent(let system) = model.link {
            StatusText(symbol: system.symbolName, text: "\(system.name), but the control agent is not installed", tint: .orange)
        } else if case .offline = model.link {
            StatusText(symbol: "moon.zzz", text: "Asleep or unreachable", tint: .orange)
        } else {
            StatusText(symbol: "ellipsis", text: "Checking")
        }
    }

    /// Whether this machine is holding the same setup this Mac is. It is given a copy whenever it
    /// is not, without being asked and without anyone being told, so the only news here is that it
    /// could not be.
    @ViewBuilder
    private var setupVerdict: some View {
        switch model.setupSharing {
        case .upToDate:
            StatusText(symbol: "checkmark.circle", text: "shared, up to date", tint: .green)
        case .justShared:
            StatusText(symbol: "checkmark.circle", text: "shared just now", tint: .green)
        case .unsupported:
            Text("not shared: agent too old").foregroundStyle(.secondary)
        case .failed(let sentence):
            StatusText(symbol: "exclamationmark.triangle", text: sentence, tint: .orange)
        case .unknown:
            Text("not known").foregroundStyle(.secondary)
        }
    }

    /// Where this machine is reached, and what the wake packet is aimed at when there is one.
    private var network: String {
        var parts: [String] = []
        if let target = model.machine.sshTarget { parts.append(target.display) }
        if let wake = model.machine.wake { parts.append(wake.mac) }
        return parts.isEmpty ? "not known" : parts.joined(separator: ", ")
    }

    private var powerNote: String {
        if !model.isAwake {
            if let probe = model.machine.wake?.probe {
                return "\(model.name) is asleep. Wake sends the magic packet and waits for it to answer on port \(probe.probePort)."
            }
            if model.canWake {
                return "\(model.name) is asleep. Wake sends the magic packet."
            }
            return "\(model.name) is asleep or unreachable, and has no wake address configured."
        }
        if model.commandableSystem == nil {
            return model.canWake
                ? "The system is awake but the control agent is not answering, so only Wake is available here."
                : "The system is awake but the control agent is not answering, so there is nothing to press."
        }
        let wake = model.canWake
            ? "Sleep suspends to memory, and Wake on LAN stays armed, so Wake brings it back. "
            : "Sleep suspends to memory. "
        return wake + "Sleeping and switching systems are both held back while work is running, unless you say otherwise."
    }
}

/// A row of controls that wraps onto the next line instead of running off the edge.
///
/// Every other row of buttons in this app has a fixed handful of members and needs nothing like
/// this. The actions do not: they are whatever the machine's own config lists.
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
