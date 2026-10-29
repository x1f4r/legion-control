import Foundation
import Testing
@testable import LegionControlCore

@MainActor
struct AppUpdateModelTests {
    @MainActor final class Repository {
        var value = "a/a"
    }
    static let release = AppUpdates.Release(
        version: "3.1.0", notes: "A new release.",
        assetURL: URL(string: "https://example.invalid/app.zip")!, sizeBytes: 100,
        manifestURL: URL(string: "https://example.invalid/manifest.json")!,
        signatureURL: URL(string: "https://example.invalid/manifest.json.sig")!
    )

    func fixture() -> (AppUpdateModel, FilePreferences, URL) {
        let directory = FileManager.default.temporaryDirectory.appending(path: "app-update-model-\(UUID().uuidString)")
        let preferences = FilePreferences(url: directory.appending(path: "preferences.json"))
        let model = AppUpdateModel(preferences: preferences, installedVersion: "3.0.0")
        model.bundleURL = directory.appending(path: "Legion Control.app")
        // Never the real Application Support: a launch confirmed here writes the marker.
        model.supportDirectory = directory.appending(path: "support")
        // Whatever the environment running the tests says: the cases that want an isolated run
        // ask for one.
        model.isIsolatedRun = false
        return (model, preferences, directory)
    }

    func settle(_ model: AppUpdateModel) async {
        let deadline = Date().addingTimeInterval(2)
        while model.phase != .idle, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
        #expect(model.phase == .idle)
    }

    @Test("foreground checks throttle for fifteen minutes and manual checks bypass the throttle")
    func foregroundCadence() async {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        var clock = Date(timeIntervalSince1970: 2_000_000_000)
        model.now = { clock }
        var calls = 0
        model.fetchRelease = { _, _ in calls += 1; return .available(Self.release) }
        model.checkIfStale()
        await settle(model)
        #expect(calls == 1)
        clock += 899
        model.checkIfStale()
        #expect(calls == 1)
        clock += 1
        model.checkIfStale()
        await settle(model)
        #expect(calls == 2)
        model.checkNow()
        await settle(model)
        #expect(calls == 3)
    }

    @Test("a startup without a viewer and a continuously running app both discover updates")
    func periodicChecks() async {
        let (model, _, directory) = fixture()
        defer { model.stopAutomaticChecks(); try? FileManager.default.removeItem(at: directory) }
        var clock = Date(timeIntervalSince1970: 2_000_000_000)
        model.now = { clock }
        var calls = 0
        model.fetchRelease = { _, _ in
            calls += 1
            return calls == 1 ? .upToDate(note: nil) : .available(Self.release)
        }
        model.startAutomaticChecks(interval: .milliseconds(10))
        model.startAutomaticChecks(interval: .milliseconds(10))
        await settle(model)
        #expect(calls == 1)
        clock += 6 * 60 * 60 - 1
        try? await Task.sleep(for: .milliseconds(30))
        #expect(calls == 1)
        clock += 1
        let deadline = Date().addingTimeInterval(2)
        while model.availableVersion == nil, Date() < deadline { try? await Task.sleep(for: .milliseconds(10)) }
        #expect(calls == 2)
        #expect(model.availableVersion == "3.1.0")
        model.stopAutomaticChecks()
        clock += 6 * 60 * 60
        try? await Task.sleep(for: .milliseconds(30))
        #expect(calls == 2)
    }

    @Test("availability and signed manifest locations survive relaunch and transient check failures")
    func persistentAvailability() async {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        var clock = Date(timeIntervalSince1970: 2_000_000_000)
        model.now = { clock }
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        #expect(model.installBlockedReason == nil)
        let restored = AppUpdateModel(preferences: preferences, installedVersion: "3.0.0")
        restored.bundleURL = model.bundleURL
        restored.isIsolatedRun = false
        #expect(restored.check == .available(Self.release))
        #expect(restored.installBlockedReason == nil)

        var calls = 0
        model.fetchRelease = { _, _ in calls += 1; return .failed(reason: "offline") }
        model.checkNow()
        await settle(model)
        #expect(model.availableVersion == "3.1.0")
        #expect(model.checkFailure == "offline")
        #expect(model.installBlockedReason == nil)
        clock += 599
        model.checkIfStale()
        #expect(calls == 1)
        clock += 1
        model.checkIfStale()
        await settle(model)
        #expect(calls == 2)
        let afterFailure = AppUpdateModel(preferences: preferences, installedVersion: "3.0.0")
        #expect(afterFailure.check == .available(Self.release))
        let updated = AppUpdateModel(preferences: preferences, installedVersion: "3.1.0")
        #expect(updated.availableVersion == nil)
    }

