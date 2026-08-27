import Foundation

/// Keeping Legion Control itself up to date, from the GitHub releases of whichever repository the
/// configuration names.
///
/// The same releases the phone reads, so the same conventions: one `vX.Y.Z` tag per release, the
/// version taken off the tag, and one archive of the app attached to it. What is different is the
/// install. Android hands an apk to the system installer and the system takes it from there; a Mac
/// app has to replace itself, while it is running, and then start the replacement.
///
/// That is why the order below is download, unpack, check, and only then swap. Everything that can
/// still go wrong happens in a temporary directory, and the one irreversible step is last. A build
/// that turns out to be truncated, unreadable or the wrong version must never have touched
/// /Applications, because a half replaced bundle is an app that will not start again and cannot fix
/// itself.
///
/// Built on URLSession and the command line tools that ship with the system, for the same reason the
/// rest of this project has no dependencies: two requests and four processes are not worth one.
enum AppUpdates {

    /// What a release of the Mac app is shipped as: `ditto -c -k --keepParent` of the bundle, which
    /// is what mac/build.sh produces and what scripts/release.sh attaches.
    static let assetSuffix = ".zip"

    /// The newest release, once it is known to be newer than what is running.
    struct Release: Sendable, Equatable {
        var version: String
        /// The first paragraph of the release body, or empty. Nobody reads a changelog in a
        /// settings row; the first paragraph is the sentence that says what changed.
        var notes: String
        var assetURL: URL
        var sizeBytes: Int64
    }

    enum Check: Sendable, Equatable {
        /// Nothing to install. The note carries the quiet reason when there is one worth saying,
        /// which today means a repository that has never cut a release at all.
        case upToDate(note: String?)
        case available(Release)
        case failed(reason: String)
    }

    // MARK: - Asking

    /// Ask GitHub what the newest release is, and whether it is newer than what is running.
    ///
    /// No credentials, and no way to pass any: this app only ever reads a public repository's
    /// releases. A fork that is private simply has nothing here to read, which the 404 below says
    /// as plainly as it can.
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
    /// asset. Kept apart from the request above so it can be exercised on its own, because this is
    /// the part that has an answer that can be wrong rather than merely absent.
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
        guard let asset = assets.first(where: { ($0["name"] as? String)?.hasSuffix(assetSuffix) == true }),
              // The asset API url rather than browser_download_url: the same one line of code then
              // works for a public repository and, with a header added, for a private fork.
              let assetURL = (asset["url"] as? String).flatMap(URL.init(string:))
        else {
            return .failed(reason: "Release \(latest) has no \(assetSuffix) attached, so there is nothing to install.")
        }

