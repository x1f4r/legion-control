import Darwin
import Foundation
import Testing
@testable import LegionControlCore

/// Running a program and getting rid of it again.
///
/// Every check here starts a real `/bin/sh`, because what is being tested is what happens to real
/// processes: which ones are still running afterwards, and whether a read is left waiting on a pipe.
struct ShellTests {

    static func scratch() throws -> URL {
        let url = FileManager.default.temporaryDirectory.appending(path: "legion-shell-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    /// The pid a script wrote, once it has.
    static func pid(in file: URL, within seconds: TimeInterval = 5) async -> pid_t? {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if let text = try? String(contentsOf: file, encoding: .utf8),
               let pid = pid_t(text.trimmingCharacters(in: .whitespacesAndNewlines)) { return pid }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return nil
    }

    /// Whether a process is gone, waiting a little for it to be collected.
    static func isGone(_ pid: pid_t, within seconds: TimeInterval = 5) async -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if kill(pid, 0) == -1, errno == ESRCH { return true }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return false
    }

    @Test("cancelling the task stops the command and everything it started")
    func cancellationStopsTheGroup() async throws {
        // M11. Cancellation used to be ignored: the command ran on to its own timeout, and whatever
        // it had started, a ProxyCommand helper for instance, ran on after that.
        let directory = try Self.scratch()
        defer { try? FileManager.default.removeItem(at: directory) }
        let marker = directory.appending(path: "child.pid")
        let started = Date()
        let task = Task {
            await Shell.run(executable: "/bin/sh",
                            arguments: ["-c", "sleep 30 & echo $! > '\(marker.path)'; wait"],
                            timeout: 60, killGrace: 1)
        }
        let child = try #require(await Self.pid(in: marker))
        defer { kill(child, SIGKILL) }
        task.cancel()
        let result = await task.value

        #expect(result.timedOut)
        #expect(result.launchFailure == nil)
        #expect(Date().timeIntervalSince(started) < 10)
        #expect(await Self.isGone(child))
    }

    @Test("a helper that ignores SIGTERM goes with the command it belonged to")
    func stragglersAreKilled() async throws {
        // The command itself stops at the first signal, but something it started does not. Its group
        // is killed while the command's own id still holds it, so the helper cannot outlive it.
        let directory = try Self.scratch()
        defer { try? FileManager.default.removeItem(at: directory) }
        let marker = directory.appending(path: "helper.pid")
        let task = Task {
            await Shell.run(executable: "/bin/sh",
                            arguments: ["-c", "(trap '' TERM; sleep 30; :) & echo $! > '\(marker.path)'; wait"],
                            timeout: 60, killGrace: 30)
        }
        let helper = try #require(await Self.pid(in: marker))
        defer { kill(helper, SIGKILL) }
        task.cancel()
        let result = await task.value
        #expect(result.timedOut)
        #expect(await Self.isGone(helper))
    }

    @Test("a command that ignores SIGTERM is killed when the grace runs out")
    func watchdogEscalates() async throws {
        let started = Date()
        let result = await Shell.run(executable: "/bin/sh", arguments: ["-c", "trap '' TERM; sleep 30"],
                                     timeout: 0.3, killGrace: 0.3)
        #expect(result.timedOut)
        // Ended by a signal, so there is no exit status to read anything from.
        #expect(result.exitCode == nil)
        #expect(Date().timeIntervalSince(started) < 10)
    }

    @Test("a command that ran to the end is not reported as ended by this app")
    func cleanExitKeepsItsStatus() async {
        let result = await Shell.run(executable: "/bin/sh", arguments: ["-c", "echo out; echo err >&2; exit 3"], timeout: 10)
        #expect(result.exitCode == 3)
        #expect(!result.timedOut)
        #expect(result.standardOutput == "out\n")
        #expect(result.standardError == "err\n")
    }

    @Test("a process ended by a signal it was not sent by this app has no exit status")
    func signalledHasNoStatus() async {
        let result = await Shell.run(executable: "/bin/sh", arguments: ["-c", "kill -9 $$"], timeout: 10)
        #expect(result.exitCode == nil)
        #expect(!result.timedOut)
    }

    @Test("output still arriving from something the command left behind is not waited for")
    func leftoverHolderDoesNotHang() async throws {
        // The command exits at once, but the sleep it started holds the output pipe open for thirty
        // seconds. The reads are stopped after the grace, not left behind waiting on it.
        let directory = try Self.scratch()
        defer { try? FileManager.default.removeItem(at: directory) }
        let marker = directory.appending(path: "holder.pid")
        let started = Date()
        let result = await Shell.run(executable: "/bin/sh",
                                     arguments: ["-c", "echo hi; sleep 30 & echo $! > '\(marker.path)'"],
                                     timeout: 20, drainGrace: 0.3)
        if let holder = await Self.pid(in: marker, within: 1) { kill(holder, SIGKILL) }
        #expect(result.standardOutput == "hi\n")
        #expect(result.exitCode == 0)
        #expect(!result.timedOut)
        #expect(Date().timeIntervalSince(started) < 5)
    }

    @Test("output past the cap is dropped, and the command still runs to the end")
    func outputIsCapped() async {
        // T11. Both streams were kept whole, however much a runaway command printed.
        let result = await Shell.run(executable: "/bin/sh",
                                     arguments: ["-c", "head -c 300000 /dev/zero | tr '\\0' a; echo done >&2"],
                                     timeout: 20, outputLimit: 1000)
        #expect(result.standardOutput.utf8.count == 1000)
        #expect(result.standardOutputTruncated)
        #expect(result.standardError == "done\n")
        #expect(!result.standardErrorTruncated)
        #expect(result.exitCode == 0)
        #expect(Shell.outputLimit == 16 * 1024 * 1024)
    }

    @Test("standard input larger than a pipe goes through while the output is read")
    func inputAndOutputTogether() async {
        let input = Data(repeating: UInt8(ascii: "x"), count: 1_000_000)
        let result = await Shell.run(executable: "/bin/cat", arguments: [], timeout: 20, input: input)
        #expect(result.exitCode == 0)
        #expect(result.standardOutput.utf8.count == input.count)
    }

    @Test("a program that cannot be started is a launch failure")
    func launchFailure() async {
        let result = await Shell.run(executable: "/nowhere/at/all", arguments: [], timeout: 5)
        #expect(result.launchFailure != nil)
        #expect(result.exitCode == nil)
    }

    // MARK: - The login PATH

    @Test("every command gets the login shell's PATH and the no-wake opt-out")
    func childEnvironment() async {
        // M4. An app started from Finder has only the system directories, so a ProxyCommand naming
        // a Homebrew tool failed here with an exit 255 that read as a sleeping machine.
        let environment = Shell.childEnvironment(base: ["PATH": "/usr/bin:/bin", "HOME": "/Users/me"],
                                                 loginPath: "/opt/tools/bin:/usr/bin:/bin")
        #expect(environment["PATH"] == "/opt/tools/bin:/usr/bin:/bin")
        #expect(environment["LEGION_NO_WAKE"] == "1")
        #expect(environment["HOME"] == "/Users/me")
        #expect(Shell.childEnvironment(base: ["PATH": "/usr/bin"], loginPath: nil)["PATH"] == "/usr/bin")

        let seen = await Shell.run(executable: "/usr/bin/printenv", arguments: ["PATH"], timeout: 10,
                                   environment: environment)
        #expect(seen.standardOutput == "/opt/tools/bin:/usr/bin:/bin\n")
    }

    /// A stand-in login shell: its profile sets a PATH and is chatty on the way in and out, and it
    /// takes `delay` seconds to get there.
    static func loginShell(in directory: URL, delay: String = "0") throws -> URL {
        let shell = directory.appending(path: "login-shell")
        try """
        #!/bin/sh
        sleep \(delay)
        PATH=/opt/tools/bin:/usr/bin:/bin; export PATH
        echo "Welcome back"
        /bin/sh -c "$3"
        echo "Logged out"
        """.write(to: shell, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: shell.path)
        return shell
    }

    @Test("the login shell's PATH is read between markers, whatever its profile prints")
    func loginPathIsRead() throws {
        let directory = try Self.scratch()
        defer { try? FileManager.default.removeItem(at: directory) }
        let shell = try Self.loginShell(in: directory)

        #expect(LoginShellPath.resolve(shell: shell.path) == "/opt/tools/bin:/usr/bin:/bin")
        #expect(LoginShellPath.resolve(shell: directory.appending(path: "missing").path) == nil)
    }

    @Test("every command can wait for the login PATH, any number of times and all at once")
    func loginPathHasManyWaiters() throws {
        // The lookup is read by every command the app runs. The first version waited on the work
        // item itself, which libdispatch allows only once: the second command the app started
        // trapped, and two machines read at once at launch took the app down together.
        let directory = try Self.scratch()
        defer { try? FileManager.default.removeItem(at: directory) }
        let shell = try Self.loginShell(in: directory, delay: "0.3")
        let lookup = LoginShellPath()
        lookup.resolveInBackground(shell: shell.path)
        // A second start changes nothing: the lookup already running is the one waited for.
        lookup.resolveInBackground(shell: directory.appending(path: "missing").path)

        // Several waiters arrive while the shell is still starting, and more after it has answered.
        final class Seen: @unchecked Sendable {
            let lock = NSLock()
            var values: [String?] = []
            func append(_ value: String?) { lock.withLock { values.append(value) } }
        }
        let seen = Seen()
        DispatchQueue.concurrentPerform(iterations: 8) { _ in seen.append(lookup.value()) }
        for _ in 0..<3 { seen.append(lookup.value()) }
        #expect(seen.values.count == 11)
        #expect(seen.values.allSatisfy { $0 == "/opt/tools/bin:/usr/bin:/bin" })
        #expect(Shell.childEnvironment(base: [:], loginPath: lookup.value())["PATH"] == "/opt/tools/bin:/usr/bin:/bin")
    }

    @Test("nothing but a list of absolute directories between the markers is taken")
    func loginPathParsing() {
        let marker = LoginShellPath.marker
        #expect(LoginShellPath.parse("noise\n\(marker)\n/a/bin:/usr/bin\n\(marker)\n") == "/a/bin:/usr/bin")
        #expect(LoginShellPath.parse("\(marker)\r\n/a/bin\r\n\(marker)\r\n") == "/a/bin")
        #expect(LoginShellPath.parse("\(marker)\nrelative/bin:/usr/bin\n\(marker)\n") == nil)
        #expect(LoginShellPath.parse("\(marker)\n\n\(marker)\n") == nil)
        #expect(LoginShellPath.parse("/usr/bin\n") == nil)
        // Without a lookup started, a command keeps the PATH it was started with, and a test run
        // never starts the app's own.
        #expect(LoginShellPath().value() == nil)
        #expect(LoginShellPath.shared.value() == nil)
    }
}