    @Test("a configured release repository change cannot reuse another repository's cached update")
    func repositoryChange() async {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        let restored = AppUpdateModel(preferences: preferences, installedVersion: "3.0.0")
        restored.repo = { "someone/another-repo" }
        var requested: [String] = []
        restored.fetchRelease = { repository, _ in
            requested.append(repository)
            return .upToDate(note: nil)
        }
        restored.checkIfStale()
        #expect(restored.availableVersion == nil)
        await settle(restored)
        #expect(requested == ["someone/another-repo"])
        #expect(restored.availableVersion == nil)
    }

    @Test("an update is visible in the menu bar even when no local machine is configured")
    func globalSuggestion() async {
        let (updates, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let app = AppModel(config: ConfigStore(url: directory.appending(path: "config.json")),
                           bindings: BindingsStore(url: directory.appending(path: "bindings.json")),
                           operations: OperationStore(url: directory.appending(path: "operations.json")),
                           appUpdates: updates)
        var redraws = 0
        app.onStateChange = { redraws += 1 }
        updates.fetchRelease = { _, _ in .available(Self.release) }
        updates.checkNow()
        await settle(updates)
        #expect(app.mac == nil)
        #expect(app.menuBarSymbol == "arrow.down.circle")
        #expect(app.menuBarDescription.contains("3.1.0"))
        #expect(redraws == 1)
        updates.fetchRelease = { _, _ in .upToDate(note: nil) }
        updates.checkNow()
        await settle(updates)
        #expect(app.menuBarSymbol != "arrow.down.circle")
        #expect(redraws == 2)
    }

    @Test("a setup repository edit immediately removes the old offer and cache")
    func setupEditInvalidatesOffer() async throws {
        let (updates, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let configURL = directory.appending(path: "config.json")
        try Data(#"{"version":1,"machines":[],"appUpdates":{"githubRepo":"a/a"}}"#.utf8).write(to: configURL)
        let app = AppModel(config: ConfigStore(url: configURL),
                           bindings: BindingsStore(url: directory.appending(path: "bindings.json")),
                           operations: OperationStore(url: directory.appending(path: "operations.json")), appUpdates: updates)
        updates.fetchRelease = { _, _ in .available(Self.release) }
        updates.checkNow()
        await settle(updates)
        #expect(updates.availableVersion != nil)
        #expect(app.editSetup("change release repository") { $0["appUpdates"] = ["githubRepo": "b/b"] } == nil)
        #expect(updates.availableVersion == nil)
        #expect(preferences.data(forKey: "appUpdateLastCheck") == nil)
        #expect(app.editSetup("restore release repository") { $0["appUpdates"] = ["githubRepo": "a/a"] } == nil)
        #expect(updates.availableVersion == nil)
    }

    @Test("a late check from before A to B to A cannot restore the old offer")
    func lateCheckEpoch() async {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let repository = Repository()
        model.repo = { repository.value }
        var resume: CheckedContinuation<Void, Never>?
        var calls = 0
        model.fetchRelease = { _, _ in
            calls += 1
            if calls == 1 {
                await withCheckedContinuation { resume = $0 }
                return .available(Self.release)
            }
            return .upToDate(note: nil)
        }
        model.checkNow()
        let deadline = Date().addingTimeInterval(2)
        while resume == nil, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
        #expect(resume != nil)
        repository.value = "b/b"; model.repositoryDidChange()
        repository.value = "a/a"; model.repositoryDidChange()
        resume?.resume()
        await settle(model)
        #expect(calls == 2)
        #expect(model.availableVersion == nil)
    }

    @Test("repository changes during download or staging revoke installation before swap", arguments: [false, true])
    func installEpoch(holdDuringStage: Bool) async throws {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        model.supportDirectory = directory.appending(path: "support")
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        model.operations = store
        let repository = Repository()
        model.repo = { repository.value }
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        var resume: CheckedContinuation<Void, Never>?
        var stages = 0
        var swaps = 0
        var quits = 0
        let downloadDirectory = directory.appending(path: "download")
        let stagedBundle = directory.appending(path: "Legion Control.new.app")
        let previous = directory.appending(path: "Legion Control.previous.app")
        try FileManager.default.createDirectory(at: model.bundleURL, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: previous, withIntermediateDirectories: true)
        try Data("current".utf8).write(to: model.bundleURL.appending(path: "proof"))
        try Data("previous".utf8).write(to: previous.appending(path: "proof"))
        model.downloadRelease = { _ in
            try FileManager.default.createDirectory(at: downloadDirectory, withIntermediateDirectories: true)
            let artifact = ReleaseTrust.Manifest.Artifact(name: "app.zip", sha256: String(repeating: "a", count: 64), size: 0)
            let download = AppUpdates.Download(directory: downloadDirectory, archive: downloadDirectory.appending(path: "app.zip"),
                                               manifest: .init(schema: 1, version: "3.1.0", artifacts: [artifact]), artifact: artifact)
            if !holdDuringStage { await withCheckedContinuation { resume = $0 } }
            return download
        }
        model.stageRelease = { _, version, _ in
            stages += 1
            try FileManager.default.createDirectory(at: stagedBundle, withIntermediateDirectories: true)
            if holdDuringStage { await withCheckedContinuation { resume = $0 } }
            return .init(bundle: stagedBundle, previous: previous, version: version)
        }
        model.performSwap = { _, _, _, _, _ in swaps += 1 }
        model.quit = { quits += 1 }
        model.install()
        let deadline = Date().addingTimeInterval(2)
        while resume == nil, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
        #expect(resume != nil)
        repository.value = "b/b"; model.repositoryDidChange()
        #expect(model.availableVersion == nil)
        repository.value = "a/a"; model.repositoryDidChange()
        resume?.resume()
        await settle(model)
        #expect(swaps == 0)
        #expect(quits == 0)
        #expect(stages == (holdDuringStage ? 1 : 0))
        #expect(store.newestFirst.first?.state == .cancelled)
        #expect(!FileManager.default.fileExists(atPath: downloadDirectory.path))
        #expect(!FileManager.default.fileExists(atPath: stagedBundle.path))
        #expect(try String(contentsOf: model.bundleURL.appending(path: "proof"), encoding: .utf8) == "current")
        #expect(try String(contentsOf: previous.appending(path: "proof"), encoding: .utf8) == "previous")
    }

    @Test("install retains the current launch marker through helper handoff when quit does not exit")
    func installRetainsMarkerUntilExit() async throws {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        model.supportDirectory = directory.appending(path: "support")
        try FileManager.default.createDirectory(at: model.supportDirectory, withIntermediateDirectories: true)
        let marker = AppUpdates.launchMarkerURL(support: model.supportDirectory)
        let proof = Data("old app launch proof".utf8)
        try proof.write(to: marker)
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        let downloadDirectory = directory.appending(path: "download")
        model.downloadRelease = { _ in
            try FileManager.default.createDirectory(at: downloadDirectory, withIntermediateDirectories: true)
            let artifact = ReleaseTrust.Manifest.Artifact(name: "app.zip", sha256: String(repeating: "a", count: 64), size: 0)
            return .init(directory: downloadDirectory, archive: downloadDirectory.appending(path: "app.zip"),
                         manifest: .init(schema: 1, version: "3.1.0", artifacts: [artifact]), artifact: artifact)
        }
        model.stageRelease = { _, version, _ in
            .init(bundle: directory.appending(path: "Legion Control.new.app"),
                  previous: directory.appending(path: "Legion Control.previous.app"), version: version)
        }
        var handedOff = false
        var quitRequested = false
        model.performSwap = { _, _, actualMarker, _, _ in
            #expect(actualMarker == marker)
            let observed = try Data(contentsOf: marker)
            #expect(observed == proof)
            handedOff = true
        }
        model.quit = { quitRequested = true }
        model.install()
        let deadline = Date().addingTimeInterval(2)
        while !quitRequested, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
        #expect(handedOff)
        #expect(quitRequested)
        #expect(try Data(contentsOf: marker) == proof)
    }

    @Test("legacy cached releases refresh their missing signature metadata immediately")
    func legacyCacheRefresh() async throws {
        let (_, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cached: [String: Any] = [
            "checkedAt": Date().timeIntervalSinceReferenceDate,
            "version": "3.1.0", "assetURL": "https://example.invalid/app.zip", "sizeBytes": 100
        ]
        preferences.set(try JSONSerialization.data(withJSONObject: cached), forKey: "appUpdateLastCheck")
        let model = AppUpdateModel(preferences: preferences, installedVersion: "3.0.0")
        model.bundleURL = directory.appending(path: "Legion Control.app")
        model.isIsolatedRun = false
        #expect(model.installBlockedReason != nil)
        var calls = 0
        model.fetchRelease = { _, _ in calls += 1; return .available(Self.release) }
        model.checkIfStale()
        await settle(model)
        #expect(calls == 1)
        #expect(model.installBlockedReason == nil)
    }

    // MARK: - Honest outcomes

    typealias Swap = @MainActor (AppUpdates.Staged, URL, URL, String, @escaping @Sendable (Int32) -> Void) throws -> Void

    /// An install taken as far as the handoff: a download and a staged bundle inside `directory`,
    /// and whatever swap the test wants in place of the helper. Returns the staged bundle.
    @discardableResult
    func prepareInstall(_ model: AppUpdateModel, in directory: URL, swap: @escaping Swap) -> URL {
        let downloadDirectory = directory.appending(path: "download")
        let stagedBundle = directory.appending(path: "Legion Control.new.app")
        model.downloadRelease = { _ in
            try FileManager.default.createDirectory(at: downloadDirectory, withIntermediateDirectories: true)
            let artifact = ReleaseTrust.Manifest.Artifact(name: "app.zip", sha256: String(repeating: "a", count: 64), size: 1)
            return .init(directory: downloadDirectory, archive: downloadDirectory.appending(path: "app.zip"),
                         manifest: .init(schema: 1, version: "3.1.0", artifacts: [artifact]), artifact: artifact)
        }
        model.stageRelease = { _, version, _ in
            try FileManager.default.createDirectory(at: stagedBundle, withIntermediateDirectories: true)
            return .init(bundle: stagedBundle, previous: directory.appending(path: "Legion Control.previous.app"), version: version)
        }
        model.performSwap = swap
        return stagedBundle
    }

    /// The model the next launch makes: the same preferences and history, a different build.
    func relaunch(after model: AppUpdateModel, preferences: FilePreferences, store: OperationStore,
                  as version: String) -> (AppUpdateModel, OperationStore) {
        let next = AppUpdateModel(preferences: preferences, installedVersion: version)
        next.bundleURL = model.bundleURL
        next.supportDirectory = model.supportDirectory
        next.isIsolatedRun = false
        let history = OperationStore(url: store.url)
        next.operations = history
        return (next, history)
    }

    func offerAndInstall(_ model: AppUpdateModel, store: OperationStore) async {
        model.operations = store
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        var quits = 0
        model.quit = { quits += 1 }
        model.install()
        let deadline = Date().addingTimeInterval(2)
        while quits == 0, model.phase != .idle || model.failure == nil, Date() < deadline {
            try? await Task.sleep(for: .milliseconds(5))
        }
    }

    /// Waits a little for something a model does on a timer of its own.
    func eventually(_ condition: () -> Bool) async {
        let deadline = Date().addingTimeInterval(2)
        while !condition(), Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
    }

    @Test("an install handed to the helper is only recorded as done once the helper has seen the new build come up")
    func installConfirmedByNextLaunch() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        prepareInstall(model, in: directory) { _, _, _, _, _ in }
        await offerAndInstall(model, store: store)

        // Handing over proves nothing about the build that comes up after it, so nothing says
        // "updated" yet: not the record, not the check, not what a relaunch of this build reads.
        let pending = try #require(model.loadPendingSwap())
        let id = try #require(pending.operationId)
        #expect(pending.version == "3.1.0")
        #expect(pending.from == "3.0.0")
        #expect(store.record(id: id)?.state == .running)
        #expect(model.check == .available(Self.release))
        #expect(AppUpdateModel(preferences: preferences, installedVersion: "3.0.0").check == .available(Self.release))

        let (next, history) = relaunch(after: model, preferences: preferences, store: store, as: "3.1.0")
        next.settleInterval = .milliseconds(5)
        next.confirmLaunch()
        let marker = AppUpdates.launchMarkerURL(support: next.supportDirectory)
        #expect(FileManager.default.fileExists(atPath: marker.path))
        // Running the build it asked for is not the helper's word yet: its wait may have run out a
        // poll before that marker, and then it stops this build and starts the old one.
        try? await Task.sleep(for: .milliseconds(30))
        #expect(next.loadPendingSwap() == pending)
        #expect(history.record(id: id)?.state == .unknown)
        #expect(next.check == nil)
        #expect(next.failure == nil)

        // The helper sees the marker and says so.
        try Self.helperEnds(AppUpdates.SwapOutcome.landed, swap: pending.id, marker: marker)
        await eventually { history.record(id: id)?.state == .succeeded }
        #expect(history.record(id: id)?.state == .succeeded)
        #expect(next.check == .upToDate(note: "Updated to 3.1.0."))
        #expect(next.failure == nil)
        #expect(next.loadPendingSwap() == nil)
        #expect(AppUpdates.swapOutcome(support: next.supportDirectory) == nil)
    }

    @Test("a new build the helper stops after all leaves the swap to the build put back, which reports it")
    func lateNewBuildLeavesSwap() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        prepareInstall(model, in: directory) { _, _, _, _, _ in }
        await offerAndInstall(model, store: store)
        let pending = try #require(model.loadPendingSwap())
        let id = try #require(pending.operationId)

        // The new build's marker came a poll after the helper's wait ran out, so the helper says
        // the old build goes back while the new one is still waiting to hear.
        let (late, lateHistory) = relaunch(after: model, preferences: preferences, store: store, as: "3.1.0")
        late.settleInterval = .milliseconds(5)
        late.confirmLaunch()
        let marker = AppUpdates.launchMarkerURL(support: late.supportDirectory)
        try Self.helperEnds(AppUpdates.SwapOutcome.putBack, swap: pending.id, marker: marker)
        try? await Task.sleep(for: .milliseconds(30))
        #expect(late.loadPendingSwap() == pending)
        #expect(lateHistory.record(id: id)?.state == .unknown)
        #expect(late.failure == nil)

        // The helper stops it and starts the old build, which finds the record and the reason.
        let (old, oldHistory) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        old.confirmLaunch()
        #expect(oldHistory.record(id: id)?.state == .failed)
        #expect(old.failure?.contains("3.1.0 did not come up, so 3.0.0 is still the build that runs") == true)
        #expect(old.check == .available(Self.release))
        #expect(old.loadPendingSwap() == nil)
    }

    @Test("the new build settles by its own version only once the helper is long gone without a word")
    func newBuildAfterHelperIsGone() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        prepareInstall(model, in: directory) { _, _, _, _, _ in }
        await offerAndInstall(model, store: store)
        let id = try #require(model.loadPendingSwap()?.operationId)

        // The Mac went down mid-swap, say, and the new build was started at login: nothing is left
        // to stop it, so it is what was installed.
        let (next, history) = relaunch(after: model, preferences: preferences, store: store, as: "3.1.0")
        next.now = { Date().addingTimeInterval(AppUpdates.helperLifetime + 1) }
        next.confirmLaunch()
        #expect(history.record(id: id)?.state == .succeeded)
        #expect(next.loadPendingSwap() == nil)
        #expect(next.failure == nil)
    }

    @Test("a relocated run leaves the ordinary installation's swap alone, whichever build it is", arguments: ["3.0.0", "3.1.0"])
    func isolatedRunLeavesSwap(version: String) async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        prepareInstall(model, in: directory) { _, _, _, _, _ in }
        await offerAndInstall(model, store: store)
        let pending = try #require(model.loadPendingSwap())
        let id = try #require(pending.operationId)

        // Long after the handover, where the ordinary installation would settle it, and with no
        // note, because the helper never writes into a relocated run's support folder.
        let (relocated, history) = relaunch(after: model, preferences: preferences, store: store, as: version)
        relocated.isIsolatedRun = true
        relocated.supportDirectory = directory.appending(path: "relocated-state")
        relocated.now = { Date().addingTimeInterval(AppUpdates.helperLifetime + 1) }
        relocated.confirmLaunch()
        #expect(relocated.failure == nil)
        #expect(relocated.loadPendingSwap() == pending)
        #expect(history.record(id: id)?.state == .unknown)
        // Its own marker is still written, in its own support folder.
        #expect(FileManager.default.fileExists(atPath: AppUpdates.launchMarkerURL(support: relocated.supportDirectory).path))
    }

