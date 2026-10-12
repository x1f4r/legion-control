import Darwin
import Dispatch
import Foundation

struct CommandResult: Sendable {
    /// The status the process exited with, or nil when it never exited on its own: a signal ended
    /// it, or it was never started. Section 15.4 of the contract reads a missing status as a link
    /// that went away, which is a different thing from any number a process can exit with.
    var exitCode: Int32?
    var standardOutput: String
    var standardError: String
    /// True when this app ended the process: its watchdog fired or the task that ran it was
    /// cancelled. Either way the far side may have been halfway through something.
    var timedOut: Bool
    /// True when the task that ran it was cancelled before the command had finished, whether it
    /// had been started or not. The watchdog firing is evidence about the far side; a cancellation
    /// is only evidence that nobody wants the answer any more, and a transport must not read it as
    /// a route that failed.
    var cancelled = false
    var launchFailure: String?
    /// True when a stream went past `Shell.outputLimit` and the rest of it was thrown away. Output
    /// that did not arrive whole is never read as an agent's reply.
    var standardOutputTruncated = false
    var standardErrorTruncated = false

    var succeeded: Bool { exitCode == 0 && !timedOut && launchFailure == nil }

    /// The most useful line to show a human when something went wrong. Only a few lines of this ever
    /// reach the screen, and both ssh and node bury the one sentence that matters under a routing
    /// banner, stack frames and caret art, so the lines that carry no message are dropped first.
    var failureText: String {
        if let launchFailure { return launchFailure }
        if cancelled { return "The request was cancelled before the command finished." }
        if timedOut { return "The command did not finish in time." }
        let fromError = Self.informativeLines(standardError)
        if !fromError.isEmpty { return fromError }
        let fromOutput = Self.informativeLines(standardOutput)
        if !fromOutput.isEmpty { return fromOutput }
        guard let exitCode else { return "The command ended without an exit status." }
        return "Exit code \(exitCode)."
    }

    static func informativeLines(_ text: String) -> String {
        text
            .split(separator: "\n", omittingEmptySubsequences: false)
            // whitespacesAndNewlines, not whitespaces: Windows sends CRLF and the CR would survive.
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { line in
                if line.isEmpty { return false }
                // Node prints one frame per line after the message itself.
                if line.hasPrefix("at ") { return false }
                // fish and node both underline the offending token on its own line.
                if line.allSatisfy({ $0 == "^" || $0 == "~" }) { return false }
                return true
            }
            .joined(separator: "\n")
    }
}

/// Runs one program and collects what it says. Everything runs on a background queue so the UI never
/// blocks, and both output streams are read as they arrive so a chatty command cannot deadlock a pipe.
///
/// The program is started in a process group of its own, and that group is what gets stopped: ssh
/// hands its connection to a ProxyCommand helper, and the local agent starts children of its own, so
/// ending only the process this app started used to leave the rest running. Stopping it is a
/// SIGTERM, then a SIGKILL a few seconds later that is called off the moment the process has exited,
/// before its id is given back, so a late kill can never land on a process that has since been
/// given the same id.
enum Shell {
    /// What is kept of each output stream. An agent's reply is a few kilobytes and a diagnostic
    /// bundle a few megabytes; past this the output is a runaway, and keeping all of it would only
    /// trade a stuck command for an app that runs out of memory.
    static let outputLimit = 16 * 1024 * 1024

