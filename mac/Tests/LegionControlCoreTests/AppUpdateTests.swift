import Foundation
import Testing
@testable import LegionControlCore

/// Replacing the app with a newer one without leaving the user without an app.
///
/// The previous version of this rsynced a downloaded bundle over the one it was running out of after
/// checking a version string. Every one of these tests is about a way that goes wrong.
struct AppUpdateTests {

    // MARK: - Reading a release

    static func release(assets: [(String, Int)]) -> Data {
        let json: [String: Any] = [
            "tag_name": "v1.4.0",
            "body": "First paragraph.\n\nSecond.",
            "assets": assets.map { name, size in
                ["name": name, "url": "https://example.invalid/\(name)", "size": size] as [String: Any]
            }
        ]
        return try! JSONSerialization.data(withJSONObject: json)
    }

    @Test("the platform asset is chosen by exact name, never by suffix")
    func exactAssetName() throws {
        // "The first zip attached to the release" is how a Windows build gets installed on a Mac.
        let data = Self.release(assets: [
            ("Legion-Control-windows-x64.zip", 10),
            ("Legion-Control-macos-arm64.zip", 20),
            ("Legion-Control-manifest.json", 1),
            ("Legion-Control-manifest.json.sig", 1)
        ])
        guard case .available(let release) = AppUpdates.read(releaseJSON: data, installedVersion: "1.3.0") else {
            Issue.record("expected an available release")
            return
        }
        #expect(release.assetURL.lastPathComponent == "Legion-Control-macos-arm64.zip")
        #expect(release.sizeBytes == 20)
        #expect(release.isVerifiable)
        #expect(release.notes == "First paragraph.")
    }

    @Test("a release with no signed manifest is reported as unverifiable rather than installed")
    func unsignedRelease() throws {
        let data = Self.release(assets: [("Legion-Control-macos-arm64.zip", 20)])
        guard case .available(let release) = AppUpdates.read(releaseJSON: data, installedVersion: "1.3.0") else {
            Issue.record("expected an available release")
            return
        }
        // The release is still offered — there may be a good reason to know it exists — but the
        // install path refuses it, and the reason says so rather than failing at the end.
        #expect(!release.isVerifiable)
    }

    @Test("a release with no build for this platform is a clear failure, not a silent no-op")
    func missingPlatformAsset() {
        let data = Self.release(assets: [("Legion-Control-linux-x64.tar.gz", 20)])
        guard case .failed(let reason) = AppUpdates.read(releaseJSON: data, installedVersion: "1.3.0") else {
            Issue.record("expected a failure")
            return
        }
        #expect(reason.contains("Legion-Control-macos-arm64.zip"))
    }

    @Test("versions are compared numerically, field by field")
    func versionComparison() {
        #expect(AppUpdates.isNewer("1.0.10", than: "1.0.9"))
        #expect(!AppUpdates.isNewer("1.0.9", than: "1.0.10"))
        #expect(AppUpdates.isNewer("1.3.0", than: "1.2.9"))
        #expect(!AppUpdates.isNewer("1.3.0", than: "1.3.0"))
        #expect(AppUpdates.version(fromTag: "v1.3.0") == "1.3.0")
        #expect(AppUpdates.version(fromTag: "1.3.0") == "1.3.0")
    }

    // MARK: - Checking the staged bundle

    /// A bundle on disk, complete enough for the checks to run against.
    static func makeBundle(at url: URL, identifier: String?, executable: String?, version: String?) throws {
        try FileManager.default.createDirectory(at: url.appending(path: "Contents/MacOS"),
                                                withIntermediateDirectories: true)
        var info: [String: Any] = [:]
        if let identifier { info["CFBundleIdentifier"] = identifier }
        if let executable { info["CFBundleExecutable"] = executable }
        if let version { info["CFBundleShortVersionString"] = version }
        let data = try PropertyListSerialization.data(fromPropertyList: info, format: .xml, options: 0)
        try data.write(to: url.appending(path: "Contents/Info.plist"))

        if let executable {
            // A real Mach-O, so the architecture check has something to read. Copying the running
            // test binary is the simplest thing that is genuinely a binary for this machine.
            let source = URL(fileURLWithPath: "/bin/echo")
            try FileManager.default.copyItem(at: source, to: url.appending(path: "Contents/MacOS/\(executable)"))
        }
    }

    static func temporaryDirectory() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-update-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    @Test("a replacement with the wrong bundle id is refused")
    func wrongBundleIdentifier() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let candidate = root.appending(path: "Other.app")
        try Self.makeBundle(at: current, identifier: "com.x1f4r.legion-control", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: candidate, identifier: "com.someone.else", executable: "LegionControl", version: "1.4.0")

