import Foundation

struct CommandResult: Sendable {
    var exitCode: Int32
    var standardOutput: String
    var standardError: String
    var timedOut: Bool
    var launchFailure: String?

    var succeeded: Bool { exitCode == 0 && !timedOut && launchFailure == nil }

    /// The most useful line to show a human when something went wrong. Only a few lines of this ever
    /// reach the screen, and both ssh and node bury the one sentence that matters under a routing
    /// banner, stack frames and caret art, so the lines that carry no message are dropped first.
    var failureText: String {
        if let launchFailure { return launchFailure }
        if timedOut { return "The command did not finish in time." }
        let fromError = Self.informativeLines(standardError)
        if !fromError.isEmpty { return fromError }
        let fromOutput = Self.informativeLines(standardOutput)
        if !fromOutput.isEmpty { return fromOutput }
        return "Exit code \(exitCode)."
    }

    private static func informativeLines(_ text: String) -> String {
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

/// Small wrapper around Process. Everything runs on a background queue so the UI never blocks, and
/// both output streams are drained on their own queues so a chatty command cannot deadlock the pipe.
enum Shell {
    /// `input` is the standard input to hand the command, for the one thing that sends a document
    /// rather than asking for one. Without it the command gets /dev/null, which is what everything
    /// else here wants: nothing should ever sit waiting on a terminal that is not there.
    static func run(
        executable: String,
        arguments: [String],
        timeout: TimeInterval,
        input: Data? = nil
    ) async -> CommandResult {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: runBlocking(
                    executable: executable,
                    arguments: arguments,
                    timeout: timeout,
                    input: input
                ))
            }
        }
    }

    private static func runBlocking(
        executable: String,
        arguments: [String],
        timeout: TimeInterval,
        input: Data?
    ) -> CommandResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments

        let inPipe = input.map { _ in Pipe() }
        process.standardInput = inPipe ?? FileHandle.nullDevice

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
        var environment = ProcessInfo.processInfo.environment
        environment["LEGION_NO_WAKE"] = "1"
        process.environment = environment

        let outPipe = Pipe()
        let errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = errPipe

        do {
            try process.run()
        } catch {
            return CommandResult(
                exitCode: -1,
                standardOutput: "",
                standardError: "",
                timedOut: false,
                launchFailure: "Could not run \(executable): \(error.localizedDescription)"
            )
        }

        // Written from its own queue, and never from this one. A document larger than the pipe buffer
        // only goes through as fast as the command reads it, and writing it here would mean nobody is
        // draining the output pipes in the meantime: the two halves would wait on each other forever.
        if let inPipe, let input {
            DispatchQueue.global(qos: .userInitiated).async {
                let handle = inPipe.fileHandleForWriting
                // A command that exits without reading its input leaves this writing into a broken
                // pipe, which is a normal end to the exchange rather than a failure to report. Asking
                // for the error instead of the signal keeps it that way: the default disposition of
                // SIGPIPE would take the whole app down with it.
                _ = fcntl(handle.fileDescriptor, F_SETNOSIGPIPE, 1)
                try? handle.write(contentsOf: input)
                try? handle.close()
            }
        }

        // Read in chunks rather than waiting for end of file, so whatever the command wrote is already
        // in hand even if the pipe never closes. See the drain deadline below for why that happens.
        let outBox = DataBox()
        let errBox = DataBox()
        let group = DispatchGroup()
        group.enter()
        DispatchQueue.global(qos: .userInitiated).async {
            drain(outPipe.fileHandleForReading, into: outBox)
            group.leave()
        }
        group.enter()
        DispatchQueue.global(qos: .userInitiated).async {
            drain(errPipe.fileHandleForReading, into: errBox)
            group.leave()
        }

        // Only the pid is captured by the watchdog: Process itself is not safe to touch from another queue.
        let pid = process.processIdentifier
        let timedOut = FlagBox()
        let watchdog = DispatchWorkItem {
            timedOut.set(true)
            kill(pid, SIGTERM)
            DispatchQueue.global().asyncAfter(deadline: .now() + 3) { kill(pid, SIGKILL) }
        }
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + timeout, execute: watchdog)

        process.waitUntilExit()
        watchdog.cancel()

        // The pipe stays open as long as anything still holds the far end, and ssh hands its own stderr
        // to the ProxyCommand, so a helper that outlives ssh would keep an unbounded wait here alive and
        // the timeout above would mean nothing. The command itself is already gone by this point and its
        // output has been drained, so give the readers a moment to settle and then take what they have.
        _ = group.wait(timeout: .now() + 2)

        return CommandResult(
            exitCode: process.terminationStatus,
            standardOutput: String(decoding: outBox.get(), as: UTF8.self),
            standardError: String(decoding: errBox.get(), as: UTF8.self),
            timedOut: timedOut.get(),
            launchFailure: nil
        )
    }
}

/// Reads a pipe to end of file, handing over each chunk as it arrives.
private func drain(_ handle: FileHandle, into box: DataBox) {
    while true {
        let chunk = handle.availableData
        if chunk.isEmpty { return }
        box.append(chunk)
    }
}

private final class DataBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value = Data()

    func append(_ chunk: Data) {
        lock.lock()
        value.append(chunk)
        lock.unlock()
    }

    func get() -> Data {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

private final class FlagBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false

    func set(_ newValue: Bool) {
        lock.lock()
        value = newValue
        lock.unlock()
    }

    func get() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}