        return .available(Release(
            version: latest,
            notes: firstParagraph(of: root["body"] as? String ?? ""),
            assetURL: assetURL,
            sizeBytes: (asset["size"] as? NSNumber)?.int64Value ?? 0
        ))
    }

    /// The tag with its leading `v` taken off, which is the whole of the release naming convention.
    static func version(fromTag tag: String) -> String {
        let trimmed = tag.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("v") else { return trimmed }
        return String(trimmed.dropFirst())
    }

    /// Numeric field by field, so 1.0.10 is correctly newer than 1.0.9. A string comparison gets
    /// that backwards, and it is exactly the version where it would start to matter.
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

    /// The first paragraph of a release body, as one line. Release notes are prose separated by
    /// blank lines, and the row this lands in is one sentence wide.
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

    /// Why an install stopped, as a sentence to put on screen. Every one of these names the thing
    /// that went wrong rather than the step that noticed it.
    struct InstallFailure: Error, Sendable {
        var message: String

        init(_ message: String) { self.message = message }
    }

    /// A downloaded archive and the directory holding it. The caller owns both and hands them back
    /// to `discard` when it is done, whether the install went through or not.
    struct Download: Sendable {
        var directory: URL
        var archive: URL
    }

    /// Fetch the asset into a directory of its own. Nothing outside that directory is touched here.
    static func download(_ release: Release) async throws -> Download {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-control-update-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        } catch {
            throw InstallFailure("A temporary directory for the download could not be made. \(error.localizedDescription)")
        }

        var request = URLRequest(url: release.assetURL, timeoutInterval: 60)
        request.setValue("application/octet-stream", forHTTPHeaderField: "Accept")
        request.setValue("legion-control", forHTTPHeaderField: "User-Agent")

        let archive = directory.appending(path: "Legion-Control.zip")
        do {
            // A download task rather than a data task: the archive is tens of megabytes and there is
            // no reason for all of it to sit in memory on its way to a file.
            let (temporary, response) = try await URLSession.shared.download(for: request)
            let code = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard code == 200 else {
                try? FileManager.default.removeItem(at: directory)
                throw InstallFailure("The download failed. GitHub answered \(code).")
            }
            try FileManager.default.moveItem(at: temporary, to: archive)
        } catch let failure as InstallFailure {
            throw failure
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw InstallFailure("The download failed. \(error.localizedDescription)")
        }

        // A truncated archive unpacks into something that looks almost right, so the size is checked
        // here where it is still only a file in /tmp.
        let written = (try? FileManager.default.attributesOfItem(atPath: archive.path)[.size] as? NSNumber)??.int64Value ?? 0
        if release.sizeBytes > 0, written != release.sizeBytes {
            try? FileManager.default.removeItem(at: directory)
            throw InstallFailure("The download stopped at \(written) of \(release.sizeBytes) bytes.")
        }

        return Download(directory: directory, archive: archive)
    }

    /// Unpack, check what came out, and only then replace the bundle this app is running out of.
    static func apply(_ download: Download, version: String, replacing bundleURL: URL) async throws {
        guard bundleURL.pathExtension == "app" else {
            throw InstallFailure("Legion Control is not running out of an app bundle, so there is nothing to replace. Build and install it with mac/build.sh instead.")
        }

        let unpacked = download.directory.appending(path: "unpacked")
        let extraction = await Shell.run(
            executable: "/usr/bin/ditto",
            arguments: ["-x", "-k", download.archive.path, unpacked.path],
            timeout: 180
        )
        guard extraction.succeeded else {
            throw InstallFailure("The download could not be unpacked. \(extraction.failureText)")
        }

        let replacement = try bundle(in: unpacked)
        try await verify(replacement, is: version)

        // The build is ad hoc signed and arrived over the network, so it carries the quarantine flag
        // and Gatekeeper would refuse to launch it with no way to say yes to something that has no
        // developer identity to trust. Stripped here, before it becomes the app in /Applications.
        _ = await Shell.run(
            executable: "/usr/bin/xattr",
            arguments: ["-dr", "com.apple.quarantine", replacement.path],
            timeout: 60
        )

        // rsync rather than a move: /Applications/Legion Control.app is the bundle this process is
        // running out of, and replacing the directory wholesale would pull it out from under a
        // running app. rsync writes each file beside its target and renames it over the top, so the
        // running executable keeps the inode it started on and nothing is missing in between.
        let swap = await Shell.run(
            executable: "/usr/bin/rsync",
            arguments: ["-a", "--delete", replacement.path + "/", bundleURL.path + "/"],
            timeout: 180
        )
        guard swap.succeeded else {
            throw InstallFailure("The new build could not be put in place. \(swap.failureText)")
        }
    }

    /// Throws the download away. Safe to call twice, and called on the way out of a failure as well
    /// as a success: a few tens of megabytes in /tmp is not something to leave behind either way.
    static func discard(_ download: Download) {
        try? FileManager.default.removeItem(at: download.directory)
    }

    /// Start the replacement a moment from now, from a process that is not this one.
    ///
    /// `open` on a bundle that is mid-quit gets the app that is already running rather than a new
    /// one, so the launch is handed to a shell that waits a second first and outlives us. Nothing
    /// waits on it: by the time it runs, this process is meant to be gone.
    static func relaunch(_ bundleURL: URL) {
        let quoted = bundleURL.path
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", "sleep 1; open -n \"\(quoted)\""]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try? process.run()
    }

    // MARK: - Checking what was unpacked

    /// The one app bundle the archive held. Anything else means the asset is not what this expects,
    /// and guessing at that point would be guessing about what to overwrite /Applications with.
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

    /// That the thing about to be copied over /Applications really is this app, and really is the
    /// version the release said it was. Both checks are cheap and both have caught the two ways a
    /// release goes wrong: an asset built from the wrong tree, and a tag bumped without a rebuild.
    private static func verify(_ bundle: URL, is version: String) async throws {
        let executable = bundle.appending(path: "Contents/MacOS/LegionControl")
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            throw InstallFailure("The downloaded bundle has no Contents/MacOS/LegionControl in it, so it is not this app.")
        }

        let plist = bundle.appending(path: "Contents/Info.plist")
        let read = await Shell.run(
            executable: "/usr/libexec/PlistBuddy",
            arguments: ["-c", "Print :CFBundleShortVersionString", plist.path],
            timeout: 30
        )
        guard read.succeeded else {
            throw InstallFailure("The downloaded bundle's version could not be read. \(read.failureText)")
        }
        let found = read.standardOutput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard found == version else {
            throw InstallFailure("The release says \(version) but the bundle in it says \(found), so it was not installed.")
        }
    }
}
