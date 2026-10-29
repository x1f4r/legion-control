import CryptoKit
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

    @Test("a tag or a remembered version of any shape compares without crashing", arguments: [
        "", "-", "--", "v", "-1", "1.4.0-", "1.4.0-beta.2", "1..4", ".", "abc", "１.２", "99999999999999999999.1"
    ])
    func oddVersions(_ value: String) {
        // The tag comes from whichever repository the setup names, and the setup from any peer.
        _ = AppUpdates.isNewer(value, than: "1.3.0")
        _ = AppUpdates.isNewer("1.3.0", than: value)
        _ = AppUpdates.isNewer(value, than: value)
        #expect(!AppUpdates.isNewer(value, than: value))
    }

    @Test("an empty or dash-only version is no version at all, and a pre-release reads as its release")
    func versionEdges() {
        #expect(!AppUpdates.isNewer("", than: "0"))
        #expect(!AppUpdates.isNewer("-", than: "0.0.1") && AppUpdates.isNewer("0.0.1", than: "-"))
        #expect(!AppUpdates.isNewer("1.4.0-beta", than: "1.4.0"))
        #expect(AppUpdates.isNewer("1.4.1-rc.1", than: "1.4.0"))
    }

    @Test("only a GitHub owner/name is asked; anything else falls back to the built-in repository with a warning")
    func repositoryGrammar() {
        let builtIn = ControllerConfig.AppUpdatesConfig.defaultRepo
        #expect(AppUpdates.source(for: "someone/a-fork") == ("someone/a-fork", nil))
        #expect(AppUpdates.source(for: "").repository == builtIn)
        #expect(AppUpdates.source(for: "").warning == nil)
        #expect(AppUpdates.source(for: "   ").warning == nil)
        for hostile in ["someone", "someone/", "/name", "-someone/name", "someone/..", "someone/.",
                        "some one/name", "someone/name/releases", "someone/name?x=1", "someone/name#x",
                        "someone/name\n", "../../orgs/x", "someone/na%2Fme", "someone/n\u{202E}ame"] {
            let source = AppUpdates.source(for: hostile)
            #expect(source.repository == builtIn, "\(hostile)")
            #expect(source.warning != nil, "\(hostile)")
        }
    }

    // MARK: - Downloading within bounds

    /// A release served by the stub: a manifest naming the Mac archive with `declaredSize`, signed
    /// with a key of the test's own, its signature, and the archive answered by `archiveRoute`.
    struct ServedRelease {
        var release: AppUpdates.Release
        var key: Curve25519.Signing.PublicKey
        var urls: [URL]
        func forget() { StubURLProtocol.forget(urls) }
    }

    static func serveRelease(archive: Data, declaredSize: Int64? = nil, manifestPadding: Int = 0,
                             signaturePadding: Int = 0, archiveRoute: StubURLProtocol.Route? = nil) throws -> ServedRelease {
        let base = URL(string: "https://stub.example/\(UUID().uuidString)")!
        let manifestURL = base.appending(path: "manifest.json")
        let signatureURL = base.appending(path: "manifest.json.sig")
        let archiveURL = base.appending(path: AppUpdates.assetName)
        var manifest: [String: Any] = [
            "schema": 1, "version": "1.4.0",
            "artifacts": [["name": AppUpdates.assetName, "sha256": ReleaseTrust.sha256(of: archive),
                           "size": declaredSize ?? Int64(archive.count)]]
        ]
        if manifestPadding > 0 { manifest["padding"] = String(repeating: "x", count: manifestPadding) }
        let manifestBytes = try JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys])
        let signer = Curve25519.Signing.PrivateKey()
        let signature = Data((try signer.signature(for: manifestBytes).base64EncodedString()
                              + String(repeating: " ", count: signaturePadding) + "\n").utf8)
        StubURLProtocol.serve(manifestURL, .init(chunks: Self.chunked(manifestBytes)))
        StubURLProtocol.serve(signatureURL, .init(chunks: [signature]))
        StubURLProtocol.serve(archiveURL, archiveRoute ?? .init(chunks: Self.chunked(archive)))
        return ServedRelease(
            release: AppUpdates.Release(version: "1.4.0", notes: "", assetURL: archiveURL, sizeBytes: Int64(archive.count),
                                        manifestURL: manifestURL, signatureURL: signatureURL),
            key: signer.publicKey,
            urls: [manifestURL, signatureURL, archiveURL]
        )
    }

    /// Bodies arrive in pieces, as they do off a network.
    static func chunked(_ data: Data, size: Int = 64 << 10) -> [Data] {
        stride(from: 0, to: data.count, by: size).map { data.subdata(in: $0..<min($0 + size, data.count)) }
    }

    static func randomBytes(_ count: Int) -> Data {
        Data((0..<count).map { _ in UInt8.random(in: 0...255) })
    }

    @Test("a signed archive streams to a file and checks out against the manifest")
    func downloadStreamsToFile() async throws {
        let parent = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: parent) }
        let archive = Self.randomBytes(300_000)
        let served = try Self.serveRelease(archive: archive)
        defer { served.forget() }

        let download = try await AppUpdates.download(served.release, session: StubURLProtocol.session(),
                                                     key: served.key, into: parent)
        #expect(try Data(contentsOf: download.archive) == archive)
        #expect(download.artifact.size == Int64(archive.count))
        AppUpdates.discard(download)
        #expect(try FileManager.default.contentsOfDirectory(atPath: parent.path).isEmpty)
    }

    @Test("an archive that runs past its signed size is stopped and deleted, not written out and refused afterwards")
    func downloadStopsPastSignedSize() async throws {
        let parent = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: parent) }
        let archive = Self.randomBytes(200_000)
        // The server keeps going well past what the manifest promised, and never says how much.
        let served = try Self.serveRelease(archive: archive,
                                           archiveRoute: .init(chunks: Self.chunked(archive + Self.randomBytes(4 << 20))))
        defer { served.forget() }

        await #expect {
            try await AppUpdates.download(served.release, session: StubURLProtocol.session(), key: served.key, into: parent)
        } throws: { error in
            (error as? AppUpdates.InstallFailure)?.message.contains("ran past the 200000 bytes") == true
        }
        #expect(try FileManager.default.contentsOfDirectory(atPath: parent.path).isEmpty)
    }

    @Test("an archive whose announced length is larger than its signed size is refused before it is read")
    func downloadRefusesAnnouncedOversize() async throws {
        let parent = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: parent) }
        let archive = Self.randomBytes(1000)
        let served = try Self.serveRelease(archive: archive, archiveRoute: .init(
            headers: ["Content-Length": "\(1 << 30)"], chunks: Self.chunked(archive)))
        defer { served.forget() }

        await #expect(throws: AppUpdates.InstallFailure.self) {
            try await AppUpdates.download(served.release, session: StubURLProtocol.session(), key: served.key, into: parent)
        }
        #expect(try FileManager.default.contentsOfDirectory(atPath: parent.path).isEmpty)
    }

    @Test("a manifest over 256 KiB is refused, and nothing after it is asked for")
    func manifestOverLimit() async throws {
        let parent = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: parent) }
        let served = try Self.serveRelease(archive: Self.randomBytes(1000), manifestPadding: 256 << 10)
        defer { served.forget() }

        await #expect {
            try await AppUpdates.download(served.release, session: StubURLProtocol.session(), key: served.key, into: parent)
        } throws: { error in
            (error as? AppUpdates.InstallFailure)?.message.contains("256 KiB") == true
        }
        #expect(!StubURLProtocol.wasRequested(served.urls[1]))
        #expect(!StubURLProtocol.wasRequested(served.urls[2]))
        #expect(try FileManager.default.contentsOfDirectory(atPath: parent.path).isEmpty)
    }

    @Test("a signature over 4 KiB is refused before the archive is asked for")
    func signatureOverLimit() async throws {
        let parent = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: parent) }
        let served = try Self.serveRelease(archive: Self.randomBytes(1000), signaturePadding: 4 << 10)
        defer { served.forget() }

        await #expect {
            try await AppUpdates.download(served.release, session: StubURLProtocol.session(), key: served.key, into: parent)
        } throws: { error in
            (error as? AppUpdates.InstallFailure)?.message.contains("4 KiB") == true
        }
        #expect(!StubURLProtocol.wasRequested(served.urls[2]))
    }

    @Test("a signed manifest that gives the archive no size installs nothing and downloads nothing")
    func zeroSizeArtifact() async throws {
        let parent = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: parent) }
        let served = try Self.serveRelease(archive: Self.randomBytes(1000), declaredSize: 0)
        defer { served.forget() }

        await #expect(throws: AppUpdates.InstallFailure.self) {
            try await AppUpdates.download(served.release, session: StubURLProtocol.session(), key: served.key, into: parent)
        }
        #expect(!StubURLProtocol.wasRequested(served.urls[2]))
        #expect(try FileManager.default.contentsOfDirectory(atPath: parent.path).isEmpty)
    }

    @Test("a release reply over 1 MiB is a failed check, not a megabyte of memory")
    func releaseReplyOverLimit() async throws {
        let repository = "someone/oversized-\(UUID().uuidString.prefix(8))"
        let url = URL(string: "https://api.github.com/repos/\(repository)/releases/latest")!
        let reply = Data(#"{"tag_name":"v9.9.9","body":""#.utf8) + Data(repeating: 0x61, count: 1 << 20) + Data(#""}"#.utf8)
        StubURLProtocol.serve(url, .init(chunks: Self.chunked(reply)))
        defer { StubURLProtocol.forget([url]) }

        let result = await AppUpdates.check(repo: repository, installedVersion: "1.3.0", session: StubURLProtocol.session())
        guard case .failed(let reason) = result else {
            Issue.record("expected a failed check, got \(result)")
            return
        }
        #expect(reason.contains("1 MiB"))
    }

    @Test("a repository that is not owner/name is never pasted into a URL; the built-in one is asked")
    func checkFallsBackToBuiltIn() async throws {
        let builtIn = URL(string: "https://api.github.com/repos/\(ControllerConfig.AppUpdatesConfig.defaultRepo)/releases/latest")!
        StubURLProtocol.serve(builtIn, .init(chunks: [Data(#"{"tag_name":"v1.0.0","assets":[]}"#.utf8)]))
        defer { StubURLProtocol.forget([builtIn]) }

        let result = await AppUpdates.check(repo: "someone/../../orgs/x", installedVersion: "1.3.0",
                                            session: StubURLProtocol.session())
        #expect(result == .upToDate(note: nil))
        #expect(StubURLProtocol.wasRequested(builtIn))
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
    //
    // None of these lets the helper near LaunchServices. Every one hands it a launcher that writes
    // down what it was asked to start, because the real `open -n` pointed at these fake bundles has
    // macOS report each one as an app that quit unexpectedly, in front of whoever runs the tests.

    /// A stand-in for `open -n`: it writes each bundle it is asked to start to a log, then runs
    /// `then` with the bundle as `$1` and the log as `$LOG`.
    static func recordingLauncher(in root: URL, then script: String = "") throws -> (argv: [String], log: URL) {
        let log = root.appending(path: "launched.log")
        let launcher = root.appending(path: "launch.sh")
        let text = "LOG=\(CommandGrammar.posixQuoted(log.path))\nprintf '%s\\n' \"$1\" >> \"$LOG\"\n" + script
        try Data(text.utf8).write(to: launcher)
        return (["/bin/sh", launcher.path], log)
    }

    /// What the recording launcher was asked to start, in order.
    static func launched(_ log: URL) -> [String] {
        ((try? String(contentsOf: log, encoding: .utf8)) ?? "").split(separator: "\n").map(String.init)
    }

    static func runHelper(_ script: String, in root: URL) async throws -> CommandResult {
        let scriptURL = root.appending(path: "swap.sh")
        try Data(script.utf8).write(to: scriptURL)
        return await Shell.run(executable: "/bin/sh", arguments: [scriptURL.path], timeout: 30)
    }

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

        // The old build's own proof is there before the swap, and the helper has to clear it: a
        // marker left over from the last launch must never read as the new build coming up.
        try Data("ok".utf8).write(to: marker)
        // The launch stands in for the new build coming up and writing its marker. It can only
        // happen after the helper has cleared the old one, so nothing races the helper's own rm.
        let marked = CommandGrammar.posixQuoted(marker.path)
        let launch = try Self.recordingLauncher(in: root, then: """
            [ -f \(marked) ] && echo stale-marker >> "$LOG"
            printf ok > \(marked)

            """)
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: try Self.exitedProcessID(), swapID: "swap-1",
                                           launchTimeout: 2, launcher: launch.argv)
        let run = try await Self.runHelper(script, in: root)

        #expect(run.exitCode == 0)
        #expect(Self.launched(launch.log) == [current.path(percentEncoded: false)])
        // The new build is at the real path and the old one is kept beside it.
        #expect(AppUpdates.version(ofBundleAt: current) == "1.4.0")
        #expect(AppUpdates.version(ofBundleAt: previous) == "1.3.0")
        #expect(!FileManager.default.fileExists(atPath: staged.path))
        #expect(AppUpdates.swapOutcome(support: root) == .init(id: "swap-1", status: AppUpdates.SwapOutcome.landed))
    }

    /// A launcher step that copies whatever outcome the helper has written so far to `seen`, so a
    /// test can tell what the build being started would have found.
    static func notingOutcome(support: URL, into seen: URL) -> String {
        "cat \(CommandGrammar.posixQuoted(AppUpdates.swapOutcomeURL(support: support).path)) >> \(CommandGrammar.posixQuoted(seen.path)) 2>/dev/null || true\n"
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
        let seen = root.appending(path: "seen")
        let launch = try Self.recordingLauncher(in: root, then: Self.notingOutcome(support: root, into: seen))
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: try Self.exitedProcessID(), swapID: "swap-1",
                                           launchTimeout: 1, launcher: launch.argv)
        let run = try await Self.runHelper(script, in: root)
        #expect(run.exitCode == 2)
        // The new build was started once, and then the old one, back at the same path.
        #expect(Self.launched(launch.log) == [current.path(percentEncoded: false), current.path(percentEncoded: false)])
        // The build that worked is back at the real path, and the one that did not is kept for
        // diagnosis rather than deleted.
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
        #expect(AppUpdates.version(ofBundleAt: staged) == "1.4.0")
        #expect(!FileManager.default.fileExists(atPath: previous.path))
        // The old build found the outcome waiting when it was started, and nothing before it did.
        #expect(try String(contentsOf: seen, encoding: .utf8) == "swap-1 2\n")
        #expect(AppUpdates.swapOutcome(support: root) == .init(id: "swap-1", status: AppUpdates.SwapOutcome.putBack))
    }

    @Test("recovery stops a new build that is still coming up before the old one is started")
    func swapHelperStopsLateBuild() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let tools = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: tools) }
        // A directory URL, with the trailing slash the running app's own bundle URL carries.
        let current = root.appending(path: "Legion Control.app", directoryHint: .isDirectory)
        let staged = root.appending(path: "Legion Control.new.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        let starting = AppUpdates.launchingURL(support: root)
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: staged, identifier: "id", executable: "LegionControl", version: "1.4.0")
        // A new build that hangs on its way up: it writes its pid as it starts and never confirms.
        // A link to sleep, so ps names the process by its path inside the bundle, as it would the
        // real executable.
        try FileManager.default.createSymbolicLink(at: staged.appending(path: "Contents/MacOS/hang"),
                                                   withDestinationURL: URL(fileURLWithPath: "/bin/sleep"))
        let started = CommandGrammar.posixQuoted(starting.path)
        // Started by its canonical path, as LaunchServices starts an app, whatever the helper wrote.
        let launch = try Self.recordingLauncher(in: tools, then: """
            bundle="${1%/}"
            if [ -e "$bundle/Contents/MacOS/hang" ]; then
                "$bundle/Contents/MacOS/hang" 30 </dev/null >/dev/null 2>&1 &
                printf '%s\\n' "$!" > \(started)
            elif kill -0 "$(cat \(started))" 2>/dev/null; then
                echo new-build-still-running >> "$LOG"
            fi

            """)
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: try Self.exitedProcessID(), launchTimeout: 1,
                                           launcher: launch.argv)
        let run = try await Self.runHelper(script, in: root)
        let hung = try #require(Int32(String(contentsOf: starting, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)))
        defer { kill(hung, SIGKILL) }

        #expect(run.exitCode == 2)
        // The old build was started only once the new one had been stopped.
        #expect(current.path(percentEncoded: false).hasSuffix("/"))
        #expect(Self.launched(launch.log) == [AppUpdates.bundlePath(current), AppUpdates.bundlePath(current)])
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
        #expect(AppUpdates.version(ofBundleAt: staged) == "1.4.0")
        #expect(kill(hung, 0) != 0)
    }

    @Test("recovery leaves alone a recorded pid that is not running out of the new bundle")
    func swapHelperSparesOtherProcesses() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let tools = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: tools) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: staged, identifier: "id", executable: "LegionControl", version: "1.4.0")

        // The pid the new build wrote has since been given to something else entirely.
        let bystander = Process()
        bystander.executableURL = URL(fileURLWithPath: "/bin/sleep")
        bystander.arguments = ["30"]
        try bystander.run()
        defer { if bystander.isRunning { bystander.terminate() } }
        let launch = try Self.recordingLauncher(in: tools, then: """
            printf '%s\\n' \(bystander.processIdentifier) > \(CommandGrammar.posixQuoted(AppUpdates.launchingURL(support: root).path))

            """)
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: try Self.exitedProcessID(), launchTimeout: 1,
                                           launcher: launch.argv)
        let run = try await Self.runHelper(script, in: root)
        #expect(run.exitCode == 2)
        #expect(bystander.isRunning)
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
    }

    @Test("recovery sends the hard kill only while the pid still runs out of the new bundle")
    func swapHelperKillsOnlyInsideBundle() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let tools = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: tools) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        let starting = AppUpdates.launchingURL(support: root)
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: staged, identifier: "id", executable: "LegionControl", version: "1.4.0")
        // A new build that, asked to stop, becomes something running from outside the bundle under
        // the same pid: what a pid handed to another program between the two signals looks like to
        // the helper. A shell linked into the bundle, so ps names it by its path there until it
        // execs sleep.
        try FileManager.default.createSymbolicLink(at: staged.appending(path: "Contents/MacOS/hang"),
                                                   withDestinationURL: URL(fileURLWithPath: "/bin/bash"))
        let started = CommandGrammar.posixQuoted(starting.path)
        let launch = try Self.recordingLauncher(in: tools, then: """
            bundle="${1%/}"
            if [ -e "$bundle/Contents/MacOS/hang" ]; then
                "$bundle/Contents/MacOS/hang" -c 'trap "exec /bin/sleep 30" TERM; while :; do /bin/sleep 0.1; done' </dev/null >/dev/null 2>&1 &
                printf '%s\\n' "$!" > \(started)
            fi

            """)
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: try Self.exitedProcessID(), launchTimeout: 1,
                                           launcher: launch.argv)
        let run = try await Self.runHelper(script, in: root)
        let pid = try #require(Int32(String(contentsOf: starting, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)))
        defer { kill(pid, SIGKILL) }

        #expect(run.exitCode == 2)
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
        // Stopped being the new build after the first signal, so it never got the second.
        #expect(kill(pid, 0) == 0)
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

        let seen = root.appending(path: "seen")
        let launch = try Self.recordingLauncher(in: root, then: Self.notingOutcome(support: root, into: seen))
        let script = AppUpdates.swapScript(current: current, staged: staged, previous: previous,
                                           marker: marker, pid: try Self.exitedProcessID(), swapID: "swap-1",
                                           launchTimeout: 1, launcher: launch.argv)
        let run = try await Self.runHelper(script, in: root)
        #expect(run.exitCode == 1)
        // The one window where the real path could hold nothing is closed by putting the old bundle
        // straight back, and that is the one launch.
        #expect(AppUpdates.version(ofBundleAt: current) == "1.3.0")
        #expect(Self.launched(launch.log) == [current.path(percentEncoded: false)])
        // Written down before the old build was started, so it reads that nothing moved.
        #expect(try String(contentsOf: seen, encoding: .utf8) == "swap-1 1\n")
    }

    @Test("a live old app aborts the helper without changing bundles, backups, marker or launching")
    func swapHelperRefusesLiveApp() async throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")
        let previous = root.appending(path: "Legion Control.previous.app")
        let marker = root.appending(path: "launched-ok")
        for (bundle, version) in [(current, "1.3.0"), (staged, "1.4.0"), (previous, "1.2.0")] {
            try Self.makeBundle(at: bundle, identifier: "id", executable: "LegionControl", version: version)
        }
        try Data("existing launch proof".utf8).write(to: marker)
        // The launcher lives outside the tree being compared, so writing it changes nothing there.
        let tools = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: tools) }
        let launch = try Self.recordingLauncher(in: tools)
        let snapshot = try Self.fileSnapshot(root)

        let oldApp = Process()
        oldApp.executableURL = URL(fileURLWithPath: "/bin/sleep")
        oldApp.arguments = ["30"]
        let exited = AsyncStream<Void> { continuation in
            oldApp.terminationHandler = { _ in continuation.yield(()); continuation.finish() }
        }
        try oldApp.run()
        defer { if oldApp.isRunning { oldApp.terminate() } }
        let script = AppUpdates.swapScript(
            current: current, staged: staged, previous: previous, marker: marker,
            pid: oldApp.processIdentifier, swapID: "swap-1", exitTimeout: 1, launchTimeout: 1, launcher: launch.argv
        )
        let run = await Shell.run(executable: "/bin/sh", arguments: ["-c", script], timeout: 10)
        #expect(run.exitCode == 3)
        #expect(oldApp.isRunning)
        #expect(Self.launched(launch.log).isEmpty)
        // The one thing written is the note of how it ended.
        #expect(AppUpdates.swapOutcome(support: root) == .init(id: "swap-1", status: AppUpdates.SwapOutcome.notQuit))
        let outcome = "/" + AppUpdates.swapOutcomeURL(support: root).lastPathComponent
        #expect(try Self.fileSnapshot(root).filter { !$0.key.hasSuffix(outcome) } == snapshot)
        oldApp.terminate()
        for await _ in exited { break }
    }

    @Test("the helper is never started for a bundle this process is not running from")
    func startSwapRefusesAnotherBundle() throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let staged = root.appending(path: "Legion Control.new.app")
        let marker = root.appending(path: "launched-ok")
        try Self.makeBundle(at: current, identifier: "id", executable: "LegionControl", version: "1.3.0")
        try Self.makeBundle(at: staged, identifier: "id", executable: "LegionControl", version: "1.4.0")
        let tools = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: tools) }
        let launch = try Self.recordingLauncher(in: tools)
        let snapshot = try Self.fileSnapshot(root)

        // This process is the test runner, not anything in that directory, so the helper that would
        // wait for it to exit and then move those bundles is refused before it is even written.
        #expect(throws: AppUpdates.InstallFailure.self) {
            try AppUpdates.startSwap(
                staged: .init(bundle: staged, previous: root.appending(path: "Legion Control.previous.app"), version: "1.4.0"),
                current: current, marker: marker, launcher: launch.argv
            )
        }
        #expect(try Self.fileSnapshot(root) == snapshot)
        #expect(Self.launched(launch.log).isEmpty)
    }

    @Test("rollback hands reversed bundles to the helper without deleting existing staging or launch proof")
    func rollbackPreservesFilesUntilHandoff() throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let current = root.appending(path: "Legion Control.app")
        let retained = root.appending(path: "Legion Control.previous.app")
        let new = root.appending(path: "Legion Control.new.app")
        let marker = root.appending(path: "launched-ok")
        for (bundle, version) in [(current, "1.4.0"), (retained, "1.3.0"), (new, "1.5.0")] {
            try Self.makeBundle(at: bundle, identifier: "id", executable: "LegionControl", version: version)
        }
        try Data("existing launch proof".utf8).write(to: marker)
        let snapshot = try Self.fileSnapshot(root)
        var starts = 0
        try AppUpdates.rollBack(to: retained, current: current, marker: marker) { staged, actualCurrent, actualMarker in
            starts += 1
            #expect(staged.bundle == retained)
            #expect(staged.previous == new)
            #expect(staged.version == "1.3.0")
            #expect(actualCurrent == current)
            #expect(actualMarker == marker)
            let observed = try Self.fileSnapshot(root)
            #expect(observed == snapshot)
        }
        #expect(starts == 1)
        #expect(try Self.fileSnapshot(root) == snapshot)
    }

    private static func exitedProcessID() throws -> Int32 {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/true")
        try process.run()
        process.waitUntilExit()
        return process.processIdentifier
    }

    private static func fileSnapshot(_ root: URL) throws -> [String: Data] {
        let entries = try #require(FileManager.default.enumerator(at: root, includingPropertiesForKeys: [.isRegularFileKey]))
        var result: [String: Data] = [:]
        for case let file as URL in entries {
            if try file.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true {
                result[file.path.replacingOccurrences(of: root.path, with: "")] = try Data(contentsOf: file)
            }
        }
        return result
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

    @Test("only an outcome naming a swap is read as one")
    func swapOutcomeReading() throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let url = AppUpdates.swapOutcomeURL(support: root)
        #expect(AppUpdates.swapOutcome(support: root) == nil)
        for unreadable in ["", "\n", " 0\n", "swap-1\n", "swap-1 two\n", "swap-1 0 extra\n"] {
            try Data(unreadable.utf8).write(to: url)
            #expect(AppUpdates.swapOutcome(support: root) == nil, "\(unreadable)")
        }
        try Data("swap-1 2\n".utf8).write(to: url)
        #expect(AppUpdates.swapOutcome(support: root) == .init(id: "swap-1", status: 2))
        AppUpdates.forgetSwapOutcome(support: root)
        #expect(!FileManager.default.fileExists(atPath: url.path))
    }

    @Test("the launch records are written where the helper looks for them, carrying this build's pid")
    func launchMarker() throws {
        let root = try Self.temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let pid = "\(ProcessInfo.processInfo.processIdentifier)\n"
        AppUpdates.recordLaunching(support: root)
        #expect(try String(contentsOf: AppUpdates.launchingURL(support: root), encoding: .utf8) == pid)
        // Starting is not coming up: an older helper reads the marker's existence as success.
        #expect(!FileManager.default.fileExists(atPath: AppUpdates.launchMarkerURL(support: root).path))
        AppUpdates.confirmLaunch(support: root)
        #expect(try String(contentsOf: AppUpdates.launchMarkerURL(support: root), encoding: .utf8) == pid)
    }
}
