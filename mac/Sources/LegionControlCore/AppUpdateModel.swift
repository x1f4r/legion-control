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
/// Release checks run at startup, on foreground entry and periodically while the app runs.
/// Fleet status polling remains separate and stops when no viewer is open.
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
    private(set) var checkFailure: String?
    private var checkedRepo: String?
    private var observedRepo: String?
    private var repositoryEpoch: UInt64 = 0
    @ObservationIgnored private var automaticCheckTask: Task<Void, Never>?
    @ObservationIgnored var onStateChange: (@MainActor () -> Void)?
    @ObservationIgnored var now: @MainActor () -> Date = { Date() }
    @ObservationIgnored var fetchRelease: @MainActor (String, String) async -> AppUpdates.Check = { await AppUpdates.check(repo: $0, installedVersion: $1) }
    @ObservationIgnored private let preferences: FilePreferences?
    @ObservationIgnored private let versionOverride: String?

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
    @ObservationIgnored var downloadRelease: @MainActor (AppUpdates.Release) async throws -> AppUpdates.Download = { try await AppUpdates.download($0) }
    @ObservationIgnored var stageRelease: @MainActor (AppUpdates.Download, String, URL) async throws -> AppUpdates.Staged = { try await AppUpdates.stage($0, version: $1, replacing: $2) }
    @ObservationIgnored var quit: @MainActor () -> Void = { NSApp.terminate(nil) }

    /// Foreground checks are throttled independently from the periodic background check.
    private static let freshFor: TimeInterval = 15 * 60
    private static let periodicFreshFor: TimeInterval = 6 * 60 * 60
    /// How long a failed look stays good for. Much shorter, because the usual reason for one is that
    /// the laptop had no network a moment ago, and that does change.
    private static let retryFailureAfter: TimeInterval = 10 * 60

    private static let storageKey = "appUpdateLastCheck"

    init(preferences: FilePreferences? = nil, installedVersion: String? = nil) {
        self.preferences = preferences
        self.versionOverride = installedVersion
        restore()
        observedRepo = checkedRepo ?? repo()
    }

    // MARK: - What the screen reads

    var installedVersion: String {
        versionOverride ?? (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0")
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
        if let checkFailure { return "The release check could not finish. \(checkFailure)" }
        switch check {
        case .available(let release): return release.notes.isEmpty ? nil : release.notes
        case .upToDate(let note): return note
        case .failed(let reason): return "The releases could not be read. \(reason)"
        case nil: return nil
        }
    }

    // MARK: - Looking

    /// Every configured origin transition revokes cached and in-flight work, including A → B → A.
    func repositoryDidChange() {
        let current = repo()
        guard observedRepo != current else { return }
        observedRepo = current
        repositoryEpoch &+= 1
        checkedRepo = nil
        check = nil
        lastChecked = nil
        checkFailure = nil
        failure = nil
        if let preferences { preferences.removeObject(forKey: Self.storageKey) }
        else { AppPreferences.removeObject(forKey: Self.storageKey) }
        onStateChange?()
    }

    private func isCurrent(repository: String, epoch: UInt64) -> Bool {
        epoch == repositoryEpoch && repository == repo()
    }

    private struct RepositoryChanged: Error {
        let message = "The update repository changed. This installation was cancelled before replacing the app."
    }

    private func requireCurrent(repository: String, epoch: UInt64) throws {
        guard isCurrent(repository: repository, epoch: epoch) else { throw RepositoryChanged() }
    }

    /// Called on foreground entry; a recent answer avoids a duplicate request.
    func checkIfStale(minimumAge: TimeInterval? = nil) {
        repositoryDidChange()
        guard phase == .idle else { return }
        let repositoryChanged = checkedRepo != repo()
        if !repositoryChanged, let lastChecked, check != nil {
            let age = now().timeIntervalSince(lastChecked)
            let window = isFailed ? Self.retryFailureAfter : (minimumAge ?? Self.freshFor)
            if age >= 0 && age < window { return }
        }
        checkNow()
    }

    func startAutomaticChecks(interval: Duration = .seconds(60)) {
        guard automaticCheckTask == nil else { return }
        checkIfStale()
        automaticCheckTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: interval) } catch { return }
                // A completed sleep can still be queued on the main actor when stop cancels it.
                guard !Task.isCancelled else { return }
                self?.checkIfStale(minimumAge: Self.periodicFreshFor)
            }
        }
    }

    func stopAutomaticChecks() {
        automaticCheckTask?.cancel()
        automaticCheckTask = nil
    }

    /// A manual check bypasses the foreground throttle.
    func checkNow() {
        repositoryDidChange()
        guard phase == .idle else { return }
        phase = .checking
        let repository = repo()
        let epoch = repositoryEpoch
        if checkedRepo != repository { check = nil; checkFailure = nil }
        let installed = installedVersion
        Task { @MainActor in
            defer { self.onStateChange?() }
            let result = await fetchRelease(repository, installed)
            self.phase = .idle
            guard self.isCurrent(repository: repository, epoch: epoch) else {
                self.checkIfStale()
                return
            }
            self.checkedRepo = repository
            self.lastChecked = self.now()
            if case .failed(let reason) = result {
                self.checkFailure = reason
                if case .available = self.check { return }
            } else {
                self.checkFailure = nil
            }
            self.check = result
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
        repositoryDidChange()
        guard checkedRepo == repo() else { checkIfStale(); return }
        guard case .available(let release) = check, phase == .idle else { return }
        if let blocked = installBlockedReason {
            failure = blocked
            return
        }
        let repository = repo()
        let epoch = repositoryEpoch
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
            var prepared: AppUpdates.Staged?
            defer { self.onStateChange?() }
            do {
                try self.requireCurrent(repository: repository, epoch: epoch)
                let download = try await self.downloadRelease(release)
                defer { AppUpdates.discard(download) }
                try self.requireCurrent(repository: repository, epoch: epoch)
                self.operations?.addPhase(record.id, name: "verified", detail: "signature and sha256 checked against the release key")
                self.phase = .installing
                let staged = try await self.stageRelease(download, release.version, bundleURL)
                prepared = staged
                try self.requireCurrent(repository: repository, epoch: epoch)
                self.operations?.addPhase(record.id, name: "staged", detail: staged.bundle.lastPathComponent)

                // Written down before the swap, so the new copy comes up knowing it is current rather
                // than offering to install the version it already is.
                self.check = .upToDate(note: "Updated to \(release.version).")
                self.lastChecked = Date()
                self.remember(self.check!)

                // The marker is removed by the helper before it launches the new build, and written
                // again by the new build once it is up. Its absence is what triggers the recovery.
                try? FileManager.default.removeItem(at: marker)
                try self.requireCurrent(repository: repository, epoch: epoch)
                try self.performSwap(staged, bundleURL, marker)
                self.operations?.finish(
                    record.id,
                    state: .succeeded,
                    summary: "Legion Control \(release.version) was staged and the swap was handed to the update helper.",
                    detail: "The previous build is kept beside it. If the new one does not come up within 45 seconds the helper puts the old one back."
                )
                self.quit()
            } catch let changed as RepositoryChanged {
                if let prepared { try? FileManager.default.removeItem(at: prepared.bundle) }
                self.failure = changed.message
                self.phase = .idle
                self.operations?.finish(record.id, state: .cancelled, summary: changed.message)
                self.checkIfStale()
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
        var repository: String?
        var manifestURL: URL?
        var signatureURL: URL?
        var version: String?
        var notes: String?
        var assetURL: URL?
        var sizeBytes: Int64?
        var note: String?
    }

    private var isFailed: Bool {
        if checkFailure != nil { return true }
        if case .failed = check { return true }
        return false
    }

    private func remember(_ result: AppUpdates.Check) {
        var stored = Stored(checkedAt: lastChecked ?? now(), repository: checkedRepo)
        switch result {
        case .available(let release):
            stored.version = release.version
            stored.notes = release.notes
            stored.assetURL = release.assetURL
            stored.sizeBytes = release.sizeBytes
            stored.manifestURL = release.manifestURL
            stored.signatureURL = release.signatureURL
        case .upToDate(let note):
            stored.note = note
        case .failed:
            // A failure is not an answer, so it is not written down. The next launch looks again
            // rather than coming up repeating a network error from hours ago.
            if let preferences { preferences.removeObject(forKey: Self.storageKey) }
            else { AppPreferences.removeObject(forKey: Self.storageKey) }
            return
        }
        guard let data = try? JSONEncoder().encode(stored) else { return }
        if let preferences { preferences.set(data, forKey: Self.storageKey) }
        else { AppPreferences.set(data, forKey: Self.storageKey) }
    }

    private func restore() {
        let saved: Data?
        if let preferences { saved = preferences.data(forKey: Self.storageKey) }
        else { saved = AppPreferences.data(forKey: Self.storageKey) }
        guard let data = saved,
              let stored = try? JSONDecoder().decode(Stored.self, from: data)
        else { return }

        lastChecked = stored.checkedAt
        checkedRepo = stored.repository
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
                sizeBytes: stored.sizeBytes ?? 0,
                manifestURL: stored.manifestURL,
                signatureURL: stored.signatureURL
            ))
        } else {
            check = .upToDate(note: stored.note)
        }
    }
}
