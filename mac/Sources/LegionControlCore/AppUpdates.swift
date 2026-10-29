import CryptoKit
import Foundation

/// Keeping Legion Control itself up to date, from the GitHub releases of whichever repository the
/// configuration names.
///
/// The install is where this differs from the phone. Android hands an apk to the system installer
/// and the system decides whether to trust it; a Mac app has to replace itself, while it is running,
/// and then start the replacement. The previous version of this file unpacked the download, checked
/// that a file existed at the expected path and that a version string matched, stripped the
/// quarantine flag and then rsynced the result over the bundle it was running out of. Every one of
/// those steps is now different:
///
/// - Nothing is trusted without a signature. A release carries a manifest signed by the release key
///   and the manifest names the exact artifact with its sha256 and size. HTTPS and control of a
///   GitHub account are not a substitute, and a release without a signed manifest installs nothing.
/// - The exact platform asset is selected by name. "The first zip attached" is how the wrong build
///   gets installed.
/// - The replacement is staged as a complete sibling bundle and checked in full — bundle id,
///   executable, version, architecture — before anything is moved.
/// - The swap is two renames with the old bundle kept beside the new one, not an rsync over a live
///   directory. An interrupted rsync leaves a mixture of two builds that cannot start and cannot fix
///   itself; an interrupted rename leaves one of the two whole bundles.
/// - A helper process does the swap and the relaunch, watches whether the new build actually comes
///   up, and puts the old one back when it does not.
enum AppUpdates {

    /// The exact asset name for this platform.
    static let assetName = ReleaseTrust.Artifacts.macApp

    /// The newest release, once it is known to be newer than what is running.
    struct Release: Sendable, Equatable {
        var version: String
        /// The first paragraph of the release body, or empty.
        var notes: String
        var assetURL: URL
        var sizeBytes: Int64
        /// Where the signed manifest and its signature are. Both are required.
        var manifestURL: URL?
        var signatureURL: URL?

        /// Whether this release can be installed at all. A release with no signed manifest is not a
        /// release this app will install, and the button says so rather than failing at the end.
        var isVerifiable: Bool { manifestURL != nil && signatureURL != nil }
    }

    enum Check: Sendable, Equatable {
        /// Nothing to install. The note carries the quiet reason when there is one worth saying.
        case upToDate(note: String?)
        case available(Release)
        case failed(reason: String)
    }

    // MARK: - Asking

    /// Ask GitHub what the newest release is, and whether it is newer than what is running.
    ///
    /// No credentials, and no way to pass any: this app only ever reads a public repository's
    /// releases.
    static func check(repo: String, installedVersion: String, session: URLSession = .shared) async -> Check {
        let repository = source(for: repo).repository
        guard let url = URL(string: "https://api.github.com/repos/\(repository)/releases/latest") else {
            return .failed(reason: "\"\(repository)\" is not a repository name this can ask about.")
        }

        var request = URLRequest(url: url, timeoutInterval: 10)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("legion-control", forHTTPHeaderField: "User-Agent")

        do {
            let body = Body()
            let code = try await receive(request, limit: Limits.releaseJSON, session: session, into: body)
            // 404 is what a public repository that has never cut a release answers, and it is also
            // what a private one answers to anyone who cannot see it. Neither is a failure worth
            // colouring: there is nothing to install, and saying so quietly is the honest reading.
            if code == 404 {
                return .upToDate(note: "\(repository) has no release yet, so there is nothing newer to install.")
            }
            guard code == 200 else { return .failed(reason: "GitHub answered \(code).") }
            return read(releaseJSON: body.data, installedVersion: installedVersion)
        } catch is TooLarge {
            return .failed(reason: "GitHub's answer was larger than the \(describe(bytes: Limits.releaseJSON)) a release reply can be, so it was not read.")
        } catch {
            return .failed(reason: error.localizedDescription)
        }
    }

    /// The repository a check asks, held to the grammar GitHub spells owner/name in before it goes
    /// anywhere near a URL. The setup names it, and the setup comes from every peer, so a value
    /// that is not one is never pasted into a request: this project's own releases are asked
    /// instead, and the warning says why.
    static func source(for configured: String) -> (repository: String, warning: String?) {
        let builtIn = ControllerConfig.AppUpdatesConfig.defaultRepo
        if configured.trimmingCharacters(in: .whitespaces).isEmpty { return (builtIn, nil) }
        guard SetupGuard.isRepository(configured) else {
            return (builtIn, "The update repository \(SetupGuard.describe(configured)) is not a GitHub owner/name, so updates are read from \(builtIn) instead.")
        }
        return (configured, nil)
    }

    /// Everything about a reply that does not need the network: the tag, the comparison and the
    /// three assets. Kept apart from the request above so it can be exercised on its own.
    static func read(releaseJSON data: Data, installedVersion: String) -> Check {
        guard let root = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
            return .failed(reason: "GitHub's answer could not be read.")
        }