        await #expect(throws: AppUpdates.InstallFailure.self) {
            try await AppUpdates.verify(candidate, is: "1.4.0", against: current)
        }
    }

    @Test("a replacement whose version does not match the release is refused")
    func wrongVersion() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let candidate = root.appending(path: "New.app")
        try Self.makeBundle(at: current, identifier: "com.x1f4r.legion-control", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: candidate, identifier: "com.x1f4r.legion-control", executable: "LegionControl", version: "1.3.9")

        await #expect(throws: AppUpdates.InstallFailure.self) {
            try await AppUpdates.verify(candidate, is: "1.4.0", against: current)
        }
    }

    @Test("a replacement with no executable where this app has one is refused")
    func missingExecutable() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let candidate = root.appending(path: "New.app")
        try Self.makeBundle(at: current, identifier: "com.x1f4r.legion-control", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: candidate, identifier: "com.x1f4r.legion-control", executable: nil, version: "1.4.0")

        await #expect(throws: AppUpdates.InstallFailure.self) {
            try await AppUpdates.verify(candidate, is: "1.4.0", against: current)
        }
    }

    @Test("a complete, matching replacement passes every check")
    func goodReplacement() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let candidate = root.appending(path: "New.app")
        try Self.makeBundle(at: current, identifier: "com.x1f4r.legion-control", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: candidate, identifier: "com.x1f4r.legion-control", executable: "LegionControl", version: "1.4.0")

        try await AppUpdates.verify(candidate, is: "1.4.0", against: current)
    }

    // MARK: - The swap

    @Test("the swap helper leaves exactly one whole bundle at the real path, whatever happens")
    func swapHelperSucceeds() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: staged, identifier: "id", executable: "LegionControl", version: "1.4.0")

        // The marker is written straight away, standing in for a new build that came up.
        try Data("ok".utf8).write(to: marker)
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: 1, launchTimeout: 2)
        // The marker is removed by the helper before it launches, so it has to be recreated by
        // something. A background writer stands in for the new build's own launch.
        let scriptURL = root.appending(path: "swap.sh")
        try Data(script.utf8).write(to: scriptURL)

        let writer = Task.detached {
            try? await Task.sleep(for: .milliseconds(300))
            try? Data("ok".utf8).write(to: marker)
        }
        let run = await Shell.run(executable: "/bin/sh", arguments: [scriptURL.path], timeout: 30)
        _ = await writer.result

        #expect(run.exitCode == 0)
        // The new build is at the real path and the old one is kept beside it.
        #expect(AppUpdates.version(ofBundleAt: current) == "1.4.0")
        #expect(AppUpdates.version(ofBundleAt: previous) == "1.3.0")
        #expect(!FileManager.default.fileExists(atPath: staged.path))
    }

    @Test("a replacement that never comes up is rolled back by the helper")
    func swapHelperRecovers() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: staged, identifier: "id", executable: "LegionControl", version: "1.4.0")

        // Nothing ever writes the marker: the new build crashed on the way up.
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: 1, launchTimeout: 1)
        let scriptURL = root.appending(path: "swap.sh")
        try Data(script.utf8).write(to: scriptURL)

        let run = await Shell.run(executable: "/bin/sh", arguments: [scriptURL.path], timeout: 30)
        #expect(run.exitCode == 2)
        // The build that worked is back at the real path, and the one that did not is kept for
        // diagnosis rather than deleted.
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
        #expect(AppUpdates.version(ofBundleAt: staged) == "1.4.0")
        #expect(!FileManager.default.fileExists(atPath: previous.path))
    }

    @Test("the helper puts everything back when the staged bundle cannot be moved into place")
    func swapHelperHandlesMissingStage() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")   // deliberately never created
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")

        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: 1, launchTimeout: 1)
        let scriptURL = root.appending(path: "swap.sh")
        try Data(script.utf8).write(to: scriptURL)

        let run = await Shell.run(executable: "/bin/sh", arguments: [scriptURL.path], timeout: 30)
        #expect(run.exitCode == 1)
        // The one window where the real path could hold nothing is closed by putting the old bundle
        // straight back.
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
    }

    @Test("the kept bundle is found and named for the roll-back action")
    func retainedPrevious() throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.4.0")
        #expect(AppUpdates.retainedPrevious(beside: current) == nil)

        try Self.makeBundle(at: previous, identifier: "id", executable: "LegionControl", version: "1.3.0")
        #expect(AppUpdates.retainedPrevious(beside: current) == previous)
        #expect(AppUpdates.version(ofBundleAt: previous) == "1.3.0")
    }

    @Test("the launch marker is written where the helper looks for it")
    func launchMarker() throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        AppUpdates.confirmLaunch(support: root)
        #expect(FileManager.default.fileExists(atPath: AppUpdates.launchMarkerURL(support: root).path))
    }
}
