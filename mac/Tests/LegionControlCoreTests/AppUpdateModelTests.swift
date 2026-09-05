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
        model.performSwap = { _, _, _ in swaps += 1 }
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
        model.performSwap = { _, _, actualMarker in
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
        #expect(model.installBlockedReason != nil)
        var calls = 0
        model.fetchRelease = { _, _ in calls += 1; return .available(Self.release) }
        model.checkIfStale()
        await settle(model)
        #expect(calls == 1)
        #expect(model.installBlockedReason == nil)
    }
}
