import Foundation

/// Where this app keeps everything it writes.
///
/// One root, so a test run or a second setup tried alongside the real one gets its own config, its
/// own history, its own remembered settings, its own key and its own known_hosts, rather than
/// writing over the setup that is actually in use. An override that relocates the config file and
/// then quietly goes on reading `~/.ssh` is not isolation; it is a trap.
enum AppPaths {
    /// `LEGION_CONTROL_HOME` relocates everything. `LEGION_CONTROL_CONFIG` still names the config
    /// file alone, for the case where someone wants to point the app at a document and nothing else.
    static var homeOverride: URL? {
        if let value = ProcessInfo.processInfo.environment["LEGION_CONTROL_HOME"], !value.isEmpty {
            return URL(fileURLWithPath: (value as NSString).expandingTildeInPath)
        }
        return nil
    }

    static var configOverride: URL? {
        if let value = ProcessInfo.processInfo.environment["LEGION_CONTROL_CONFIG"], !value.isEmpty {
            return URL(fileURLWithPath: (value as NSString).expandingTildeInPath)
        }
        return nil
    }

    /// True when the app was pointed somewhere other than its usual home.
    static var isRedirected: Bool { homeOverride != nil || configOverride != nil }

    /// The directory the config file lives in.
    static var configDirectory: URL {
        if let home = homeOverride { return home }
        if let config = configOverride { return config.deletingLastPathComponent() }
        return FileManager.default.homeDirectoryForCurrentUser.appending(path: ".config/legion-control")
    }

    /// The shared, publishable setup.
    static var configFile: URL {
        if let config = configOverride { return config }
        return configDirectory.appending(path: "config.json")
    }

    /// This device's own settings, which are never published to anyone.
    static var bindingsFile: URL { configDirectory.appending(path: "bindings.json") }

    /// Superseded documents, one file per hash. Kept because a three-way merge needs the common
    /// ancestor's bytes, and because an edit made here should be recoverable.
    static var revisionsDirectory: URL { configDirectory.appending(path: "revisions") }

    /// State this app owns: the operation history, the update marker.
    static var support: URL {
        if let home = homeOverride { return home.appending(path: "state") }
        if let config = configOverride { return config.deletingLastPathComponent().appending(path: "state") }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? FileManager.default.homeDirectoryForCurrentUser.appending(path: "Library/Application Support")
        return base.appending(path: "Legion Control")
    }

    static func file(_ name: String) -> URL { support.appending(path: name) }

    /// Where ssh material lives.
    ///
    /// Under an override this is a directory of the run's own, and the transport is told about it
    /// with `-o UserKnownHostsFile`. Without one it is the user's real `~/.ssh`, which is the whole
    /// point: an ordinary run must use the ssh setup the user already has, aliases and all.
    static var sshDirectory: URL {
        if let home = homeOverride { return home.appending(path: "ssh") }
        return FileManager.default.homeDirectoryForCurrentUser.appending(path: ".ssh")
    }

    static var knownHostsFile: URL { sshDirectory.appending(path: "known_hosts") }

    /// The key this device offers when the setup names none. Only generated on request.
    static var defaultIdentityFile: URL { sshDirectory.appending(path: "legion-control_ed25519") }

    /// True when the app should tell ssh which known_hosts to use. Off in an ordinary run, so the
    /// user's own file and their own `~/.ssh/config` keep working exactly as before.
    static var overridesKnownHosts: Bool { homeOverride != nil }

    static func ensureSupportDirectory() throws {
        try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
    }

}

/// The few preferences that are not part of the shared setup.
///
/// A normal installation uses the app's ordinary defaults domain. An isolated run cannot use that
/// domain under a different key prefix: it would still write a plist in the user's real Library.
/// Its preferences therefore live in the isolated state directory with every other local file.
enum AppPreferences {
    private static let lock = NSLock()

