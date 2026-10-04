import Foundation
@testable import LegionControlCore

/// What a fake ssh does that a test has to do by hand: ssh writes its own errors to the file `-E`
/// names, and with LogLevel=ERROR nowhere else, so a fake that means "the connection was refused"
/// has to say so there. Saying it on stderr is what a remote program would do, and the transport
/// rightly ignores that.
enum FakeSSH {
    /// Writes ssh's own words where this attempt's log is.
    static func writeLog(_ text: String, for arguments: [String]) {
        guard let index = arguments.firstIndex(of: "-E"), index + 1 < arguments.count else { return }
        try? Data(text.utf8).write(to: URL(fileURLWithPath: arguments[index + 1]))
    }

    /// The command line an attempt sent, which ssh is always given last.
    static func line(_ arguments: [String]) -> String { arguments.last ?? "" }

    /// The destination it was sent to, which always follows `--`.
    static func destination(_ arguments: [String]) -> String? {
        arguments.firstIndex(of: "--").map { arguments[$0 + 1] }
    }

    /// A directory of its own for one test's ssh logs, so no test writes into the real application
    /// support folder.
    static func logDirectory() -> URL {
        FileManager.default.temporaryDirectory.appending(path: "legion-ssh-logs-\(UUID().uuidString)")
    }

    static func result(exit: Int32? = 0, out: String = "", err: String = "",
                       timedOut: Bool = false, launchFailure: String? = nil) -> CommandResult {
        CommandResult(exitCode: exit, standardOutput: out, standardError: err,
                      timedOut: timedOut, launchFailure: launchFailure)
    }
}