    /// `input` is the standard input to hand the command, for the one thing that sends a document
    /// rather than asking for one. Without it the command gets /dev/null, which is what everything
    /// else here wants: nothing should ever sit waiting on a terminal that is not there.
    ///
    /// Cancelling the task that awaits this stops the command the way the watchdog does, and the
    /// result says this app ended it rather than that it finished, and that a cancellation was why.
    ///
    /// The rest is for the tests: `environment` replaces the one every command normally gets,
    /// `killGrace` is the time between SIGTERM and SIGKILL, and `drainGrace` is how long output may
    /// keep arriving after the process has exited.
    static func run(
        executable: String,
        arguments: [String],
        timeout: TimeInterval,
        input: Data? = nil,
        environment: [String: String]? = nil,
        outputLimit: Int = Shell.outputLimit,
        killGrace: TimeInterval = 3,
        drainGrace: TimeInterval = 2
    ) async -> CommandResult {
        let control = RunControl(killGrace: killGrace)
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                // A thread of its own, not a block on a global queue. This thread spends the whole
                // command blocked in waitid and in the wait for the readers, and the readers are
                // DispatchIO handlers that need a free global-queue thread to deliver anything. GCD
                // keeps that pool near one thread per core, so on a three-core machine three commands
                // in flight left no thread to read their output, and a command that printed was
                // reported as having printed nothing once the drain deadline passed.
                Thread.detachNewThread {
                    continuation.resume(returning: runBlocking(
                        executable: executable,
                        arguments: arguments,
                        timeout: timeout,
                        input: input,
                        // Worked out here rather than in the caller: the first command waits for the
                        // login shell to answer, and that wait belongs on this thread, not on one of
                        // the few that run every task in the app.
                        environment: environment ?? childEnvironment(),
                        outputLimit: outputLimit,
                        drainGrace: drainGrace,
                        control: control
                    ))
                }
            }
        } onCancel: {
            control.cancel()
        }
    }

    /// The environment every command gets: this app's own, with two changes.
    static func childEnvironment(
        base: [String: String] = ProcessInfo.processInfo.environment,
        loginPath: String? = LoginShellPath.shared.value()
    ) -> [String: String] {
        var environment = base
        // Never wake a machine just by looking at it.
        //
        // ssh ProxyCommand helpers that wake a machine when it does not answer can honour this to
        // stay quiet. That kind of helper is right for a human opening a shell and completely wrong
        // here: this app polls every fifteen seconds, so a sleeping machine is woken by the next
        // status check and sleep can never stick. Measured on a machine set up that way: it came
        // back about a minute after going down, with no wake timer armed, so the only thing that
        // could have done it was our own transport.
        //
        // ProxyCommand inherits this environment, which is what makes the opt-out reach the helper.
        // The Wake button does not go through here at all: it sends its own packet from Swift.
        environment["LEGION_NO_WAKE"] = "1"
        // An app started from Finder or at login has only the bare system PATH, so a ProxyCommand
        // naming a Homebrew tool worked in Terminal and failed here, with an exit 255 that read as
        // a sleeping machine. The PATH a login shell sets up is the one the user's ssh config was
        // written against.
        if let loginPath { environment["PATH"] = loginPath }
        return environment
    }

    fileprivate static func runBlocking(
        executable: String,
        arguments: [String],
        timeout: TimeInterval,
        input: Data?,
        environment: [String: String],
        outputLimit: Int,
        drainGrace: TimeInterval,
        control: RunControl
    ) -> CommandResult {
        func launchFailure(_ message: String) -> CommandResult {
            CommandResult(exitCode: nil, standardOutput: "", standardError: "", timedOut: false,
                          launchFailure: message)
        }

        guard let output = Pipes() else {
            return launchFailure("Could not run \(executable): no pipe could be made for its output.")
        }
        guard let errors = Pipes() else {
            output.closeBoth()
            return launchFailure("Could not run \(executable): no pipe could be made for its output.")
        }
        var inputPipe: Pipes?
        if input != nil {
            guard let pipe = Pipes() else {
                output.closeBoth(); errors.closeBoth()
                return launchFailure("Could not run \(executable): no pipe could be made for its input.")
            }
            inputPipe = pipe
        }

        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        if let inputPipe {
            posix_spawn_file_actions_adddup2(&actions, inputPipe.read, STDIN_FILENO)
        } else {
            posix_spawn_file_actions_addopen(&actions, STDIN_FILENO, "/dev/null", O_RDONLY, 0)
        }
        posix_spawn_file_actions_adddup2(&actions, output.write, STDOUT_FILENO)
        posix_spawn_file_actions_adddup2(&actions, errors.write, STDERR_FILENO)

        var attributes: posix_spawnattr_t?
        posix_spawnattr_init(&attributes)
        defer { posix_spawnattr_destroy(&attributes) }
        // A group of its own, so everything it starts can be stopped together. Nothing but the three
        // standard streams is inherited, and the signal state is a fresh one: whatever this app
        // ignores or blocks is not the command's business.
        posix_spawnattr_setflags(&attributes, Int16(POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_CLOEXEC_DEFAULT
                                                     | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))
        posix_spawnattr_setpgroup(&attributes, 0)
        var defaults = sigset_t()
        sigfillset(&defaults)
        posix_spawnattr_setsigdefault(&attributes, &defaults)
        var mask = sigset_t()
        sigemptyset(&mask)
        posix_spawnattr_setsigmask(&attributes, &mask)

        let argv: [UnsafeMutablePointer<CChar>?] = ([executable] + arguments).map { strdup($0) } + [nil]
        let envp: [UnsafeMutablePointer<CChar>?] = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer {
            argv.forEach { free($0) }
            envp.forEach { free($0) }
        }

        // The task may have been cancelled while this waited for a thread. Nothing has been started
        // yet, so nothing has to be stopped, and nothing ran.
        if control.isStopping {
            output.closeBoth(); errors.closeBoth(); inputPipe?.closeBoth()
            var result = launchFailure("\(executable) was not started: the request was cancelled first.")
            result.cancelled = true
            return result
        }

        var pid: pid_t = 0
        let spawned = posix_spawn(&pid, executable, &actions, &attributes, argv, envp)
        // The child has its own copies of these now. Holding the write ends open here would mean the
        // output pipes never reach end of file.
        close(output.write)
        close(errors.write)
        if let inputPipe { close(inputPipe.read) }
        guard spawned == 0 else {
            close(output.read)
            close(errors.read)
            if let inputPipe { close(inputPipe.write) }
            return launchFailure("Could not run \(executable): \(String(cString: strerror(spawned))).")
        }
        control.started(pid)

        // Read as it arrives rather than waiting for end of file, so whatever the command wrote is
        // already in hand even if the pipe never closes. See the drain deadline below for why that
        // happens.
        let readers = DispatchGroup()
        let standardOutput = StreamReader(fileDescriptor: output.read, limit: outputLimit, group: readers)
        let standardError = StreamReader(fileDescriptor: errors.read, limit: outputLimit, group: readers)

        // Written through its own channel, never from this thread. A document larger than the pipe
        // buffer only goes through as fast as the command reads it, and writing it here would mean
        // nobody is waiting for the process in the meantime.
        var writer: StreamWriter?
        if let inputPipe, let input { writer = StreamWriter(fileDescriptor: inputPipe.write, data: input) }

        let watchdog = DispatchWorkItem { control.stop() }
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + timeout, execute: watchdog)

        // Wait for the exit without collecting it. Until it is collected the id still belongs to this
        // process, so the kill timer is called off while that is still true.
        var info = siginfo_t()
        while waitid(P_PID, id_t(pid), &info, WEXITED | WNOWAIT) == -1, errno == EINTR {}
        watchdog.cancel()
        control.exited()
        var status: Int32 = 0
        var collected: pid_t = -1
        repeat { collected = waitpid(pid, &status, 0) } while collected == -1 && errno == EINTR

        // The pipe stays open as long as anything still holds the far end, and ssh hands its own
        // stderr to the ProxyCommand, so a helper that outlives ssh would keep an unbounded wait here
        // alive and the timeout above would mean nothing. The command itself is already gone, so
        // give the readers a moment to settle, then stop them and take what they have. A stopped
        // read is over for good: nothing is left behind waiting on a pipe.
        if readers.wait(timeout: .now() + drainGrace) == .timedOut {
            standardOutput.stop()
            standardError.stop()
            _ = readers.wait(timeout: .now() + 1)
        }
        writer?.stop()

        let exited = collected == pid && status & 0x7f == 0
        let out = standardOutput.collected()
        let err = standardError.collected()
        return CommandResult(
            exitCode: exited ? (status >> 8) & 0xff : nil,
            standardOutput: String(decoding: out.data, as: UTF8.self),
            standardError: String(decoding: err.data, as: UTF8.self),
            timedOut: control.endedByThisApp,
            cancelled: control.wasCancelled,
            launchFailure: nil,
            standardOutputTruncated: out.truncated,
            standardErrorTruncated: err.truncated
        )
    }
}

