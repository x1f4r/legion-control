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
    static func check(repo: String, installedVersion: String) async -> Check {
        guard let url = URL(string: "https://api.github.com/repos/\(repo)/releases/latest") else {
            return .failed(reason: "\"\(repo)\" is not a repository name this can ask about.")
        }

        var request = URLRequest(url: url, timeoutInterval: 10)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("legion-control", forHTTPHeaderField: "User-Agent")

        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            let code = (response as? HTTPURLResponse)?.statusCode ?? 0
            // 404 is what a public repository that has never cut a release answers, and it is also
            // what a private one answers to anyone who cannot see it. Neither is a failure worth
            // colouring: there is nothing to install, and saying so quietly is the honest reading.
            if code == 404 {
                return .upToDate(note: "\(repo) has no release yet, so there is nothing newer to install.")
            }
            guard code == 200 else { return .failed(reason: "GitHub answered \(code).") }
            return read(releaseJSON: data, installedVersion: installedVersion)
        } catch {
            return .failed(reason: error.localizedDescription)
        }
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
        func fields(_ value: String) -> [Int] {
            value.split(separator: "-", maxSplits: 1)[0]
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
    static func download(_ release: Release, session: URLSession = .shared) async throws -> Download {
        guard let manifestURL = release.manifestURL, let signatureURL = release.signatureURL else {
            throw InstallFailure("""
                Release \(release.version) does not carry a signed manifest, so this build cannot \
                establish that it really is a Legion Control release. Nothing was installed. Download \
                it yourself if you know it is genuine.
                """)
        }

        let manifestBytes = try await fetch(manifestURL, what: "the release manifest", session: session)
        let signatureBytes = try await fetch(signatureURL, what: "the manifest signature", session: session)

        let manifest: ReleaseTrust.Manifest
        do {
            manifest = try ReleaseTrust.verifiedManifest(bytes: manifestBytes, signature: signatureBytes)
        } catch let failure as ReleaseTrust.TrustFailure {
            throw InstallFailure(failure.message)
        }

        guard manifest.version == release.version else {
            throw InstallFailure("The signed manifest is for version \(manifest.version) and the release says \(release.version), so nothing was installed.")
        }
        guard let artifact = manifest.artifact(named: assetName) else {
            throw InstallFailure("The signed manifest does not name \(assetName), so there is nothing for this Mac to install.")
        }

        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-control-update-\(UUID().uuidString)")
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
            // A download task rather than a data task: the archive is tens of megabytes and there is
            // no reason for all of it to sit in memory on its way to a file.
            let (temporary, response) = try await session.download(for: request)
            let code = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard code == 200 else {
                throw InstallFailure("The download failed. GitHub answered \(code).")
            }
            try FileManager.default.moveItem(at: temporary, to: archive)
        } catch let failure as InstallFailure {
            try? FileManager.default.removeItem(at: directory)
            throw failure
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

    private static func fetch(_ url: URL, what: String, session: URLSession) async throws -> Data {
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.setValue("application/octet-stream", forHTTPHeaderField: "Accept")
        request.setValue("legion-control", forHTTPHeaderField: "User-Agent")
        do {
            let (data, response) = try await session.data(for: request)
            let code = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard code == 200 else {
                throw InstallFailure("\(what) could not be downloaded. GitHub answered \(code).")
            }
            return data
        } catch let failure as InstallFailure {
            throw failure
        } catch {
            throw InstallFailure("\(what) could not be downloaded. \(error.localizedDescription)")
        }
    }

    static func discard(_ download: Download) {
        try? FileManager.default.removeItem(at: download.directory)
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

            let staged = parent.appending(path: "Legion Control.new.app")
            try? FileManager.default.removeItem(at: staged)
            try FileManager.default.moveItem(at: unpacked, to: staged)
            try? FileManager.default.removeItem(at: staging)

            return Staged(
                bundle: staged,
                previous: parent.appending(path: "Legion Control.previous.app"),
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

    /// The helper script that does the dangerous part from outside this process.
    ///
    /// It cannot be done from inside. The swap moves the bundle this executable is running out of,
    /// and the recovery has to happen after this process is gone — which is exactly when the new
    /// build failing to launch would leave a user with no app and nothing to run to fix it.
    ///
    /// The sequence: wait for this process to exit, move the current bundle aside, move the staged
    /// one into place, launch it, and wait for it to say it came up. If it does not, put the old
    /// bundle back and launch that instead. Every path ends with exactly one bundle at the real
    /// path, and it is always one of the two whole bundles rather than a mixture.
    static func swapScript(
        current: URL,
        staged: URL,
        previous: URL,
        marker: URL,
        pid: Int32,
        exitTimeout: Int = 10,
        launchTimeout: Int = 45
    ) -> String {
        func quoted(_ url: URL) -> String { RemoteShell.posixQuoted(url.path(percentEncoded: false)) }
        return """
        #!/bin/sh
        set -u
        CURRENT=\(quoted(current))
        STAGED=\(quoted(staged))
        PREVIOUS=\(quoted(previous))
        MARKER=\(quoted(marker))
        PID=\(pid)

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
            exit 3
        fi

        rm -f "$MARKER"
        rm -rf "$PREVIOUS"

        if ! mv "$CURRENT" "$PREVIOUS"; then
            # Nothing has moved, so the running build is still exactly where it was.
            open -n "$CURRENT" 2>/dev/null || true
            exit 1
        fi

        if ! mv "$STAGED" "$CURRENT"; then
            # The only window in which the real path holds nothing. Put it back and stop.
            mv "$PREVIOUS" "$CURRENT"
            open -n "$CURRENT" 2>/dev/null || true
            exit 1
        fi

        open -n "$CURRENT" 2>/dev/null || true

        # The new build writes the marker as soon as it has finished launching. No marker means it
        # crashed on the way up, or refused to start at all, and the old one goes back.
        i=0
        while [ $i -lt \(launchTimeout * 10) ]; do
            if [ -f "$MARKER" ]; then
                exit 0
            fi
            sleep 0.1
            i=$((i + 1))
        done

        # Recovery. The new build is put where the previous one was rather than deleted, so a failure
        # that turns out to be about something else is still diagnosable.
        rm -rf "$STAGED"
        if mv "$CURRENT" "$STAGED" && mv "$PREVIOUS" "$CURRENT"; then
            open -n "$CURRENT" 2>/dev/null || true
        fi
        exit 2
        """
    }

    /// Where the new build says "I came up".
    static func launchMarkerURL(support: URL) -> URL {
        support.appending(path: "launched-ok")
    }

    /// Write the swap helper and start it detached, then it is out of this process's hands.
    static func startSwap(staged: Staged, current: URL, marker: URL) throws {
        let script = swapScript(
            current: current,
            staged: staged.bundle,
            previous: staged.previous,
            marker: marker,
            pid: ProcessInfo.processInfo.processIdentifier
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
        do {
            try process.run()
        } catch {
            throw InstallFailure("The update helper could not be started. \(error.localizedDescription)")
        }
    }

    /// Called on launch. Writing the marker is how the helper learns the new build actually came up,
    /// and clearing the kept bundle is the last step of an update that worked.
    static func confirmLaunch(support: URL) {
        let marker = launchMarkerURL(support: support)
        try? FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        try? Data("ok".utf8).write(to: marker, options: .atomic)
    }

    /// The bundle kept from the last update, if one is still there.
    static func retainedPrevious(beside bundleURL: URL) -> URL? {
        let previous = bundleURL.deletingLastPathComponent().appending(path: "Legion Control.previous.app")
        return FileManager.default.fileExists(atPath: previous.path(percentEncoded: false)) ? previous : nil
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
        let staged = current.deletingLastPathComponent().appending(path: "Legion Control.new.app")
        // The helper's own vocabulary: "staged" is what goes in, "previous" is what comes out. Rolling
        // back is the same two renames with the roles swapped.
        try start(
            Staged(bundle: previous, previous: staged, version: version(ofBundleAt: previous) ?? "the previous build"),
            current,
            marker
        )
    }

    /// Throw away the bundle kept from the last update.
    static func discardRetained(beside bundleURL: URL) {
        guard let previous = retainedPrevious(beside: bundleURL) else { return }
        try? FileManager.default.removeItem(at: previous)
    }
}