        let latest = version(fromTag: root["tag_name"] as? String ?? "")
        guard !latest.isEmpty else {
            return .failed(reason: "The newest release has no tag to take a version from.")
        }
        guard isNewer(latest, than: installedVersion) else { return .upToDate(note: nil) }

        let assets = root["assets"] as? [[String: Any]] ?? []
        func asset(_ name: String) -> [String: Any]? {
            assets.first { ($0["name"] as? String) == name }
        }
        // The asset API url rather than browser_download_url: the same one line of code then works
        // for a public repository and, with a header added, for a private fork.
        func url(_ asset: [String: Any]?) -> URL? {
            (asset?["url"] as? String).flatMap(URL.init(string:))
        }

        guard let app = asset(assetName), let assetURL = url(app) else {
            return .failed(reason: "Release \(latest) has no \(assetName) attached, so there is nothing for this Mac to install.")
        }

        return .available(Release(
            version: latest,
            notes: firstParagraph(of: root["body"] as? String ?? ""),
            assetURL: assetURL,
            sizeBytes: (app["size"] as? NSNumber)?.int64Value ?? 0,
            manifestURL: url(asset(ReleaseTrust.Artifacts.manifest)),
            signatureURL: url(asset(ReleaseTrust.Artifacts.manifestSignature))
        ))
    }

    /// The tag with its leading `v` taken off, which is the whole of the release naming convention.
    static func version(fromTag tag: String) -> String {
        let trimmed = tag.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("v") else { return trimmed }
        return String(trimmed.dropFirst())
    }

    /// Numeric field by field, so 1.0.10 is correctly newer than 1.0.9.
    static func isNewer(_ candidate: String, than installed: String) -> Bool {
        // Everything before the first "-", which for "" and "-" is nothing at all, rather than
        // element zero of an empty split.
        func fields(_ value: String) -> [Int] {
            (value.split(separator: "-", maxSplits: 1).first ?? "")
                .split(separator: ".")
                .map { Int($0) ?? 0 }
        }
        let a = fields(candidate)
        let b = fields(installed)
        for index in 0..<max(a.count, b.count) {
            let left = index < a.count ? a[index] : 0
            let right = index < b.count ? b[index] : 0
            if left != right { return left > right }
        }
        return false
    }

    /// The first paragraph of a release body, as one line.
    static func firstParagraph(of body: String) -> String {
        let normalised = body.replacingOccurrences(of: "\r\n", with: "\n")
        let paragraph = normalised
            .components(separatedBy: "\n\n")
            .first { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty } ?? ""
        return paragraph
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .joined(separator: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    // MARK: - Installing

    struct InstallFailure: Error, Sendable, Equatable {
        var message: String
        init(_ message: String) { self.message = message }
    }

    /// A downloaded release and the directory holding it.
    struct Download: Sendable {
        var directory: URL
        var archive: URL
        var manifest: ReleaseTrust.Manifest
        var artifact: ReleaseTrust.Manifest.Artifact
    }

    /// Fetch the manifest, check its signature, then fetch the asset the manifest names and check it
    /// against what the manifest says. In that order: the signature is what decides whether the
    /// hashes mean anything, so it is established before anything large is downloaded.
    ///
    /// Nothing is read past what it can be. The manifest and its signature have fixed ceilings, and
    /// the archive the size the signed manifest gives it: a download that runs past that is stopped
    /// and deleted there, rather than written out in full and refused afterwards. `key` is the same
    /// seam as `ReleaseTrust.verifiedManifest`'s, and `parent` where the download directory goes,
    /// both for the tests.
    static func download(_ release: Release, session: URLSession = .shared,
                         key: Curve25519.Signing.PublicKey? = nil,
                         into parent: URL = URL(fileURLWithPath: NSTemporaryDirectory())) async throws -> Download {
        guard let manifestURL = release.manifestURL, let signatureURL = release.signatureURL else {
            throw InstallFailure("""
                Release \(release.version) does not carry a signed manifest, so this build cannot \
                establish that it really is a Legion Control release. Nothing was installed. Download \
                it yourself if you know it is genuine.
                """)
        }

        let manifestBytes = try await fetch(manifestURL, what: "The release manifest",
                                            limit: Limits.manifest, session: session)
        let signatureBytes = try await fetch(signatureURL, what: "The manifest signature",
                                             limit: Limits.signature, session: session)

        let manifest: ReleaseTrust.Manifest
        do {
            manifest = try ReleaseTrust.verifiedManifest(bytes: manifestBytes, signature: signatureBytes, key: key)
        } catch let failure as ReleaseTrust.TrustFailure {
            throw InstallFailure(failure.message)
        }

        guard manifest.version == release.version else {
            throw InstallFailure("The signed manifest is for version \(manifest.version) and the release says \(release.version), so nothing was installed.")
        }
        guard let artifact = manifest.artifact(named: assetName) else {
            throw InstallFailure("The signed manifest does not name \(assetName), so there is nothing for this Mac to install.")
        }
        // The size is the ceiling the download is read against, so a manifest without one gives the
        // download nothing to stop at.
        guard artifact.size > 0 else {
            throw InstallFailure("The signed manifest gives \(assetName) no size, so there is nothing to hold the download to and nothing was installed.")
        }

        let directory = parent.appending(path: "legion-control-update-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        } catch {
            throw InstallFailure("A temporary directory for the download could not be made. \(error.localizedDescription)")
        }

        let archive = directory.appending(path: assetName)
        do {
            var request = URLRequest(url: release.assetURL, timeoutInterval: 120)
            request.setValue("application/octet-stream", forHTTPHeaderField: "Accept")
            request.setValue("legion-control", forHTTPHeaderField: "User-Agent")
            // Straight to a file as it arrives: the archive is tens of megabytes and there is no
            // reason for any of it to sit in memory on its way there.
            guard FileManager.default.createFile(atPath: archive.path(percentEncoded: false), contents: nil) else {
                throw InstallFailure("The download could not be written to \(archive.lastPathComponent).")
            }
            let file = try FileHandle(forWritingTo: archive)
            defer { try? file.close() }
            let code = try await receive(request, limit: artifact.size, session: session, into: Body(file: file))
            guard code == 200 else {
                throw InstallFailure("The download failed. GitHub answered \(code).")
            }
        } catch let failure as InstallFailure {
            try? FileManager.default.removeItem(at: directory)
            throw failure
        } catch is TooLarge {
            try? FileManager.default.removeItem(at: directory)
            throw InstallFailure("The download ran past the \(artifact.size) bytes the signed manifest gives \(assetName), so it was stopped and deleted. Nothing was installed.")
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw InstallFailure("The download failed. \(error.localizedDescription)")
        }

        do {
            try ReleaseTrust.verify(artifact: artifact, fileAt: archive)
        } catch let failure as ReleaseTrust.TrustFailure {
            try? FileManager.default.removeItem(at: directory)
            throw InstallFailure(failure.message)
        }

        return Download(directory: directory, archive: archive, manifest: manifest, artifact: artifact)
    }

    private static func fetch(_ url: URL, what: String, limit: Int64, session: URLSession) async throws -> Data {
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.setValue("application/octet-stream", forHTTPHeaderField: "Accept")
        request.setValue("legion-control", forHTTPHeaderField: "User-Agent")
        do {
            let body = Body()
            let code = try await receive(request, limit: limit, session: session, into: body)
            guard code == 200 else {
                throw InstallFailure("\(what) could not be downloaded. GitHub answered \(code).")
            }
            return body.data
        } catch let failure as InstallFailure {
            throw failure
        } catch is TooLarge {
            throw InstallFailure("\(what) is larger than the \(describe(bytes: limit)) it can be, so it was not read and nothing was installed.")
        } catch {
            throw InstallFailure("\(what) could not be downloaded. \(error.localizedDescription)")
        }
    }

    static func discard(_ download: Download) {
        try? FileManager.default.removeItem(at: download.directory)
    }

    // MARK: - Reading with a ceiling

    /// The most this build reads of each small thing it downloads. A release reply, a manifest and
    /// a signature are all a few kilobytes; one that is not is not what it claims to be, and is
    /// refused rather than held in memory on the way to being refused. The archive's ceiling is the
    /// size its signed manifest gives it.
    enum Limits {
        static let releaseJSON: Int64 = 1 << 20
        static let manifest: Int64 = 256 << 10
        static let signature: Int64 = 4 << 10
    }

    /// A body that went past its ceiling.
    struct TooLarge: Error {}

    static func describe(bytes: Int64) -> String {
        if bytes >= 1 << 20, bytes % (1 << 20) == 0 { return "\(bytes >> 20) MiB" }
        if bytes >= 1 << 10, bytes % (1 << 10) == 0 { return "\(bytes >> 10) KiB" }
        return "\(bytes) bytes"
    }

    /// Where a body goes as it arrives: memory for the small things, a file for the archive. Only
    /// ever written from the one delegate queue of the session reading it, and only read once that
    /// session has finished.
    final class Body: @unchecked Sendable {
        private(set) var data = Data()
        private let file: FileHandle?

        init(file: FileHandle? = nil) { self.file = file }

        func append(_ chunk: Data) throws {
            if let file { try file.write(contentsOf: chunk) } else { data.append(chunk) }
        }
    }

    /// Send `request` and hand the body to `body` as it arrives, never more than `limit` bytes of it.
    ///
    /// A reply that says up front it is larger is refused before any of it is read, and one that
    /// turns out larger is stopped the moment it passes, with `TooLarge` either way. Anything but a
    /// 200 is returned without its body being read at all: the caller only wants the status.
    ///
    /// A delegate rather than `bytes(for:)`, whose byte at a time iteration is far too slow for an
    /// archive of this size. Its own session, built from the configuration of the one passed in, so
    /// a test's stub protocol still answers.
    static func receive(_ request: URLRequest, limit: Int64, session: URLSession, into body: Body) async throws -> Int {
        let receiver = CappedReceiver(limit: limit, body: body)
        let reading = URLSession(configuration: session.configuration, delegate: receiver, delegateQueue: nil)
        defer { reading.finishTasksAndInvalidate() }
        let task = reading.dataTask(with: request)
        return try await withTaskCancellationHandler {
            try await receiver.run(task)
        } onCancel: {
            task.cancel()
        }
    }

    private final class CappedReceiver: NSObject, URLSessionDataDelegate, @unchecked Sendable {
        private let limit: Int64
        private let body: Body
        private let lock = NSLock()
        private var received: Int64 = 0
        private var status = 0
        private var problem: (any Error)?
        private var continuation: CheckedContinuation<Int, any Error>?

        init(limit: Int64, body: Body) {
            self.limit = limit
            self.body = body
        }

        func run(_ task: URLSessionDataTask) async throws -> Int {
            try await withCheckedThrowingContinuation { continuation in
                lock.withLock { self.continuation = continuation }
                task.resume()
            }
        }

        func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
                        completionHandler: @escaping @Sendable (URLSession.ResponseDisposition) -> Void) {
            let code = (response as? HTTPURLResponse)?.statusCode ?? 0
            let tooLarge = code == 200 && response.expectedContentLength > limit
            lock.withLock {
                status = code
                if tooLarge { problem = TooLarge() }
            }
            completionHandler(code == 200 && !tooLarge ? .allow : .cancel)
        }

        func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
            let refused: Bool = lock.withLock {
                guard problem == nil else { return true }
                received += Int64(data.count)
                if received > limit { problem = TooLarge() }
                return problem != nil
            }
            if refused { dataTask.cancel(); return }
            do {
                try body.append(data)
            } catch {
                lock.withLock { problem = error }
                dataTask.cancel()
            }
        }

        func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: (any Error)?) {
            let (continuation, problem, status) = lock.withLock {
                let taken = self.continuation
                self.continuation = nil
                return (taken, self.problem, self.status)
            }
            if let problem {
                continuation?.resume(throwing: problem)
            } else if status != 0, status != 200 {
                // Cancelled here on purpose, once the status was known.
                continuation?.resume(returning: status)
            } else if let error {
                continuation?.resume(throwing: error)
            } else {
                continuation?.resume(returning: status)
            }
        }
    }

    // MARK: - Staging and swapping

    /// What a staged replacement is, once it has been unpacked and checked.
    struct Staged: Sendable {
        /// The complete replacement bundle, sitting beside the one that is running.
        var bundle: URL
        /// Where the current bundle will be moved to.
        var previous: URL
        var version: String
    }

    /// Unpack the verified archive next to the bundle it will replace, and check it thoroughly.
    ///
    /// Beside it, not in a temporary directory: the swap has to be two renames within one directory
    /// to be atomic, and a rename across volumes is a copy that can be interrupted halfway.
    static func stage(_ download: Download, version: String, replacing bundleURL: URL) async throws -> Staged {
        guard bundleURL.pathExtension == "app" else {
            throw InstallFailure("Legion Control is not running out of an app bundle, so there is nothing to replace. Build and install it with mac/build.sh instead.")
        }
        if let sibling = siblingReason(for: bundleURL) { throw InstallFailure(sibling) }
        let parent = bundleURL.deletingLastPathComponent()
        guard FileManager.default.isWritableFile(atPath: parent.path(percentEncoded: false)) else {
            throw InstallFailure("\(parent.path(percentEncoded: false)) is not writable, so the update cannot be staged. Move Legion Control to a folder you can write to.")
        }

        let staging = parent.appending(path: ".Legion Control.staging-\(UUID().uuidString)")
        try? FileManager.default.removeItem(at: staging)
        let extraction = await Shell.run(
            executable: "/usr/bin/ditto",
            arguments: ["-x", "-k", download.archive.path(percentEncoded: false), staging.path(percentEncoded: false)],
            timeout: 300
        )
        guard extraction.succeeded else {
            try? FileManager.default.removeItem(at: staging)
            throw InstallFailure("The download could not be unpacked. \(extraction.failureText)")
        }

        do {
            let unpacked = try bundle(in: staging)
            try await verify(unpacked, is: version, against: bundleURL)

            // The quarantine flag is only ever removed from an artifact whose signature has already
            // been checked against the pinned release key. Stripping it from anything else would be
            // handing Gatekeeper's job to whoever served the download.
            _ = await Shell.run(
                executable: "/usr/bin/xattr",
                arguments: ["-dr", "com.apple.quarantine", unpacked.path(percentEncoded: false)],
                timeout: 120
            )

            let staged = parent.appending(path: stagedName)
            try? FileManager.default.removeItem(at: staged)
            try FileManager.default.moveItem(at: unpacked, to: staged)
            try? FileManager.default.removeItem(at: staging)

            return Staged(
                bundle: staged,
                previous: parent.appending(path: previousName),
                version: version
            )
        } catch {
            try? FileManager.default.removeItem(at: staging)
            throw error
        }
    }

    /// The one app bundle the archive held.
    private static func bundle(in directory: URL) throws -> URL {
        let contents = (try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: nil,
            options: [.skipsHiddenFiles]
        )) ?? []
        let bundles = contents.filter { $0.pathExtension == "app" }
        guard bundles.count == 1, let only = bundles.first else {
            throw InstallFailure("The archive did not hold exactly one app bundle, so there is nothing safe to install from it.")
        }
        return only
    }

    /// That the thing about to replace this app really is this app: the same bundle identifier, an
    /// executable where the running one has its executable, the version the release claimed, and a
    /// binary that runs on this machine.
    static func verify(_ candidate: URL, is version: String, against current: URL) async throws {
        let currentPlist = current.appending(path: "Contents/Info.plist")
        let candidatePlist = candidate.appending(path: "Contents/Info.plist")

        guard let candidateInfo = try? plist(at: candidatePlist) else {
            throw InstallFailure("The downloaded bundle has no readable Contents/Info.plist, so it is not this app.")
        }
        let currentInfo = (try? plist(at: currentPlist)) ?? [:]

        let expectedIdentifier = (currentInfo["CFBundleIdentifier"] as? String)
            ?? Bundle.main.bundleIdentifier
        if let expectedIdentifier {
            let found = candidateInfo["CFBundleIdentifier"] as? String
            guard found == expectedIdentifier else {
                throw InstallFailure("The downloaded bundle identifies itself as \(found ?? "nothing") and this app is \(expectedIdentifier), so it was not installed.")
            }
        }

        let executableName = (currentInfo["CFBundleExecutable"] as? String)
            ?? (candidateInfo["CFBundleExecutable"] as? String)
            ?? "LegionControl"
        let executable = candidate.appending(path: "Contents/MacOS/\(executableName)")
        guard FileManager.default.isExecutableFile(atPath: executable.path(percentEncoded: false)) else {
            throw InstallFailure("The downloaded bundle has no executable at Contents/MacOS/\(executableName), so it is not this app.")
        }

        let found = candidateInfo["CFBundleShortVersionString"] as? String ?? ""
        guard found == version else {
            throw InstallFailure("The release says \(version) but the bundle in it says \(found.isEmpty ? "nothing" : found), so it was not installed.")
        }

        // A binary for the wrong architecture installs cleanly and then will not launch, which is
        // the one failure a version check cannot catch.
        let architectures = await architectures(of: executable)
        guard architectures.contains(where: runsHere) else {
            throw InstallFailure("The downloaded build is for \(architectures.isEmpty ? "an unknown architecture" : architectures.joined(separator: ", ")) and this Mac runs \(currentArchitecture), so it was not installed.")
        }
    }

    /// Whether a slice named by `lipo` will actually run on this machine.
    ///
    /// `arm64e` is the pointer-authentication variant of `arm64` and runs on the same hardware; the
    /// system's own binaries are built that way. Comparing the strings outright would refuse a
    /// perfectly good build.
    static func runsHere(_ architecture: String) -> Bool {
        switch currentArchitecture {
        case "arm64": architecture == "arm64" || architecture == "arm64e"
        case "x86_64": architecture == "x86_64"
        default: architecture == currentArchitecture
        }
    }

    static func plist(at url: URL) throws -> [String: Any] {
        let data = try Data(contentsOf: url)
        guard let object = try PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any] else {
            throw InstallFailure("\(url.lastPathComponent) is not a property list.")
        }
        return object
    }

    /// What `lipo` says the binary holds.
    static func architectures(of executable: URL) async -> [String] {
        let run = await Shell.run(
            executable: "/usr/bin/lipo",
            arguments: ["-archs", executable.path(percentEncoded: false)],
            timeout: 30
        )
        guard run.succeeded else { return [] }
        return run.standardOutput
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .split(separator: " ")
            .map(String.init)
    }

    static var currentArchitecture: String {
        #if arch(arm64)
        return "arm64"
        #elseif arch(x86_64)
        return "x86_64"
        #else
        return "unknown"
        #endif
    }

    // MARK: - The swap

    /// What the helper starts a bundle with: LaunchServices, as a new instance, so it never brings an
    /// already running copy forward in place of the one it has just put there.
    ///
    /// A parameter of the helper rather than a word written into it, so a test can hand it a command
    /// that only writes down what it would have started. The real one, pointed at the fake bundles a
    /// test makes, has macOS report each of them as an app that quit unexpectedly and offer to move it
    /// to the Bin, in front of whoever is running the tests.
    static let launcher = ["/usr/bin/open", "-n"]

    /// The helper script that does the dangerous part from outside this process.
    ///
    /// It cannot be done from inside. The swap moves the bundle this executable is running out of,
    /// and the recovery has to happen after this process is gone — which is exactly when the new
    /// build failing to launch would leave a user with no app and nothing to run to fix it.
    ///
    /// The sequence: wait for this process to exit, move the current bundle aside, move the staged
    /// one into place, launch it, and wait for it to say it came up. If it does not, stop it if it is
    /// still on its way up, put the old bundle back and launch that instead. Every path ends with
    /// exactly one bundle at the real path, and it is always one of the two whole bundles rather
    /// than a mixture, and with one copy of the app running rather than two.
    ///
    /// Every path also writes down how it ended, as `swapID` and the exit status, for the build
    /// that comes up next (`swapOutcome`).
    ///
    /// `launcher` is the argv every path starts a bundle with, the bundle's path appended.
    static func swapScript(
        current: URL,
        staged: URL,
        previous: URL,
        marker: URL,
        pid: Int32,
        swapID: String = "",
        exitTimeout: Int = 10,
        launchTimeout: Int = 45,
        launcher: [String] = launcher
    ) -> String {
        func quoted(_ url: URL) -> String { CommandGrammar.posixQuoted(bundlePath(url)) }
        let support = marker.deletingLastPathComponent()
        return """
        #!/bin/sh
        set -u
        CURRENT=\(quoted(current))
        STAGED=\(quoted(staged))
        PREVIOUS=\(quoted(previous))
        MARKER=\(quoted(marker))
        STARTING=\(quoted(launchingURL(support: support)))
        OUTCOME=\(quoted(swapOutcomeURL(support: support)))
        SWAP_ID=\(CommandGrammar.posixQuoted(swapID))
        PID=\(pid)

        # Every path below starts a bundle through this one function.
        launch() {
            \(launcher.map(CommandGrammar.posixQuoted).joined(separator: " ")) "$1" 2>/dev/null || true
        }

        # How this swap ended, for the build that comes up next to read. Written before any build is
        # started on the way out, so the one that comes up never looks before it is there.
        finish() {
            printf '%s %s\\n' "$SWAP_ID" "$1" > "$OUTCOME.tmp" 2>/dev/null && mv -f "$OUTCOME.tmp" "$OUTCOME" 2>/dev/null
            return 0
        }

        # Whether a pid names a process running out of the bundle this helper put in place. Asked
        # again before every signal: a pid that has since gone to something else, or a marker an
        # older build wrote without one, is left alone.
        runs_from_current() {
            case "$(/bin/ps -p "$1" -o comm= 2>/dev/null)" in
                "$CURRENT"/*) return 0 ;;
            esac
            return 1
        }

        # The new build writes its pid to STARTING as it starts and to the marker once it is up. A
        # build that is still coming up when the wait runs out, slowly or never, is stopped here
        # before the old one is started, so the two never end up running side by side.
        stop_new_build() {
            for record in "$STARTING" "$MARKER"; do
                [ -f "$record" ] || continue
                found=""
                read -r found < "$record" || true
                case "$found" in
                    ''|*[!0-9]*) continue ;;
                esac
                runs_from_current "$found" || continue
                kill -TERM "$found" 2>/dev/null
                j=0
                while [ $j -lt 50 ] && runs_from_current "$found"; do
                    sleep 0.1
                    j=$((j + 1))
                done
                if runs_from_current "$found"; then
                    kill -KILL "$found" 2>/dev/null
                fi
            done
            return 0
        }

        # Wait for the app to actually be gone. Moving a bundle out from under a running process is
        # survivable on macOS but leaves the old code running against the new resources.
        app_is_running() {
            kill -0 "$PID" 2>/dev/null || /bin/ps -p "$PID" -o pid= >/dev/null 2>&1
        }
        i=0
        while [ $i -lt \(exitTimeout * 10) ] && app_is_running; do
            sleep 0.1
            i=$((i + 1))
        done
        if app_is_running; then
            echo "The current app did not exit. No update files were changed." >&2
            finish 3
            exit 3
        fi

        rm -f "$MARKER" "$STARTING"
        rm -rf "$PREVIOUS"

        if ! mv "$CURRENT" "$PREVIOUS"; then
            # Nothing has moved, so the running build is still exactly where it was.
            finish 1
            launch "$CURRENT"
            exit 1
        fi

        if ! mv "$STAGED" "$CURRENT"; then
            # The only window in which the real path holds nothing. Put it back and stop.
            mv "$PREVIOUS" "$CURRENT"
            finish 1
            launch "$CURRENT"
            exit 1
        fi

        launch "$CURRENT"

        # The new build writes the marker as soon as it has finished launching. No marker means it
        # crashed on the way up, or refused to start at all, and the old one goes back.
        i=0
        while [ $i -lt \(launchTimeout * 10) ]; do
            if [ -f "$MARKER" ]; then
                finish 0
                exit 0
            fi
            sleep 0.1
            i=$((i + 1))
        done

        # Recovery. The new build is put where the previous one was rather than deleted, so a failure
        # that turns out to be about something else is still diagnosable.
        stop_new_build
        rm -rf "$STAGED"
        finish 2
        if mv "$CURRENT" "$STAGED" && mv "$PREVIOUS" "$CURRENT"; then
            launch "$CURRENT"
        fi
        exit 2
        """
    }

    /// How the helper ended a swap: its exit status, written down beside the launch marker with the
    /// id of the swap it was given.
    ///
    /// The build that comes up next cannot tell this from the version it is. A roll back into a build
    /// released before swaps were written down lands, and that build never says so; when this one
    /// runs again later, the record left behind looks exactly like a roll back that never came up.
    /// The helper is always written by the build handing over, so it is always one that writes this.
    struct SwapOutcome: Equatable, Sendable {
        var id: String
        var status: Int32

        /// The build the helper put in place came up.
        static let landed: Int32 = 0
        /// A bundle could not be moved, and the one that was running is where it was.
        static let notMoved: Int32 = 1
        /// The build put in place did not come up, and the previous one was put back.
        static let putBack: Int32 = 2
        /// The build that handed over never quit, so nothing was moved.
        static let notQuit: Int32 = 3
    }

    static func swapOutcomeURL(support: URL) -> URL {
        support.appending(path: "swap-outcome")
    }

    /// What the last helper wrote down, if it wrote anything readable.
    static func swapOutcome(support: URL) -> SwapOutcome? {
        guard let text = try? String(contentsOf: swapOutcomeURL(support: support), encoding: .utf8) else { return nil }
        let fields = text.split(whereSeparator: \.isWhitespace)
        guard fields.count == 2, let status = Int32(fields[1]) else { return nil }
        return SwapOutcome(id: String(fields[0]), status: status)
    }

    static func forgetSwapOutcome(support: URL) {
        try? FileManager.default.removeItem(at: swapOutcomeURL(support: support))
    }

    /// How long a helper can still be at work after it was handed a swap: ten seconds for this build
    /// to quit, forty-five for the next to come up, and up to ten more stopping it, with room to
    /// spare. A swap with no outcome after this never got one.
    static let helperLifetime: TimeInterval = 2 * 60

    /// A bundle's path as the helper and `ps` spell it: without the trailing slash a directory URL
    /// carries. The running app's own bundle URL has one, and `"$CURRENT"/*` written with it would
    /// never match the executable path inside it.
    static func bundlePath(_ url: URL) -> String {
        let path = url.standardizedFileURL.path(percentEncoded: false)
        guard path.count > 1, path.hasSuffix("/") else { return path }
        return String(path.dropLast())
    }

    /// Where the new build says "I came up".
    static func launchMarkerURL(support: URL) -> URL {
        support.appending(path: "launched-ok")
    }

    /// Where a build says "I am starting", before it has come up. The marker cannot say this: a
    /// helper from an older release reads the marker's mere existence as a launch that worked.
    static func launchingURL(support: URL) -> URL {
        support.appending(path: "launching")
    }

    /// Write the swap helper and start it detached, then it is out of this process's hands.
    ///
    /// Only ever for the bundle this process runs from: the helper waits for this process to exit
    /// and then moves `current`, so any other bundle would be swapped under a process that is not
    /// running out of it. The same check is what stops a test that forgot to replace this from
    /// starting a helper over its fake bundles.
    ///
    /// `swapID` is what the helper writes down with its outcome, so the next build can tell this
    /// swap from any other. `onExit` hears the helper's exit status, which this process only ever
    /// lives to hear when the helper gave up: it moves nothing until this process has gone.
    static func startSwap(
        staged: Staged, current: URL, marker: URL,
        swapID: String = "",
        launcher: [String] = launcher,
        runningFrom running: URL = Bundle.main.bundleURL,
        onExit: (@Sendable (Int32) -> Void)? = nil
    ) throws {
        guard bundlePath(current) == bundlePath(running) else {
            throw InstallFailure("\(current.lastPathComponent) is not the bundle Legion Control is running from, so the update helper was not started.")
        }
        let script = swapScript(
            current: current,
            staged: staged.bundle,
            previous: staged.previous,
            marker: marker,
            pid: ProcessInfo.processInfo.processIdentifier,
            swapID: swapID,
            launcher: launcher
        )
        let helper = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-control-swap-\(UUID().uuidString).sh")
        do {
            try Data(script.utf8).write(to: helper, options: .atomic)
            try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: helper.path(percentEncoded: false))
        } catch {
            throw InstallFailure("The update helper could not be written. \(error.localizedDescription)")
        }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = [helper.path(percentEncoded: false)]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        if let onExit {
            // The handler holds the process, so it is still there to report when the helper exits
            // whatever becomes of the call that started it.
            process.terminationHandler = { _ in onExit(process.terminationStatus) }
        }
        do {
            try process.run()
        } catch {
            throw InstallFailure("The update helper could not be started. \(error.localizedDescription)")
        }
    }

    /// Called first thing, before the app has done anything that could hang or crash. A helper
    /// waiting on this build can then stop it if it never comes up, rather than starting the old one
    /// beside it.
    static func recordLaunching(support: URL, pid: Int32 = ProcessInfo.processInfo.processIdentifier) {
        try? FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        try? Data("\(pid)\n".utf8).write(to: launchingURL(support: support), options: .atomic)
    }

    /// Called on launch. Writing the marker is how the helper learns the new build actually came up.
    /// It carries this build's pid; a helper only reads it to be sure of what it is stopping, and an
    /// older helper reads nothing but the file's existence.
    static func confirmLaunch(support: URL, pid: Int32 = ProcessInfo.processInfo.processIdentifier) {
        let marker = launchMarkerURL(support: support)
        try? FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        try? Data("\(pid)\n".utf8).write(to: marker, options: .atomic)
    }

    /// The bundle kept from the last update, if one is still there.
    ///
    /// Never the bundle running now. A copy started straight from the kept bundle, which Spotlight
    /// finds as readily as the real one, is not a build kept beside itself: going back would hand the
    /// helper the bundle it runs out of, and discarding would delete it.
    static func retainedPrevious(beside bundleURL: URL) -> URL? {
        let previous = bundleURL.deletingLastPathComponent().appending(path: previousName)
        guard bundlePath(previous) != bundlePath(bundleURL) else { return nil }
        return FileManager.default.fileExists(atPath: previous.path(percentEncoded: false)) ? previous : nil
    }

    /// The names the swap gives the two bundles beside the real one: the staged replacement and the
    /// build kept from the last update.
    static let stagedName = "Legion Control.new.app"
    static let previousName = "Legion Control.previous.app"

    /// Why a copy running from `bundleURL` must not swap, when it is one of those two rather than the
    /// installed app: staging deletes whatever sits at the staged name and the helper whatever sits
    /// at the kept one, and either would be the bundle it runs out of.
    static func siblingReason(for bundleURL: URL) -> String? {
        guard [stagedName, previousName].contains(bundleURL.lastPathComponent) else { return nil }
        return "Legion Control is running from \(bundleURL.lastPathComponent), a copy beside the installed app, so it does not install, roll back or discard builds. Do that from the installed app."
    }

    /// The version of the kept bundle, for the "roll back to X" action.
    static func version(ofBundleAt url: URL) -> String? {
        guard let info = try? plist(at: url.appending(path: "Contents/Info.plist")) else { return nil }
        return info["CFBundleShortVersionString"] as? String
    }

    /// Put the kept bundle back, deliberately, because the user asked. The same helper does it, so
    /// the failure paths are the ones that have already been thought about.
    static func rollBack(
        to previous: URL, current: URL, marker: URL,
        start: (Staged, URL, URL) throws -> Void = { try startSwap(staged: $0, current: $1, marker: $2) }
    ) throws {
        let staged = current.deletingLastPathComponent().appending(path: stagedName)
        // The helper's own vocabulary: "staged" is what goes in, "previous" is what comes out. Rolling
        // back is the same two renames with the roles swapped.
        try start(
            Staged(bundle: previous, previous: staged, version: version(ofBundleAt: previous) ?? "the previous build"),
            current,
            marker
        )
    }

    /// Throw away the bundle kept from the last update. It is a whole second copy of the app, kept
    /// only so there is something to go back to, and it stays until someone says it can go.
    static func discardRetained(beside bundleURL: URL) throws {
        guard let previous = retainedPrevious(beside: bundleURL) else { return }
        do {
            try FileManager.default.removeItem(at: previous)
        } catch {
            throw InstallFailure("\(previous.lastPathComponent) could not be deleted. \(error.localizedDescription)")
        }
    }
}