/// Who may stop a running command, and when it is too late to.
///
/// The watchdog and a cancellation can both ask for the command to stop, from any thread, before or
/// after it has started. Every signal is sent under one lock and only while the process has not
/// been collected, which is what keeps a late SIGKILL off whatever process is given the id next.
private final class RunControl: @unchecked Sendable {
    private let lock = NSLock()
    private let killGrace: TimeInterval
    private var pid: pid_t = 0
    private var stopping = false
    private var signalled = false
    private var cancelled = false
    private var collected = false
    private var killTimer: DispatchWorkItem?

    init(killGrace: TimeInterval) { self.killGrace = killGrace }

    var isStopping: Bool { lock.withLock { stopping } }
    var endedByThisApp: Bool { lock.withLock { signalled } }
    var wasCancelled: Bool { lock.withLock { cancelled } }

    func started(_ pid: pid_t) {
        lock.withLock {
            self.pid = pid
            if stopping { terminate() }
        }
    }

    /// The watchdog's way in.
    func stop() {
        lock.withLock { stopLocked() }
    }

    /// The task that ran the command was cancelled. It is stopped the way the watchdog stops it,
    /// and remembered as cancelled, unless it had already finished on its own.
    func cancel() {
        lock.withLock {
            guard !collected else { return }
            cancelled = true
            stopLocked()
        }
    }

