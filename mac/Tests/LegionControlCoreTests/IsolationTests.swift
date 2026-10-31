import Foundation
import Testing
@testable import LegionControlCore

/// No test writes into the real app state.
///
/// The TestHome target gives the whole run a LEGION_CONTROL_HOME of its own before any test starts
/// (Tests/TestHome/home.c). Before it, a test that reached a store through its default location
/// wrote into this Mac's real one: a hundred refused bootstrap archives in the application support
/// folder, and remembered settings in the test runner's preferences. This is what fails when that
/// stops holding, rather than the real folder quietly filling up again.
@MainActor
struct IsolationTests {
    static func resolved(_ url: URL) -> String { url.resolvingSymlinksInPath().standardizedFileURL.path }

    @Test("every test runs under a home of the run's own, and nothing the app writes resolves outside it")
    func noTestReachesTheRealHome() throws {
        let home = try #require(AppPaths.homeOverride, "LEGION_CONTROL_HOME is not set: the TestHome target is not linked into the tests")
        let root = Self.resolved(home)
        #expect(root.hasPrefix(Self.resolved(FileManager.default.temporaryDirectory) + "/"))
        #expect(home.lastPathComponent.hasPrefix("legion-control-tests-"))
        // The config file is named on its own by LEGION_CONTROL_CONFIG, home or no home.
        #expect(AppPaths.configOverride == nil)

        let written: [(String, URL)] = [
            ("the config", AppPaths.configFile), ("the bindings", AppPaths.bindingsFile),
            ("the revisions", AppPaths.revisionsDirectory), ("the app's state", AppPaths.support),
            ("the staged agent archives", AppPaths.file("bootstrap")), ("ssh's logs", SSHLog.directory),
            ("known_hosts", AppPaths.knownHostsFile), ("the generated key", AppPaths.defaultIdentityFile),
            ("the operation history", OperationStore().url), ("the host pins", HostIdentityStore().url),
            ("the private settings", BindingsStore().url), ("the launch marker", AppUpdateModel().supportDirectory),
        ]
        for (what, url) in written {
            #expect(Self.resolved(url).hasPrefix(root + "/"), "\(what) resolves to \(url.path)")
        }
        // ssh is told about the run's own known_hosts rather than reading the user's.
        #expect(AppPaths.overridesKnownHosts)
    }

    @Test("preferences go to the run's own file, never to the test runner's defaults")
    func preferencesStayInTheRun() throws {
        // Without a home of its own, the write below would land in the real defaults.
        try #require(AppPaths.homeOverride != nil, "LEGION_CONTROL_HOME is not set: the TestHome target is not linked into the tests")
        let key = "isolation-\(UUID().uuidString)"
        AppPreferences.set(true, forKey: key)
        defer { AppPreferences.removeObject(forKey: key) }
        #expect(AppPreferences.bool(forKey: key) == true)
        #expect(UserDefaults.standard.object(forKey: key) == nil)
        #expect(FileManager.default.fileExists(atPath: AppPaths.file("preferences.json").path))
    }
}
