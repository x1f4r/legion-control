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
    /// True when `LEGION_CONTROL_HOME` or `LEGION_CONTROL_CONFIG` points this run somewhere else.
    ///
    /// Such a run never replaces, rolls back or discards a build, for the same reason it never
    /// changes the login items. The bundle belongs to the ordinary installation, not to the run, and
    /// `open -n` starts the next build without this run's environment: it would come up reading the
    /// real setup, write its marker where this run is not looking, and be rolled back with both
    /// copies left running.
    @ObservationIgnored var isIsolatedRun: Bool = AppPaths.isRedirected
    /// Set in tests so nothing is ever actually swapped or relaunched. The string is the id the
    /// helper writes down with its outcome, and the last argument hears its exit status, which this
    /// process only lives to hear when the helper gave up.
    @ObservationIgnored var performSwap: @MainActor (AppUpdates.Staged, URL, URL, String, @escaping @Sendable (Int32) -> Void) throws -> Void = {
        try AppUpdates.startSwap(staged: $0, current: $1, marker: $2, swapID: $3, onExit: $4)
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

    /// The repository a check asks: the configured one once it has been held to GitHub's owner/name
    /// grammar, and the built-in one when it fails that.
    var repository: String { AppUpdates.source(for: repo()).repository }

    /// Why updates do not come from the repository the setup names, when they do not.
    var repositoryWarning: String? { AppUpdates.source(for: repo()).warning }

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
        forget(Self.storageKey)
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
        if let blocked = swapBlockedReason { return blocked }
        if bundleURL.pathExtension != "app" {
            return "Legion Control is not running out of an app bundle, so it cannot replace itself. Build and install it with mac/build.sh."
        }
        return nil
    }

    /// Why this run may not change which build is installed: install, roll back or discard.
    var swapBlockedReason: String? {
        if let sibling = AppUpdates.siblingReason(for: bundleURL) { return sibling }
        guard isIsolatedRun else { return nil }
        return "This run was pointed at its own home with LEGION_CONTROL_HOME or LEGION_CONTROL_CONFIG, so it does not install, roll back or discard builds of the app. Do that from an ordinary run."
    }

    /// Bumped when this process changes what is kept beside the bundle. The kept build is read from
    /// the disk, which nothing observes, so this is what has a view that shows it draw again.
    private var retainedChanges = 0

    /// The kept bundle from the last update, when there is one.
    var rollbackVersion: String? {
        _ = retainedChanges
        guard let previous = AppUpdates.retainedPrevious(beside: bundleURL) else { return nil }
        return AppUpdates.version(ofBundleAt: previous)
    }

    /// Verify, download, stage beside the running bundle, and hand the swap to a helper that can put
    /// the old build back if the new one does not come up.
    ///
    /// Nothing here touches the bundle this app is running out of. The staged replacement is a whole
    /// sibling directory, and the only thing that moves anything is the helper, after this process
    /// has exited.
    ///
    /// Nothing here says the update worked either. Handing the swap over proves nothing about the
    /// build that comes up after it, so the operation stays open and the offer stays on screen until
    /// the next launch finds out which build is running (`confirmLaunch`).
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

                // Written down before the helper starts, because the build that comes up after it is
                // the only one that can say whether it worked, and it has to know what was meant.
                let pending = PendingSwap(
                    id: record.id, kind: .update, version: release.version, from: self.installedVersion,
                    operationId: record.id, repository: repository, checkedAt: self.lastChecked,
                    handedOverAt: self.now()
                )
                self.rememberPendingSwap(pending)

                // The marker is removed by the helper before it launches the new build, and written
                // again by the new build once it is up. Its absence is what triggers the recovery.
                do {
                    try self.performSwap(staged, bundleURL, marker, pending.id,
                                         self.helperExit(pending, discarding: staged.bundle))
                } catch {
                    self.forgetPendingSwap()
                    throw error
                }
                self.operations?.addPhase(
                    record.id, name: "handed over",
                    detail: "The update helper swaps the bundles once this build has quit, and puts the previous one back if the new one does not come up within 45 seconds. The build that comes up records the outcome."
                )
                self.quit()
            } catch let changed as RepositoryChanged {
                if let prepared { try? FileManager.default.removeItem(at: prepared.bundle) }
                self.failure = changed.message
                self.phase = .idle
                self.operations?.finish(record.id, state: .cancelled, summary: changed.message)
                self.checkIfStale()
            } catch {
                // The offer was never touched, so it is still the answer on screen and on disk. The
                // staged bundle goes: nothing will ever swap it in now.
                if let prepared { try? FileManager.default.removeItem(at: prepared.bundle) }
                let message = (error as? AppUpdates.InstallFailure)?.message ?? error.localizedDescription
                self.failure = message
                self.phase = .idle
                self.operations?.finish(record.id, state: .failed, summary: message)
            }
        }
    }

    /// Put the kept build back, because the user asked for it.
    func rollBack() {
        guard phase == .idle, let previous = AppUpdates.retainedPrevious(beside: bundleURL) else { return }
        if let blocked = swapBlockedReason {
            failure = blocked
            return
        }
        phase = .installing
        failure = nil
        let marker = AppUpdates.launchMarkerURL(support: supportDirectory)
        let id = UUID().uuidString
        // Only a build that says which version it is can be checked for on the way back up.
        let pending = AppUpdates.version(ofBundleAt: previous).map {
            PendingSwap(id: id, kind: .rollBack, version: $0, from: installedVersion, handedOverAt: now())
        }
        if let pending { rememberPendingSwap(pending) }
        // This build's answer is about this build, and the kept one may be a release that takes any
        // stored answer as its own: it would come up "up to date" with a newer build one step away.
        // If the helper puts this build back instead, it simply asks again.
        let answer = storedData(Self.storageKey)
        forget(Self.storageKey)
        do {
            // Through the same seam as an install, so nothing here can start the real helper unless
            // the app itself asked for it. The kept build is what goes in, so it is never deleted
            // here, whatever the helper does.
            let ignore: @Sendable (Int32) -> Void = { _ in }
            let onExit = pending.map { helperExit($0, discarding: nil) } ?? ignore
            try AppUpdates.rollBack(to: previous, current: bundleURL, marker: marker) { staged, current, marker in
                try self.performSwap(staged, current, marker, id, onExit)
            }
            quit()
        } catch {
            if pending != nil { forgetPendingSwap() }
            if let answer { store(answer, Self.storageKey) }
            failure = (error as? AppUpdates.InstallFailure)?.message ?? error.localizedDescription
            phase = .idle
        }
    }

    /// Throw away the build kept from the last update, because the user asked. Afterwards there is
    /// nothing to go back to, which is the point: it is a whole second copy of the app.
    func discardPrevious() {
        guard phase == .idle, AppUpdates.retainedPrevious(beside: bundleURL) != nil else { return }
        if let blocked = swapBlockedReason {
            failure = blocked
            return
        }
        do {
            try AppUpdates.discardRetained(beside: bundleURL)
            failure = nil
        } catch {
            failure = (error as? AppUpdates.InstallFailure)?.message ?? error.localizedDescription
        }
        retainedChanges &+= 1
        onStateChange?()
    }

    /// What the helper exiting means to a process that is still here to hear it: that it gave up
    /// waiting for this build to quit, because it moves nothing until then. So nothing was swapped.
    private func helperExit(_ pending: PendingSwap, discarding staged: URL?) -> @Sendable (Int32) -> Void {
        { [weak self] status in
            Task { @MainActor in self?.helperGaveUp(pending, status: status, discarding: staged) }
        }
    }

    private func helperGaveUp(_ pending: PendingSwap, status: Int32, discarding staged: URL?) {
        guard loadPendingSwap()?.id == pending.id else { return }
        forgetPendingSwap()
        AppUpdates.forgetSwapOutcome(support: supportDirectory)
        if let staged { try? FileManager.default.removeItem(at: staged) }
        // A roll back put this build's answer away for the build it was going to start. That build
        // is not coming, so the answer goes back.
        if pending.kind == .rollBack, let check { remember(check) }
        let message = Self.notQuit(pending)
        failure = message
        phase = .idle
        if let id = pending.operationId {
            operations?.finish(id, state: .failed, summary: message,
                               detail: "The update helper exited with status \(status) while this build was still running.")
        }
        onStateChange?()
    }

    private static func notQuit(_ pending: PendingSwap) -> String {
        switch pending.kind {
        case .update: "Legion Control did not quit, so the update helper changed nothing and \(pending.version) was not installed."
        case .rollBack: "Legion Control did not quit, so the update helper changed nothing and \(pending.version) was not put back."
        }
    }

    /// Called once the app is up. Tells the update helper it launched, which is what stops the
    /// recovery from putting the old build back over a perfectly good new one, and settles the swap
    /// the last run handed over, now that a build is up to say which one it is.
    func confirmLaunch() {
        AppUpdates.confirmLaunch(support: supportDirectory)
        resolvePendingSwap()
    }

    /// How soon the build a swap put in place looks again for the helper's word, while it waits for
    /// it. Injected so a test need not wait as long.
    @ObservationIgnored var settleInterval: Duration = .milliseconds(250)
    @ObservationIgnored private var settleTask: Task<Void, Never>?

    /// The one place an install is recorded as done: the helper saw the build it put in place come
    /// up, or that build is running and the helper is long gone.
    ///
    /// Not on the running version alone, even when it is the one asked for. The marker this build
    /// has just written can reach the helper a poll after its wait ran out, and then the helper stops
    /// this build and starts the previous one, which has to find the record still there to say the
    /// update did not come up. The helper says "landed" within a poll of seeing the marker, so the
    /// build it was waiting for looks again until it has.
    ///
    /// Any other build settles the swap by what the helper wrote down about it too. A roll back into
    /// a release from before swaps were written down lands without that release saying so, and the
    /// record is next read by a build that knows about it, maybe days later: by version alone, that
    /// looks exactly like the roll back failing.
    private func resolvePendingSwap() {
        // A relocated run never hands a swap over, and its support folder is not where the helper
        // writes, so whatever record it finds belongs to the ordinary installation, unread.
        guard !isIsolatedRun, let pending = loadPendingSwap() else { return }
        let running = installedVersion
        let isTarget = running == pending.version
        let outcome = AppUpdates.swapOutcome(support: supportDirectory).flatMap { $0.id == pending.id ? $0.status : nil }
        if let handedOverAt = pending.handedOverAt {
            let age = now().timeIntervalSince(handedOverAt)
            if age >= 0 && age < AppUpdates.helperLifetime {
                // No word yet while the helper may still be at work. The build it put in place waits
                // for it; any other is a copy started beside the swap, and the next launch settles it.
                if outcome == nil {
                    if isTarget { lookAgainShortly() }
                    return
                }
                // The helper did not see this build come up and is stopping it to start the one
                // before, which settles the swap as the helper says.
                if isTarget && outcome != AppUpdates.SwapOutcome.landed { return }
            }
        }
        forgetPendingSwap()
        AppUpdates.forgetSwapOutcome(support: supportDirectory)

        if isTarget, outcome == nil || outcome == AppUpdates.SwapOutcome.landed {
            switch pending.kind {
            case .update: updateCameUp(pending)
            case .rollBack: discardReplaced(by: pending)
            }
        } else if outcome == AppUpdates.SwapOutcome.landed {
            // The helper saw the build it put in place come up, and something has replaced it since:
            // most often a roll back into an older release, then an update or a reinstall.
            if pending.kind == .update, let id = pending.operationId {
                operations?.finish(id, state: .succeeded, summary: "Legion Control \(pending.version) was installed and came up.")
            }
        } else {
            let message = Self.swapFailure(pending, running: running, outcome: outcome)
            failure = message
            if let id = pending.operationId { operations?.finish(id, state: .failed, summary: message) }
        }
        onStateChange?()
    }

    private func lookAgainShortly() {
        guard settleTask == nil else { return }
        let interval = settleInterval
        settleTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: interval)
            self?.settleTask = nil
            self?.resolvePendingSwap()
        }
    }

    /// After a roll back that landed, the build it replaced is left where the helper moved it, under
    /// the staged name. Nothing ever starts it from there, discarding only reaches the kept build,
    /// and the next install stages over it, so until then it is a second whole copy of the app that
    /// only Spotlight can find. The update offer is the way back to it.
    ///
    /// Only the build the roll back replaced, by its version, and never from a copy running out of
    /// one of the names beside the real app, which may be that very bundle.
    private func discardReplaced(by pending: PendingSwap) {
        guard AppUpdates.siblingReason(for: bundleURL) == nil else { return }
        let replaced = bundleURL.deletingLastPathComponent().appending(path: AppUpdates.stagedName)
        guard AppUpdates.version(ofBundleAt: replaced) == pending.from else { return }
        try? FileManager.default.removeItem(at: replaced)
    }

    private func updateCameUp(_ pending: PendingSwap) {
        let running = installedVersion
        if let id = pending.operationId {
            operations?.finish(id, state: .succeeded,
                               summary: "Legion Control \(running) is installed and came up.",
                               detail: "The previous build, \(pending.from), is kept beside it until it is discarded.")
        }
        // The check that offered this build found it the newest there was, and until the next
        // check that is still the answer, so the new build does not come up with none. Not when
        // the setup has since named another repository: that one has not been asked. Nor once this
        // build has an answer of its own, which it can by the time the helper has had its say.
        if check == nil, pending.repository == nil || pending.repository == repo() {
            let answer = AppUpdates.Check.upToDate(note: "Updated to \(running).")
            checkedRepo = repo()
            lastChecked = pending.checkedAt ?? now()
            check = answer
            remember(answer)
        }
    }

    /// What a swap that did not land is reported as, by what the helper said happened.
    private static func swapFailure(_ pending: PendingSwap, running: String, outcome: Int32?) -> String {
        let undone = pending.kind == .update ? "was not installed" : "was not put back"
        switch outcome {
        case AppUpdates.SwapOutcome.putBack?:
            guard running == pending.from else {
                return "Legion Control \(pending.version) did not come up, and the build running now is \(running)."
            }
            let still = "Legion Control \(pending.version) did not come up, so \(running) is still the build that runs."
            return pending.kind == .update
                ? still + " The update helper puts the previous build back when the new one does not start."
                : still
        case AppUpdates.SwapOutcome.notMoved?:
            return "The update helper could not move the bundles, so Legion Control \(pending.version) \(undone) and \(running) is still the build that runs."
        case AppUpdates.SwapOutcome.notQuit?:
            return notQuit(pending)
        case let status?:
            return "The update helper stopped with status \(status), so Legion Control \(pending.version) \(undone). The build running now is \(running)."
        case nil:
            return "Legion Control \(pending.version) was not confirmed: the update helper stopped before it said how the swap ended, and the build running now is \(running)."
        }
    }

    // MARK: - The swap in flight

    /// A swap handed to the helper and not yet seen to land. Written before the helper starts and
    /// read by whichever build comes up next.
    struct PendingSwap: Codable, Equatable {
        enum Kind: String, Codable { case update, rollBack }

        var id: String
        var kind: Kind
        /// The build that should be running once it worked, and the one that was running before.
        var version: String
        var from: String
        /// The operation record an install keeps. A roll back has none.
        var operationId: String?
        /// Where and when the check that offered the update looked.
        var repository: String?
        var checkedAt: Date?
        /// When the helper was given the swap, so a launch can tell a helper that may still be at
        /// work from one that is long gone.
        var handedOverAt: Date?
    }

    private static let pendingKey = "appUpdatePendingSwap"

    func loadPendingSwap() -> PendingSwap? {
        storedData(Self.pendingKey).flatMap { try? JSONDecoder().decode(PendingSwap.self, from: $0) }
    }

    private func rememberPendingSwap(_ pending: PendingSwap) {
        guard let data = try? JSONEncoder().encode(pending) else { return }
        store(data, Self.pendingKey)
    }

    private func forgetPendingSwap() {
        forget(Self.pendingKey)
    }

    // MARK: - Remembering the answer

    /// What survives a restart. Only the answer and when it was given: enough for the app to come
    /// back up already knowing whether there is anything to install, and small enough that it does
    /// not become a second copy of the release list.
    private struct Stored: Codable {
        var checkedAt: Date
        var repository: String?
        /// The build that asked. An answer is only ever about the build that asked for it: after an
        /// update, a roll back or a reinstall, "up to date" from another build is not an answer.
        var installedVersion: String?
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
        var stored = Stored(checkedAt: lastChecked ?? now(), repository: checkedRepo, installedVersion: installedVersion)
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
            forget(Self.storageKey)
            return
        }
        guard let data = try? JSONEncoder().encode(stored) else { return }
        store(data, Self.storageKey)
    }

    private func restore() {
        guard let data = storedData(Self.storageKey),
              let stored = try? JSONDecoder().decode(Stored.self, from: data)
        else { return }
        // Another build's answer, so this one comes up having not asked yet and asks straight away.
        // Older records do not say who asked, and are compared below instead.
        if let asked = stored.installedVersion, asked != installedVersion { return }

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

    /// The injected preferences in tests, this app's own defaults otherwise.
    private func storedData(_ key: String) -> Data? {
        if let preferences { return preferences.data(forKey: key) }
        return AppPreferences.data(forKey: key)
    }

    private func store(_ data: Data, _ key: String) {
        if let preferences { preferences.set(data, forKey: key) }
        else { AppPreferences.set(data, forKey: key) }
    }

    private func forget(_ key: String) {
        if let preferences { preferences.removeObject(forKey: key) }
        else { AppPreferences.removeObject(forKey: key) }
    }
}