    /// Called with the lock held.
    private func stopLocked() {
        guard !stopping else { return }
        stopping = true
        if pid != 0, !collected { terminate() }
    }

    /// The process has exited and is about to be collected. When this app was stopping it, whatever
    /// is left in its group is killed now, while the exited process still holds the group's id: a
    /// helper that ignored the SIGTERM would otherwise outlive the command it belonged to. Nothing
    /// may be sent to the id after this.
    func exited() {
        lock.withLock {
            if signalled { kill(-pid, SIGKILL) }
            collected = true
            killTimer?.cancel()
            killTimer = nil
        }
    }

    /// Called with the lock held.
    private func terminate() {
        signalled = true
        kill(-pid, SIGTERM)
        let group = pid
        let item = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.lock.withLock {
                guard !self.collected else { return }
                kill(-group, SIGKILL)
            }
        }
        killTimer = item
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + killGrace, execute: item)
    }
}

/// Both ends of one pipe, marked so no other process this app starts can inherit them.
private struct Pipes {
    let read: Int32
    let write: Int32

    init?() {
        var fds: [Int32] = [0, 0]
        guard pipe(&fds) == 0 else { return nil }
        read = fds[0]
        write = fds[1]
        _ = fcntl(read, F_SETFD, FD_CLOEXEC)
        _ = fcntl(write, F_SETFD, FD_CLOEXEC)
    }

    func closeBoth() {
        close(read)
        close(write)
    }
}

