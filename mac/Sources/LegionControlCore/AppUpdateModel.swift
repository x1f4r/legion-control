import AppKit
import Foundation
import Observation

/// Updating Legion Control itself: what the last look at the releases found, and the one button
/// that acts on it.
///
/// This lives on the app rather than in the section that draws it, and that is not tidiness. The
/// window's detail pane is rebuilt every time the sidebar selection changes, and the menu bar panel
/// is rebuilt every time it opens. State held in either of them would mean a fresh request to GitHub
/// on every click, and an install losing track of itself by being navigated away from.
///
/// The cadence is the same rule the rest of the app lives by: nothing runs while nothing is open.
/// There is no timer here. A check is asked for when a viewer appears, it is skipped when the last
/// one is still fresh, and the answer is written down so a relaunch inside the same window costs
/// nothing at all.
@MainActor
@Observable
final class AppUpdateModel {

    /// What the update machinery is doing, which is nothing almost all of the time.
    enum Phase: Equatable {
        case idle
        case checking
        case downloading
        case installing

        var isWorking: Bool { self != .idle }
    }

    private(set) var check: AppUpdates.Check?
    private(set) var phase: Phase = .idle
    private(set) var lastChecked: Date?
    /// Why the last install stopped, as a sentence. Separate from the check: a failed install says
    /// nothing about whether the release is still there to try again.
    private(set) var failure: String?

    /// Where to look. Read on every check rather than once, because the config file that names it
    /// is edited while the app is open and the next check has to go to the new address.
    @ObservationIgnored var repo: @MainActor () -> String = { ControllerConfig.AppUpdatesConfig.defaultRepo }
    /// Where an install is written down, so an update that fails halfway leaves a record.
    @ObservationIgnored var operations: OperationStore?
    /// The bundle this app is running out of. Injected so a test can stage into a directory of its
    /// own and never anywhere near a live app.
    @ObservationIgnored var bundleURL: URL = Bundle.main.bundleURL
    /// Where the launch marker lives.
    @ObservationIgnored var supportDirectory: URL = AppPaths.support
    /// Set in tests so nothing is ever actually swapped or relaunched.
    @ObservationIgnored var performSwap: @MainActor (AppUpdates.Staged, URL, URL) throws -> Void = {
        try AppUpdates.startSwap(staged: $0, current: $1, marker: $2)
    }
    @ObservationIgnored var quit: @MainActor () -> Void = { NSApp.terminate(nil) }

    /// How long an answer stays good for. Releases are cut by hand, a few times a year; asking more
    /// often than this would be asking a question whose answer cannot have changed.
    private static let freshFor: TimeInterval = 6 * 60 * 60
    /// How long a failed look stays good for. Much shorter, because the usual reason for one is that
    /// the laptop had no network a moment ago, and that does change.
    private static let retryFailureAfter: TimeInterval = 10 * 60

    private static let storageKey = "appUpdateLastCheck"

    init() { restore() }

    // MARK: - What the screen reads

