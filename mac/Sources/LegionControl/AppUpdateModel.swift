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

    /// Download the release, put it in place of the bundle this app is running out of, and start it
    /// again. Every step that can fail happens before anything in /Applications is touched.
    func install() {
        guard case .available(let release) = check, phase == .idle else { return }
        let bundleURL = Bundle.main.bundleURL
        phase = .downloading
        failure = nil

        Task { @MainActor in
            do {
                let download = try await AppUpdates.download(release)
                self.phase = .installing
                do {
                    try await AppUpdates.apply(download, version: release.version, replacing: bundleURL)
                } catch {
                    AppUpdates.discard(download)
                    throw error
                }
                AppUpdates.discard(download)

                // Written down before the relaunch, so the new copy comes up knowing it is current
                // rather than offering to install the version it already is.
                self.check = .upToDate(note: "Updated to \(release.version).")
                self.lastChecked = Date()
                self.remember(self.check!)

                AppUpdates.relaunch(bundleURL)
                NSApp.terminate(nil)
            } catch let problem as AppUpdates.InstallFailure {
                self.failure = problem.message
                self.phase = .idle
            } catch {
                self.failure = error.localizedDescription
                self.phase = .idle
            }
        }
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
            UserDefaults.standard.removeObject(forKey: Self.storageKey)
            return
        }
        guard let data = try? JSONEncoder().encode(stored) else { return }
        UserDefaults.standard.set(data, forKey: Self.storageKey)
    }

    private func restore() {
        guard let data = UserDefaults.standard.data(forKey: Self.storageKey),
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