/// Reads one output stream until it ends or is stopped, keeping at most `limit` bytes of it.
///
/// Past the limit the stream is still read and the rest thrown away: a command blocked on a full
/// pipe would never exit, and the watchdog would end up reporting a timeout for a command that had
/// simply said too much.
///
/// Each stream has a thread of its own that polls the pipe. Reading through DispatchIO left the
/// output to handlers on the global queues, which GCD runs only when it has a thread free for that
/// priority; on a small machine busy with other work that could be never within the drain deadline,
/// and a command that printed was reported as having printed nothing.
private final class StreamReader: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    private var truncated = false
    private var stopping = false
    private let limit: Int

    init(fileDescriptor: Int32, limit: Int, group: DispatchGroup) {
        self.limit = limit
        _ = fcntl(fileDescriptor, F_SETFL, fcntl(fileDescriptor, F_GETFL) | O_NONBLOCK)
        group.enter()
        Thread.detachNewThread { [self] in
            defer {
                close(fileDescriptor)
                group.leave()
            }
            var buffer = [UInt8](repeating: 0, count: 64 * 1024)
            var poller = pollfd(fd: fileDescriptor, events: Int16(POLLIN), revents: 0)
            while !lock.withLock({ stopping }) {
                let ready = poll(&poller, 1, 50)
                if ready < 0, errno != EINTR { return }
                if ready <= 0 { continue }
                if !drain(fileDescriptor, &buffer) { return }
            }
            // Asked to stop: whatever is already in the pipe is still the command's output, so it is
            // kept, and only what has not arrived yet is given up on.
            _ = drain(fileDescriptor, &buffer)
        }
    }

    /// Reads until the pipe has nothing more for now. False once the stream has ended.
    private func drain(_ fileDescriptor: Int32, _ buffer: inout [UInt8]) -> Bool {
        while true {
            let count = buffer.withUnsafeMutableBytes { read(fileDescriptor, $0.baseAddress, $0.count) }
            if count > 0 {
                keep(buffer, count: count)
                continue
            }
            if count == 0 { return false }
            if errno == EINTR { continue }
            return errno == EAGAIN
        }
    }

    private func keep(_ bytes: [UInt8], count: Int) {
        lock.withLock {
            let take = min(count, limit - data.count)
            if take < count { truncated = true }
            if take > 0 { data.append(contentsOf: bytes[0..<take]) }
        }
    }

    /// Gives up on what has not arrived yet; the thread closes the descriptor as it leaves.
    func stop() { lock.withLock { stopping = true } }

    func collected() -> (data: Data, truncated: Bool) {
        lock.withLock { (data, truncated) }
    }
}

/// Writes a command's standard input and then closes it, so the command sees the end of it.
private final class StreamWriter: @unchecked Sendable {
    private let lock = NSLock()
    private var stopping = false

    init(fileDescriptor: Int32, data: Data) {
        // A command that exits without reading its input leaves this writing into a broken pipe,
        // which is a normal end to the exchange rather than a failure to report. Asking for the
        // error instead of the signal keeps it that way: the default disposition of SIGPIPE would
        // take the whole app down with it.
        _ = fcntl(fileDescriptor, F_SETNOSIGPIPE, 1)
        _ = fcntl(fileDescriptor, F_SETFL, fcntl(fileDescriptor, F_GETFL) | O_NONBLOCK)
        // Its own thread for the same reason as the readers: a document larger than the pipe
        // buffer is only written while the command reads it, and that must not wait on GCD.
        Thread.detachNewThread { [self] in
            defer { close(fileDescriptor) }
            data.withUnsafeBytes { (payload: UnsafeRawBufferPointer) in
                guard let base = payload.baseAddress else { return }
                var offset = 0
                var poller = pollfd(fd: fileDescriptor, events: Int16(POLLOUT), revents: 0)
                while offset < payload.count, !lock.withLock({ stopping }) {
                    let ready = poll(&poller, 1, 50)
                    if ready < 0, errno != EINTR { return }
                    if ready <= 0 { continue }
                    let written = write(fileDescriptor, base + offset, payload.count - offset)
                    if written > 0 {
                        offset += written
                    } else if written < 0, errno != EAGAIN, errno != EINTR {
                        return
                    }
                }
            }
        }
    }

