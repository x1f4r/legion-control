import AppKit
import SwiftUI

/// Legion Control itself: which build is running, updating it, going back, starting at login and
/// exporting diagnostics.
///
/// Its own page rather than part of this Mac's, because none of it is about this Mac. It used to
/// live under the local device's settings, so a setup that drives no agent here had no way to roll
/// back, check for an update or export diagnostics at all.
struct AppSection: View {
    let model: AppModel
    /// The page in the window, or the part of the first-run page that says the same things.
    var isPage = true

    @State private var startsAtLogin = LoginItem.isEnabled
    @State private var needsLoginApproval = LoginItem.needsApproval
    @State private var loginProblem: String?
    @State private var exportingDiagnostics = false
    @State private var exportProblem: String?

    private var updates: AppUpdateModel { model.appUpdates }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if isPage {
                PageHeading(title: "Legion Control", note: DiagnosticsExport.appVersion)
            } else {
                SectionHeading(title: "Legion Control", note: DiagnosticsExport.appVersion)
            }

            updateBlock

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
            .padding(.top, 20)

            if let loginProblem {
                QuietNote(text: loginProblem).padding(.top, 8)
            } else if needsLoginApproval {
                QuietNote(text: "Waiting for approval in System Settings, under General and then Login Items.")
                    .padding(.top, 8)
            }

            FlowRow(spacing: 10) {
                Button("Export all diagnostics") { exportDiagnostics() }.disabled(exportingDiagnostics)
                Button("Export operation history") {
                    exportProblem = DiagnosticsExport.save(model.operations.exportText(),
                                                           suggestedName: "legion-control-operations.txt")
                }
            }
            .padding(.top, 20)
            if exportingDiagnostics {
                Text("Collecting diagnostics…").font(.caption).foregroundStyle(.secondary).padding(.top, 8)
            }
            if let exportProblem {
                Text(exportProblem).foregroundStyle(.orange).textSelection(.enabled).padding(.top, 8)
            }
        }
        .onAppear {
            startsAtLogin = LoginItem.isEnabled
            needsLoginApproval = LoginItem.needsApproval
        }
    }

    /// The same shape as a service block on a machine's page, because it is the same question:
    /// what is installed, what is newer, and one button that closes the gap.
    private var updateBlock: some View {
        VStack(alignment: .leading, spacing: 12) {
            DetailRow(label: "Update") { updateVerdict }

            FlowRow(spacing: 10) {
                if let version = updates.availableVersion {
                    PrimaryActionButton(
                        title: updates.phase == .idle ? "Install \(version)" : updates.summary,
                        isHighlighted: updates.installBlockedReason == nil,
                        isEnabled: !updates.phase.isWorking && updates.installBlockedReason == nil
                    ) { updates.install() }
                }

                Button("Check now") { updates.checkNow() }
                    .disabled(updates.phase.isWorking)

                if let previous = updates.rollbackVersion {
                    Button("Go back to \(previous)…") { model.requestRollback() }
                        .disabled(updates.phase.isWorking || updates.swapBlockedReason != nil)
                    Button("Discard \(previous)…") { model.requestDiscardPreviousBuild() }
                        .disabled(updates.phase.isWorking || updates.swapBlockedReason != nil)
                        .help("Delete the build kept from the last update.")
                }

                if updates.phase.isWorking {
                    ProgressView().controlSize(.small)
                }
            }

            if let blocked = updates.installBlockedReason {
                QuietNote(text: blocked)
            } else if updates.rollbackVersion != nil, let blocked = updates.swapBlockedReason {
                QuietNote(text: blocked)
            }
            if let note = updates.note {
                QuietNote(text: note)
            }
            if let warning = updates.repositoryWarning {
                QuietNote(text: warning)
            }
            DisclosureGroup("Update details") { QuietNote(text: cadenceNote) }
        }
    }

    /// Where it looks, how often, and how old the answer on screen is.
    private var cadenceNote: String {
        let age = AppModel.freshness(of: updates.lastChecked)
        var note = "Releases of \(updates.repository) are checked at startup, on foreground entry after 15 minutes, and every six hours while the app runs. Last check: \(age). Installation verifies the signed release and retains the previous version."
        if let previous = updates.rollbackVersion {
            note += " \(previous) is kept beside this build until it is discarded."
        }
        return note
    }

    @ViewBuilder
    private var updateVerdict: some View {
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

    private func exportDiagnostics() {
        Task { @MainActor in
            exportProblem = await DiagnosticsExport.exportAll(model) { exportingDiagnostics = $0 }
        }
    }
}

/// Saving what the app knows, from the window or from the menu bar panel.
@MainActor
enum DiagnosticsExport {
    /// Everything a bug report needs, written where the user chooses. The place is asked for first
    /// and the reading done after: the reading goes to every machine and can take a while, and a
    /// save panel that appears out of nowhere half a minute after the click is easy to mistake for
    /// something else. `collecting` hears when that reading starts and stops, so nothing says it is
    /// collecting behind the save panel or after a cancel. Nil when it was written or the user
    /// cancelled, else the sentence to show.
    static func exportAll(_ model: AppModel, collecting: (Bool) -> Void) async -> String? {
        guard let url = destination(suggestedName: "legion-control-diagnostics.txt") else { return nil }
        collecting(true)
        defer { collecting(false) }
        let text = await model.diagnosticsBundle()
        return write(text, to: url)
    }

    /// Writes the text somewhere the user chose. A save panel rather than a fixed path: a
    /// diagnostic bundle is something you attach to a message, not something you go looking for.
    @discardableResult static func save(_ text: String, suggestedName: String) -> String? {
        guard let url = destination(suggestedName: suggestedName) else { return nil }
        return write(text, to: url)
    }

    private static func destination(suggestedName: String) -> URL? {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = suggestedName
        panel.canCreateDirectories = true
        guard panel.runModal() == .OK else { return nil }
        return panel.url
    }

    private static func write(_ text: String, to url: URL) -> String? {
        do { try Data(text.utf8).write(to: url, options: .atomic); return nil }
        catch { return error.localizedDescription }
    }

    /// The running build, with its build number when that says something the version does not.
    static var appVersion: String? {
        guard let short = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String else { return nil }
        if let build = Bundle.main.infoDictionary?["CFBundleVersion"] as? String, build != short {
            return "\(short) (\(build))"
        }
        return short
    }
}