    static func data(forKey key: String) -> Data? {
        if AppPaths.homeOverride == nil { return UserDefaults.standard.data(forKey: key) }
        return lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).data(forKey: key) }
    }

    static func string(forKey key: String) -> String? {
        if AppPaths.homeOverride == nil { return UserDefaults.standard.string(forKey: key) }
        return lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).string(forKey: key) }
    }

    static func bool(forKey key: String) -> Bool? {
        if AppPaths.homeOverride == nil { return UserDefaults.standard.object(forKey: key) as? Bool }
        return lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).bool(forKey: key) }
    }

    static func set(_ data: Data, forKey key: String) {
        if AppPaths.homeOverride == nil {
            UserDefaults.standard.set(data, forKey: key)
        } else {
            lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).set(data, forKey: key) }
        }
    }

    static func set(_ value: String, forKey key: String) {
        if AppPaths.homeOverride == nil {
            UserDefaults.standard.set(value, forKey: key)
        } else {
            lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).set(value, forKey: key) }
        }
    }

    static func set(_ value: Bool, forKey key: String) {
        if AppPaths.homeOverride == nil {
            UserDefaults.standard.set(value, forKey: key)
        } else {
            lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).set(value, forKey: key) }
        }
    }

    static func removeObject(forKey key: String) {
        if AppPaths.homeOverride == nil {
            UserDefaults.standard.removeObject(forKey: key)
        } else {
            lock.withLock { FilePreferences(url: AppPaths.file("preferences.json")).removeObject(forKey: key) }
        }
    }
}

/// A deliberately small file-backed defaults store. Values are tagged strings so old or malformed
/// files fail one key at a time rather than resetting unrelated remembered state.
struct FilePreferences {
    let url: URL

    private func load() -> [String: String] {
        guard let data = try? Data(contentsOf: url),
              let values = try? JSONDecoder().decode([String: String].self, from: data)
        else { return [:] }
        return values
    }

    private func save(_ values: [String: String]) {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        guard let data = try? encoder.encode(values) else { return }
        try? AtomicFile.write(data, to: url)
    }

    func data(forKey key: String) -> Data? {
        guard let value = load()[key], value.hasPrefix("data:") else { return nil }
        return Data(base64Encoded: String(value.dropFirst(5)))
    }

    func string(forKey key: String) -> String? {
        guard let value = load()[key], value.hasPrefix("string:") else { return nil }
        return String(value.dropFirst(7))
    }

    func bool(forKey key: String) -> Bool? {
        switch load()[key] {
        case "bool:true": true
        case "bool:false": false
        default: nil
        }
    }

    func set(_ data: Data, forKey key: String) {
        update(key, value: "data:\(data.base64EncodedString())")
    }

    func set(_ value: String, forKey key: String) {
        update(key, value: "string:\(value)")
    }

    func set(_ value: Bool, forKey key: String) {
        update(key, value: "bool:\(value)")
    }

    func removeObject(forKey key: String) {
        update(key, value: nil)
    }

    private func update(_ key: String, value: String?) {
        var values = load()
        values[key] = value
        save(values)
    }
}

/// Writing a small document to disk without the two ways it usually goes wrong: a shared temporary
/// name that two writers fight over, and a failure nobody hears about.
enum AtomicFile {
    struct WriteFailure: Error, Sendable {
        var message: String
    }

    /// Write into a uniquely named temporary file in the same directory, then rename it over the
    /// target. The rename is atomic on the same volume, so a reader sees either the old document or
    /// the new one and never a half written file.
    static func write(_ data: Data, to url: URL) throws {
        let directory = url.deletingLastPathComponent()
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        } catch {
            throw WriteFailure(message: "\(directory.path(percentEncoded: false)) could not be made. \(error.localizedDescription)")
        }
        // A unique name per write. A shared `.tmp` is a file two processes can be inside at once,
        // and the loser's bytes end up in the winner's document.
        let temporary = directory.appending(path: ".\(url.lastPathComponent).\(UUID().uuidString).tmp")
        do {
            try data.write(to: temporary, options: [.atomic])
            if FileManager.default.fileExists(atPath: url.path(percentEncoded: false)) {
                _ = try FileManager.default.replaceItemAt(url, withItemAt: temporary)
            } else {
                try FileManager.default.moveItem(at: temporary, to: url)
            }
        } catch {
            try? FileManager.default.removeItem(at: temporary)
            throw WriteFailure(message: "\(url.lastPathComponent) could not be written. \(error.localizedDescription)")
        }
    }
}