    /// The command is gone. Whatever it did not read is dropped rather than left waiting for a
    /// reader that will never come.
    func stop() { lock.withLock { stopping = true } }
}

/// The PATH the user's login shell sets up, which an app started from Finder or at login does not
/// have: launchd hands it only the bare system directories.
///
/// Read once per app run, in the background from the moment the app starts, and only by the app:
/// a test run keeps the environment it was started with and never reads anyone's shell profile.
/// A test that needs a lookup makes one of its own.
final class LoginShellPath: @unchecked Sendable {
    /// The one every command reads. Started at launch, and never in a test run.
    static let shared = LoginShellPath()

    private let lock = NSLock()
    /// Entered when the lookup starts and left when it ends. A group, because every command waits
    /// on it, often several at once: a DispatchWorkItem may be waited on only once, and the second
    /// command used to trap there and take the app down with it.
    private let lookup = DispatchGroup()
    private var started = false
    private var resolved: String?

    /// Starts reading it. Called once, at launch; a second call changes nothing.
    func resolveInBackground(shell: String = LoginShellPath.userShell()) {
        let first = lock.withLock {
            guard !started else { return false }
            started = true
            lookup.enter()
            return true
        }
        guard first else { return }
        // Its own thread for the same reason as every command: the lookup blocks while its output
        // is read on the global queues.
        Thread.detachNewThread {
            let path = Self.resolve(shell: shell)
            self.lock.withLock { self.resolved = path }
            self.lookup.leave()
        }
    }

    /// The login PATH, waiting for a read still in flight. Nil when none was started or the shell
    /// gave nothing usable, and then a command keeps the PATH this app was started with.
    func value() -> String? {
        lookup.wait()
        return lock.withLock { resolved }
    }

    /// The account's login shell, from the user database rather than from `SHELL`, which an app
    /// started by launchd may not have.
    static func userShell() -> String {
        if let entry = getpwuid(getuid()), let shell = entry.pointee.pw_shell {
            let path = String(cString: shell)
            if path.hasPrefix("/") { return path }
        }
        return "/bin/zsh"
    }

    static let marker = "__LEGION_CONTROL_PATH__"

    /// Asks a login shell for its PATH. `echo`, `;` and an absolute `printenv` mean the same thing
    /// to sh, bash, zsh, fish and tcsh, and the markers keep whatever a profile prints on the way
    /// in or out from being taken for the answer. The shell gets this app's own environment: the
    /// one every other command gets is the one being worked out.
    static func resolve(shell: String, timeout: TimeInterval = 10) -> String? {
        var environment = ProcessInfo.processInfo.environment
        environment["LEGION_NO_WAKE"] = "1"
        let result = Shell.runBlocking(
            executable: shell,
            arguments: ["-l", "-c", "echo \(marker); /usr/bin/printenv PATH; echo \(marker)"],
            timeout: timeout,
            input: nil,
            environment: environment,
            outputLimit: 64 * 1024,
            drainGrace: 2,
            control: RunControl(killGrace: 3)
        )
        guard result.launchFailure == nil, !result.timedOut else { return nil }
        return parse(result.standardOutput)
    }

    /// The line between the two markers, when it is a list of absolute directories.
    static func parse(_ output: String) -> String? {
        let lines = output.unicodeScalars.split(separator: "\n", omittingEmptySubsequences: false)
            .map { String(String.UnicodeScalarView($0)).trimmingCharacters(in: CharacterSet(charactersIn: "\r")) }
        guard let start = lines.firstIndex(of: marker), start + 2 < lines.count, lines[start + 2] == marker else {
            return nil
        }
        let path = lines[start + 1]
        guard !path.isEmpty,
              path.split(separator: ":", omittingEmptySubsequences: false).allSatisfy({ $0.hasPrefix("/") })
        else { return nil }
        return path
    }
}
