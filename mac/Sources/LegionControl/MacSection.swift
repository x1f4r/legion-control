import SwiftUI

/// This device's own business: the services that run here rather than over ssh, and Legion Control
/// itself. Nothing in here goes over the network.
struct MacSection: View {
    let model: AppModel
    let mac: MacModel

    // Read once and after every change rather than inside the body: the status comes from launchd,
    // and the body runs again on every poll while this section is open.
    @State private var startsAtLogin = LoginItem.isEnabled
    @State private var needsLoginApproval = LoginItem.needsApproval
    @State private var loginProblem: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: mac.name, note: AppModel.freshness(of: mac.lastChecked))

            if let failure = mac.failure {
                VStack(alignment: .leading, spacing: 6) {
                    Text(failure.message)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let detail = failure.detail, !detail.isEmpty {
                        Text(detail)
                            .font(.system(.caption, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                            .lineLimit(3)
                    }
                }
            } else if mac.services.isEmpty {
                Text("Reading \(mac.name).")
                    .foregroundStyle(.secondary)
            } else {
                QuietNote(text: "Applying an update quits the service on this Mac and starts it again, so anything open in it goes with it. While work is running the update is held back on purpose: this Mac will not be pulled out from under work in progress, and that refusal is the point rather than a failure.")
                    .padding(.bottom, 14)

                Toggle(isOn: Binding(
                    get: { mac.autoUpdate ?? false },
                    set: { mac.setAutoUpdate($0) }
                )) {
                    Text("Apply a waiting update on its own")
                }
                .toggleStyle(.switch)
                .disabled(!mac.isReachable || mac.isWorking)

                ForEach(mac.services) { service in
                    serviceBlock(service)
                }

                if let note = mac.note, !note.isEmpty {
                    Text(note)
                        .font(.callout)
                        .foregroundStyle(mac.noteIsError ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, 14)
                }
            }

            SectionHeading(title: "Legion Control")

            DetailRow(label: "Version") {
                VersionText(value: Self.appVersion, placeholder: "not known")
            }
            .padding(.bottom, 14)

            DetailRow(label: "Config file") {
                Text(model.config.path)
                    .font(.system(.callout, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .padding(.bottom, 14)

            if let problem = model.config.problem {
                QuietNote(text: "The config file was edited into something that could not be read, so the machines above are the ones from before it. \(problem)")
                    .padding(.bottom, 14)
            }

            QuietNote(text: "Closing the window puts Legion Control back in the menu bar. It keeps running there and asks the machines nothing at all until you open it again.")
                .padding(.bottom, 16)

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
                QuietNote(text: loginProblem)
                    .padding(.top, 8)
            } else if needsLoginApproval {
                QuietNote(text: "Waiting for approval in System Settings, under General and then Login Items.")
                    .padding(.top, 8)
            }
        }
        .onAppear {
            // System Settings can turn this off behind our back, so it is read again every time the
            // section is shown rather than trusted from launch.
            startsAtLogin = LoginItem.isEnabled
            needsLoginApproval = LoginItem.needsApproval
        }
    }

    // MARK: - One service

    private func serviceBlock(_ service: ServiceStatus) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: service.displayName)

            VStack(alignment: .leading, spacing: 12) {
                DetailRow(label: "Installed") {
                    VersionText(value: service.installed, placeholder: "not installed")
                }
                DetailRow(label: "Newest") {
                    VersionText(value: service.latest, placeholder: "could not be checked")
                }
                DetailRow(label: "Update waiting") { stagedVerdict(service) }
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

            HStack(spacing: 10) {
                PrimaryActionButton(
                    title: "Update \(service.displayName) now",
                    isHighlighted: mac.updateUnavailableReason(service) == nil,
                    isEnabled: !mac.isWorking && mac.updateUnavailableReason(service) == nil
                ) { mac.requestUpdate(service) }

                if mac.isWorking {
                    ProgressView().controlSize(.small)
                }
            }

            QuietNote(text: updateNote(service))
                .padding(.top, 12)
        }
    }

    // MARK: - Verdicts

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
        if let reason = service.busyReason {
            StatusText(symbol: "circle.dotted", text: reason, tint: .orange)
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

    private static var appVersion: String? {
        guard let short = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String else { return nil }
        if let build = Bundle.main.infoDictionary?["CFBundleVersion"] as? String, build != short {
            return "\(short) (\(build))"
        }
        return short
    }
}