    /// What the helper writes down as it ends, as its `finish` spells it: the swap's id and the
    /// status it exits with, beside the marker. `AppUpdateTests` holds the real helper to this.
    static func helperEnds(_ status: Int32, swap id: String, marker: URL) throws {
        let support = marker.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        try Data("\(id) \(status)\n".utf8).write(to: AppUpdates.swapOutcomeURL(support: support))
    }

    @Test("a launch that is not the new build records the install as failed, as the helper says, and keeps the offer",
          arguments: [
              (AppUpdates.SwapOutcome.putBack, "3.1.0 did not come up, so 3.0.0 is still the build that runs"),
              (AppUpdates.SwapOutcome.notMoved, "could not move the bundles, so Legion Control 3.1.0 was not installed"),
              (AppUpdates.SwapOutcome.notQuit, "did not quit, so the update helper changed nothing"),
              (Int32(9), "stopped with status 9")
          ])
    func installReversedByNextLaunch(status: Int32, says: String) async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        prepareInstall(model, in: directory) { _, _, marker, id, _ in try Self.helperEnds(status, swap: id, marker: marker) }
        await offerAndInstall(model, store: store)
        let id = try #require(model.loadPendingSwap()?.operationId)

        // The old build is what comes up, and the helper has said why.
        let (next, history) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        next.confirmLaunch()
        #expect(history.record(id: id)?.state == .failed)
        #expect(next.check == .available(Self.release))
        #expect(next.failure?.contains(says) == true, "\(next.failure ?? "no failure")")
        #expect(next.loadPendingSwap() == nil)
        #expect(AppUpdates.swapOutcome(support: next.supportDirectory) == nil)
    }

    @Test("with no word from the helper, a launch leaves a fresh swap for later and calls a long-gone one unconfirmed")
    func installWithoutOutcome() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        // What another swap's helper left behind is no word about this one.
        prepareInstall(model, in: directory) { _, _, marker, _, _ in
            try Self.helperEnds(AppUpdates.SwapOutcome.landed, swap: "another-swap", marker: marker)
        }
        await offerAndInstall(model, store: store)
        let id = try #require(model.loadPendingSwap()?.operationId)

        // A copy of the old build started while the helper may still be at work decides nothing.
        let (early, earlyHistory) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        early.confirmLaunch()
        #expect(early.failure == nil)
        #expect(early.loadPendingSwap()?.id == id)
        // Still owed an answer, as anything left running by the last run is, and not failed.
        #expect(earlyHistory.record(id: id)?.state == .unknown)

        // Long after the helper could have finished, it never said, and that is what is reported.
        let (late, lateHistory) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        late.now = { Date().addingTimeInterval(AppUpdates.helperLifetime + 1) }
        late.confirmLaunch()
        #expect(late.failure?.contains("3.1.0 was not confirmed") == true)
        #expect(late.loadPendingSwap() == nil)
        #expect(lateHistory.record(id: id)?.state == .failed)
    }

    @Test("a swap that cannot be handed over keeps the offer and deletes the staged bundle")
    func failedHandoffRestores() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let staged = prepareInstall(model, in: directory) { _, _, _, _, _ in
            throw AppUpdates.InstallFailure("The update helper could not be started.")
        }
        await offerAndInstall(model, store: store)

        #expect(model.phase == .idle)
        #expect(model.failure == "The update helper could not be started.")
        #expect(store.newestFirst.first?.state == .failed)
        #expect(model.loadPendingSwap() == nil)
        #expect(!FileManager.default.fileExists(atPath: staged.path))
        #expect(model.check == .available(Self.release))
        #expect(AppUpdateModel(preferences: preferences, installedVersion: "3.0.0").check == .available(Self.release))
    }

    @Test("a helper that gives up while this build still runs means nothing was installed")
    func helperGivingUpRestores() async throws {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        var helperExited: (@Sendable (Int32) -> Void)?
        let staged = prepareInstall(model, in: directory) { _, _, _, _, onExit in helperExited = onExit }
        // quit() is answered by nothing: this build is still running when the helper gives up.
        await offerAndInstall(model, store: store)
        let exit = try #require(helperExited)
        #expect(model.phase == .installing)

        exit(3)
        let deadline = Date().addingTimeInterval(2)
        while model.phase != .idle, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
        #expect(model.phase == .idle)
        #expect(model.failure?.contains("did not quit") == true)
        #expect(store.newestFirst.first?.state == .failed)
        #expect(model.loadPendingSwap() == nil)
        #expect(!FileManager.default.fileExists(atPath: staged.path))
        #expect(model.check == .available(Self.release))
    }

    @Test("an answer another build wrote down is asked again rather than shown")
    func cacheBelongsToTheBuildThatAsked() async {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        model.fetchRelease = { _, _ in .upToDate(note: nil) }
        model.checkNow()
        await settle(model)
        #expect(model.check == .upToDate(note: nil))

        // A roll back to 2.9.0 must not come up saying 2.9.0 is up to date.
        let older = AppUpdateModel(preferences: preferences, installedVersion: "2.9.0")
        #expect(older.check == nil)
        #expect(older.lastChecked == nil)
        var calls = 0
        older.fetchRelease = { _, _ in calls += 1; return .available(Self.release) }
        older.checkIfStale()
        await settle(older)
        #expect(calls == 1)
        #expect(older.availableVersion == "3.1.0")
    }

    @Test("a roll back goes through the swap seam and is checked for by the next launch")
    func rollBackIsConfirmed() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let previous = directory.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        model.fetchRelease = { _, _ in .upToDate(note: nil) }
        model.checkNow()
        await settle(model)
        #expect(preferences.data(forKey: "appUpdateLastCheck") != nil)
        var handed: [AppUpdates.Staged] = []
        var quits = 0
        model.performSwap = { staged, _, _, _, _ in handed.append(staged) }
        model.quit = { quits += 1 }
        model.rollBack()
        #expect(handed.map(\.bundle) == [previous])
        #expect(quits == 1)
        let pending = try #require(model.loadPendingSwap())
        #expect(pending.version == "2.9.0")
        // A released 2.9.0 would take this build's "up to date" as its own.
        #expect(preferences.data(forKey: "appUpdateLastCheck") == nil)

        // The kept build coming up is not the helper's word either, and settles once there is one.
        let (back, _) = relaunch(after: model, preferences: preferences, store: store, as: "2.9.0")
        back.settleInterval = .milliseconds(5)
        back.confirmLaunch()
        #expect(back.loadPendingSwap() == pending)
        try Self.helperEnds(AppUpdates.SwapOutcome.landed, swap: pending.id,
                            marker: AppUpdates.launchMarkerURL(support: back.supportDirectory))
        await eventually { back.loadPendingSwap() == nil }
        #expect(back.failure == nil)
        #expect(back.loadPendingSwap() == nil)

        // Asked again from 3.0.0, and this time the kept build does not come up.
        let (again, _) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        again.performSwap = { _, _, marker, id, _ in try Self.helperEnds(AppUpdates.SwapOutcome.putBack, swap: id, marker: marker) }
        again.quit = { }
        again.rollBack()
        #expect(again.loadPendingSwap()?.version == "2.9.0")
        let (stuck, _) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        stuck.confirmLaunch()
        #expect(stuck.failure?.contains("2.9.0 did not come up") == true)
        // The kept build is what was handed in, so nothing here deletes it.
        #expect(FileManager.default.fileExists(atPath: previous.path))
    }

    @Test("a roll back that lands deletes the build it replaced, and no other bundle under that name",
          arguments: [nil, "3.1.0"] as [String?])
    func rollBackDiscardsReplacedBuild(otherBuild: String?) async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        try AppUpdateTests.makeBundle(at: model.bundleURL, identifier: "id", executable: "LegionControl", version: "3.0.0")
        let previous = directory.appending(path: AppUpdates.previousName)
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        let replaced = directory.appending(path: AppUpdates.stagedName)
        // What the helper leaves once the kept build is up: that build at the real path, and the
        // one it replaced under the staged name.
        model.performSwap = { staged, current, marker, id, _ in
            try FileManager.default.moveItem(at: current, to: staged.previous)
            try FileManager.default.moveItem(at: staged.bundle, to: current)
            // Or some other build under that name, which is not this roll back's to delete.
            if let otherBuild {
                try FileManager.default.removeItem(at: staged.previous)
                try AppUpdateTests.makeBundle(at: staged.previous, identifier: "id", executable: "LegionControl",
                                              version: otherBuild)
            }
            try Self.helperEnds(AppUpdates.SwapOutcome.landed, swap: id, marker: marker)
        }
        model.quit = { }
        model.rollBack()
        #expect(AppUpdates.version(ofBundleAt: replaced) == otherBuild ?? "3.0.0")

        let (back, _) = relaunch(after: model, preferences: preferences, store: store, as: "2.9.0")
        back.confirmLaunch()
        #expect(back.loadPendingSwap() == nil)
        #expect(back.failure == nil)
        #expect(AppUpdates.version(ofBundleAt: replaced) == otherBuild)
        #expect(AppUpdates.version(ofBundleAt: back.bundleURL) == "2.9.0")
        #expect(AppUpdates.retainedPrevious(beside: back.bundleURL) == nil)
    }

    @Test("a roll back into a build that never settles it is not reported as failed by a later launch")
    func rollBackIntoOlderReleaseLands() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = OperationStore(url: directory.appending(path: "operations.json"))
        let previous = directory.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        // The helper sees the kept build come up and says so. That build, like a 1.3 release, never
        // reads the record.
        model.performSwap = { _, _, marker, id, _ in try Self.helperEnds(AppUpdates.SwapOutcome.landed, swap: id, marker: marker) }
        model.quit = { }
        model.rollBack()
        #expect(model.loadPendingSwap()?.version == "2.9.0")
        _ = relaunch(after: model, preferences: preferences, store: store, as: "2.9.0")

        // Later 2.9.0 updates to 3.0.0 again, or 3.0.0 is reinstalled.
        let (later, _) = relaunch(after: model, preferences: preferences, store: store, as: "3.0.0")
        later.confirmLaunch()
        #expect(later.failure == nil)
        #expect(later.loadPendingSwap() == nil)
        #expect(AppUpdates.swapOutcome(support: later.supportDirectory) == nil)
    }

    @Test("a roll back that is never handed over, or that the helper gives up on, keeps this build's answer")
    func rollBackNotHandedOverKeepsAnswer() async throws {
        let (model, preferences, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let previous = directory.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        model.fetchRelease = { _, _ in .upToDate(note: "Checked.") }
        model.checkNow()
        await settle(model)
        let answer = try #require(preferences.data(forKey: "appUpdateLastCheck"))

        model.performSwap = { _, _, _, _, _ in throw AppUpdates.InstallFailure("The update helper could not be started.") }
        model.quit = { Issue.record("quit without a handover") }
        model.rollBack()
        #expect(model.failure == "The update helper could not be started.")
        #expect(model.loadPendingSwap() == nil)
        #expect(preferences.data(forKey: "appUpdateLastCheck") == answer)

        var helperExited: (@Sendable (Int32) -> Void)?
        model.performSwap = { _, _, _, _, onExit in helperExited = onExit }
        model.quit = { }
        model.rollBack()
        #expect(preferences.data(forKey: "appUpdateLastCheck") == nil)
        let exit = try #require(helperExited)
        exit(AppUpdates.SwapOutcome.notQuit)
        let deadline = Date().addingTimeInterval(2)
        while model.phase != .idle, Date() < deadline { try? await Task.sleep(for: .milliseconds(5)) }
        #expect(model.failure?.contains("2.9.0 was not put back") == true)
        #expect(AppUpdateModel(preferences: preferences, installedVersion: "3.0.0").check == .upToDate(note: "Checked."))
    }

    @Test("a configured repository that is not owner/name is shown as replaced by the built-in one")
    func repositoryWarning() {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let repository = Repository()
        model.repo = { repository.value }
        #expect(model.repository == "a/a")
        #expect(model.repositoryWarning == nil)
        repository.value = "a/a/../../b"
        #expect(model.repository == ControllerConfig.AppUpdatesConfig.defaultRepo)
        #expect(model.repositoryWarning?.contains(ControllerConfig.AppUpdatesConfig.defaultRepo) == true)
    }

    @Test("an isolated run never installs, rolls back or discards a build")
    func isolatedRunRefusesSwaps() async throws {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        model.isIsolatedRun = true
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        // Looking is still fine; it is changing the installation that is refused.
        #expect(model.availableVersion == "3.1.0")
        let previous = directory.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        var downloads = 0
        var swaps = 0
        var quits = 0
        model.downloadRelease = { _ in downloads += 1; throw AppUpdates.InstallFailure("not reached") }
        model.performSwap = { _, _, _, _, _ in swaps += 1 }
        model.quit = { quits += 1 }

        #expect(model.installBlockedReason == model.swapBlockedReason)
        model.install()
        model.rollBack()
        model.discardPrevious()
        await settle(model)
        #expect(downloads == 0)
        #expect(swaps == 0)
        #expect(quits == 0)
        #expect(model.failure == model.swapBlockedReason)
        #expect(model.rollbackVersion == "2.9.0")
        #expect(FileManager.default.fileExists(atPath: previous.path))
    }

    @Test("discarding the kept build deletes it, and with it the way back")
    func discardPrevious() throws {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        try AppUpdateTests.makeBundle(at: model.bundleURL, identifier: "id", executable: "LegionControl", version: "3.0.0")
        let previous = directory.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        var redraws = 0
        model.onStateChange = { redraws += 1 }
        #expect(model.rollbackVersion == "2.9.0")

        model.discardPrevious()
        #expect(!FileManager.default.fileExists(atPath: previous.path))
        #expect(model.rollbackVersion == nil)
        #expect(model.failure == nil)
        #expect(redraws == 1)
        // The running build is never what goes.
        #expect(AppUpdates.version(ofBundleAt: model.bundleURL) == "3.0.0")
        // Nothing left to discard is not an error either.
        model.discardPrevious()
        #expect(model.failure == nil)
    }

    @Test("a copy started from the kept build neither discards, goes back to nor installs over itself")
    func copyRunningFromKeptBuild() async throws {
        let (model, _, directory) = fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        try AppUpdateTests.makeBundle(at: model.bundleURL, identifier: "id", executable: "LegionControl", version: "3.0.0")
        let previous = directory.appending(path: "Legion Control.previous.app")
        try AppUpdateTests.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "2.9.0")
        // As Bundle.main spells it, with a trailing slash.
        model.bundleURL = directory.appending(path: "Legion Control.previous.app", directoryHint: .isDirectory)
        model.fetchRelease = { _, _ in .available(Self.release) }
        model.checkNow()
        await settle(model)
        var downloads = 0
        var swaps = 0
        model.downloadRelease = { _ in downloads += 1; throw AppUpdates.InstallFailure("not reached") }
        model.performSwap = { _, _, _, _, _ in swaps += 1 }
        model.quit = { Issue.record("quit from the kept build") }

        #expect(model.rollbackVersion == nil)
        #expect(model.installBlockedReason?.contains("Legion Control.previous.app") == true)
        model.discardPrevious()
        model.rollBack()
        model.install()
        await settle(model)
        #expect(downloads == 0)
        #expect(swaps == 0)
        #expect(AppUpdates.version(ofBundleAt: previous) == "2.9.0")
        #expect(AppUpdates.version(ofBundleAt: directory.appending(path: "Legion Control.app")) == "3.0.0")
    }
}