    var installedVersion: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    }

    /// The version waiting to be installed, or nil when there is none.
    var availableVersion: String? {
        guard case .available(let release) = check else { return nil }
        return release.version
    }

    /// The first paragraph of the release notes, when there is a release and it carried any.
    var releaseNotes: String? {
        guard case .available(let release) = check, !release.notes.isEmpty else { return nil }
        return release.notes
    }

    /// The one line the row shows: what the last look found, or what is happening right now.
    var summary: String {
        switch phase {
        case .checking: return "Looking for a newer release"
        case .downloading: return "Downloading…"
        case .installing: return "Installing…"
        case .idle: break
        }
        switch check {
        case .available(let release): return "Version \(release.version) is available"
        case .upToDate: return "Up to date"
        case .failed: return "Not checked"
        case nil: return "Not checked yet"
        }
    }

    /// The quiet sentence under the buttons: what an install went wrong on, the release notes when
    /// there is a release, and the reason there is nothing when the answer was a quiet no.
    var note: String? {
        if let failure { return failure }
        switch check {
        case .available(let release): return release.notes.isEmpty ? nil : release.notes
        case .upToDate(let note): return note
        case .failed(let reason): return "The releases could not be read. \(reason)"
        case nil: return nil
        }
    }

    // MARK: - Looking

    /// Called when the window or the panel comes on screen. Cheap by design: almost every call
    /// returns here without doing anything.
    func checkIfStale() {
        guard phase == .idle else { return }
        if let lastChecked, check != nil {
            let age = Date().timeIntervalSince(lastChecked)
            let window = isFailed ? Self.retryFailureAfter : Self.freshFor
            if age < window { return }
        }
        checkNow()
    }

    /// The Check now button, and the only way to ask again inside the fresh window.
    func checkNow() {
        guard phase == .idle else { return }
        phase = .checking
        failure = nil
        let repo = repo()
        let installed = installedVersion
        Task { @MainActor in
            let result = await AppUpdates.check(repo: repo, installedVersion: installed)
            self.check = result
            self.lastChecked = Date()
            self.phase = .idle
            self.remember(result)
        }
    }

    // MARK: - Installing

    /// Whether this release can be installed at all.
    var installBlockedReason: String? {
        guard case .available(let release) = check else { return nil }
        if !release.isVerifiable {
            return """
                Release \(release.version) does not carry a signed manifest, so this build cannot \
                establish that it is a genuine Legion Control release and will not install it.
                """
        }
        if bundleURL.pathExtension != "app" {
            return "Legion Control is not running out of an app bundle, so it cannot replace itself. Build and install it with mac/build.sh."
        }
        return nil
    }

    /// The kept bundle from the last update, when there is one.
    var rollbackVersion: String? {
        guard let previous = AppUpdates.retainedPrevious(beside: bundleURL) else { return nil }
        return AppUpdates.version(ofBundleAt: previous)
    }

    /// Verify, download, stage beside the running bundle, and hand the swap to a helper that can put
    /// the old build back if the new one does not come up.
    ///
    /// Nothing here touches the bundle this app is running out of. The staged replacement is a whole
    /// sibling directory, and the only thing that moves anything is the helper, after this process
    /// has exited.
    func install() {
        guard case .available(let release) = check, phase == .idle else { return }
        if let blocked = installBlockedReason {
            failure = blocked
            return
        }
        let bundleURL = self.bundleURL
        let marker = AppUpdates.launchMarkerURL(support: supportDirectory)
        phase = .downloading
        failure = nil

        let record = OperationRecord(
            id: AgentToken.newOperationId(),
            machineId: MacModel.machineId,
            machineName: "This Mac",
            kind: .appUpdate,
            subject: "Legion Control \(release.version)",
            targetVersion: release.version,
            summary: "Downloading Legion Control \(release.version)."
        )
        operations?.begin(record)

        Task { @MainActor in
            do {
                let download = try await AppUpdates.download(release)
                self.operations?.addPhase(record.id, name: "verified", detail: "signature and sha256 checked against the release key")
                self.phase = .installing
                let staged: AppUpdates.Staged
                do {
                    staged = try await AppUpdates.stage(download, version: release.version, replacing: bundleURL)
                } catch {
                    AppUpdates.discard(download)
                    throw error
                }
                AppUpdates.discard(download)
                self.operations?.addPhase(record.id, name: "staged", detail: staged.bundle.lastPathComponent)

                // Written down before the swap, so the new copy comes up knowing it is current rather
                // than offering to install the version it already is.
                self.check = .upToDate(note: "Updated to \(release.version).")
                self.lastChecked = Date()
                self.remember(self.check!)

                // The marker is removed by the helper before it launches the new build, and written
                // again by the new build once it is up. Its absence is what triggers the recovery.
                try? FileManager.default.removeItem(at: marker)
                try self.performSwap(staged, bundleURL, marker)
                self.operations?.finish(
                    record.id,
                    state: .succeeded,
                    summary: "Legion Control \(release.version) was staged and the swap was handed to the update helper.",
                    detail: "The previous build is kept beside it. If the new one does not come up within 45 seconds the helper puts the old one back."
                )
                self.quit()
            } catch let problem as AppUpdates.InstallFailure {
                self.failure = problem.message
                self.phase = .idle
                self.operations?.finish(record.id, state: .failed, summary: problem.message)
            } catch {
                self.failure = error.localizedDescription
                self.phase = .idle
                self.operations?.finish(record.id, state: .failed, summary: error.localizedDescription)
            }
        }
    }

    /// Put the kept build back, because the user asked for it.
    func rollBack() {
        guard phase == .idle, let previous = AppUpdates.retainedPrevious(beside: bundleURL) else { return }
        phase = .installing
        failure = nil
        let marker = AppUpdates.launchMarkerURL(support: supportDirectory)
        do {
            try? FileManager.default.removeItem(at: marker)
            try AppUpdates.rollBack(to: previous, current: bundleURL, marker: marker)
            quit()
        } catch let problem as AppUpdates.InstallFailure {
            failure = problem.message
            phase = .idle
        } catch {
            failure = error.localizedDescription
            phase = .idle
        }
    }

    /// Called once the app is up. Tells the update helper it launched, which is what stops the
    /// recovery from putting the old build back over a perfectly good new one.
    func confirmLaunch() {
        AppUpdates.confirmLaunch(support: supportDirectory)
    }

    // MARK: - Remembering the answer

    /// What survives a restart. Only the answer and when it was given: enough for the app to come
    /// back up already knowing whether there is anything to install, and small enough that it does
    /// not become a second copy of the release list.
    private struct Stored: Codable {
        var checkedAt: Date
        var version: String?
        var notes: String?
        var assetURL: URL?
        var sizeBytes: Int64?
        var note: String?
    }

    private var isFailed: Bool {
        if case .failed = check { return true }
        return false
    }

    private func remember(_ result: AppUpdates.Check) {
        var stored = Stored(checkedAt: Date())
        switch result {
        case .available(let release):
            stored.version = release.version
            stored.notes = release.notes
            stored.assetURL = release.assetURL
            stored.sizeBytes = release.sizeBytes
        case .upToDate(let note):
            stored.note = note
        case .failed:
            // A failure is not an answer, so it is not written down. The next launch looks again
            // rather than coming up repeating a network error from hours ago.
            AppPreferences.removeObject(forKey: Self.storageKey)
            return
        }
        guard let data = try? JSONEncoder().encode(stored) else { return }
        AppPreferences.set(data, forKey: Self.storageKey)
    }

    private func restore() {
        guard let data = AppPreferences.data(forKey: Self.storageKey),
              let stored = try? JSONDecoder().decode(Stored.self, from: data)
        else { return }

        lastChecked = stored.checkedAt
        if let version = stored.version, let assetURL = stored.assetURL {
            // The version that was waiting last time may be the version running now, because
            // installing it is what ended the last run. Compare again rather than trusting the file.
            guard AppUpdates.isNewer(version, than: installedVersion) else {
                check = .upToDate(note: nil)
                return
            }
            check = .available(AppUpdates.Release(
                version: version,
                notes: stored.notes ?? "",
                assetURL: assetURL,
                sizeBytes: stored.sizeBytes ?? 0
            ))
        } else {
            check = .upToDate(note: stored.note)
        }
    }
}
