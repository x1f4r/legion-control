import Foundation

/// How far a command got before it went wrong.
///
/// This is the distinction the whole safety story rests on. A command that provably never reached
/// the far side can be retried on another route or another system without any risk at all. A command
/// that may have reached it can never be retried, because "reboot" and "update" are not questions
/// you get to ask twice, and a second one landing on a machine that already took the first is how a
/// user loses work.
enum Dispatch: Sendable, Equatable {
    /// The agent provably never ran: no route, refused, authentication rejected, host key refused,
    /// or the interpreter itself was missing.
    case never
    /// It may or may not have run. A timeout and a link that dropped mid-session both land here.
    case unknown
    /// The agent acknowledged in JSON and something went wrong afterwards.
    case acknowledged

    /// Whether it is safe to try the same mutation somewhere else.
    var isSafeToRetryMutation: Bool { self == .never }
}

/// What went wrong talking to an agent, told apart finely enough that the UI never has to guess.
///
/// The previous shape of this collapsed every ssh failure into "unreachable", which meant a command
/// that timed out, a key that was rejected and a host key that had changed all read as "the machine
/// is asleep" — and, worse, let a reboot that never happened be drawn as a reboot in progress.
struct AgentFailure: Error, Sendable, Equatable {
    enum Kind: Sendable, Equatable {
        /// The config names no way to reach this machine.
        case noRoute
        /// ssh itself could not be launched.
        case launchFailed
        /// The host is not answering: asleep, off, or off the network.
        case hostUnreachable
        /// Something answered on the port and refused the connection.
        case connectionRefused
        /// The key was rejected, or there was no key to offer.
        case authenticationFailed
        /// The host key does not match the one in known_hosts. On a dual boot machine this is
        /// usually the other system answering, not an attack, but it is never something to paper
        /// over.
        case hostKeyChanged
        /// The host is not in known_hosts at all and BatchMode cannot ask.
        case hostKeyUnknown
        /// Our own watchdog fired. The far side may well still be working.
        case timedOut(seconds: Int)
        /// The session dropped after it was established.
        case linkLost
        /// We got a shell and the interpreter named in the config is not there. Proof that the
        /// command shape is wrong for whatever is running.
        case interpreterMissing
        /// The interpreter ran and could not find the agent. Proof the shape was right and the
        /// agent really is not installed.
        case agentMissing
        /// Something came back that is not the one JSON object the agent promises.
        case unreadableOutput
        /// The agent replied cleanly and what it said was that it had failed.
        case agentFailed
    }

    var kind: Kind
    /// The configured system the attempt was aimed at, when it was aimed at one.
    var system: SystemConfig?
    /// The route it went over, for the diagnosis line.
    var route: String?
    var target: SSHTarget?
    /// Raw text worth showing under the sentence: ssh's own words, exit codes, stack frames.
    var detail: String
    var dispatch: Dispatch

    init(_ kind: Kind, system: SystemConfig? = nil, route: String? = nil, detail: String = "", dispatch: Dispatch) {
        self.kind = kind
        self.system = system
        self.route = route
        self.detail = detail
        self.dispatch = dispatch
    }

    /// One sentence naming what went wrong, in terms of the thing that has to change.
    func message(machine: String) -> String {
        switch kind {
        case .noRoute:
            return "There is no ssh host configured for \(machine)."
        case .launchFailed:
            return "ssh could not be run on this Mac."
        case .hostUnreachable:
            return "\(machine) did not answer. It is asleep, off, or off the network."
        case .connectionRefused:
            return "\(machine) refused the connection. Something is answering, but not ssh."
        case .authenticationFailed:
            return "\(machine) rejected the key. Nothing was run there."
        case .hostKeyChanged:
            return "The host key for \(machine) has changed, so ssh refused to connect and nothing was run. On a dual boot machine this is usually the other system answering."
        case .hostKeyUnknown:
            return "\(machine) is not in known_hosts, so ssh refused to connect without being able to ask. Connect once from a terminal to accept it."
        case .timedOut(let seconds):
            return "\(machine) did not answer within \(seconds) seconds."
        case .linkLost:
            return "The connection to \(machine) dropped."
        case .interpreterMissing:
            return "The interpreter the config names for \(system?.name ?? machine) is not on that system."
        case .agentMissing:
            return "The control agent is not installed on \(system?.name ?? machine). Run the installer on that system."
        case .unreadableOutput:
            return "The agent replied with something unreadable."
        case .agentFailed:
            return "The control agent reported a problem. \(detail)"
        }
    }

    var detailText: String? { detail.isEmpty ? nil : detail }

    /// Whether this reads as "the machine is simply not up", which is the only failure the app is
    /// allowed to draw quietly. Everything else is news.
    var meansAsleepOrOff: Bool {
        switch kind {
        case .hostUnreachable, .noRoute: true
        default: false
        }
    }

    /// A short phrase for the operation history, where the sentence above is too long.
    var shortReason: String {
        switch kind {
        case .noRoute: "no route configured"
        case .launchFailed: "ssh could not be run"
        case .hostUnreachable: "no answer"
        case .connectionRefused: "connection refused"
        case .authenticationFailed: "key rejected"
        case .hostKeyChanged: "host key changed"
        case .hostKeyUnknown: "host key not known"
        case .timedOut(let seconds): "timed out after \(seconds)s"
        case .linkLost: "connection dropped"
        case .interpreterMissing: "interpreter missing"
        case .agentMissing: "agent not installed"
        case .unreadableOutput: "unreadable reply"
        case .agentFailed: "agent reported a failure"
        }
    }
}

/// Reads ssh's own diagnostics.
///
/// ssh reserves exit status 255 for its own failures and puts the reason in one line of stderr. That
/// line is the only evidence there is for whether anything ran on the far side, so it is worth
/// reading properly rather than folding into a single "unreachable".
enum SSHDiagnosis {
    /// What the text says, or nil when it says nothing recognisable.
    static func classify(_ text: String) -> (kind: AgentFailure.Kind, dispatch: Dispatch)? {
        let lower = text.lowercased()

        // Host key first: it is the one failure that looks alarming and has a mundane cause on a
        // dual boot machine, and its text also contains "permission denied" in some versions.
        if lower.contains("remote host identification has changed")
            || lower.contains("host key verification failed")
            || lower.contains("key_verify failed") {
            return (.hostKeyChanged, .never)
        }
        if lower.contains("no matching host key type")
            || lower.contains("host key for") && lower.contains("has changed") {
            return (.hostKeyChanged, .never)
        }
        if lower.contains("no rsa host key is known")
            || lower.contains("host key is not known")
            || lower.contains("no ed25519 host key is known") {
            return (.hostKeyUnknown, .never)
        }

        if lower.contains("permission denied")
            || lower.contains("too many authentication failures")
            || lower.contains("no supported authentication methods")
            || lower.contains("authentication failed") {
            return (.authenticationFailed, .never)
        }

        if lower.contains("connection refused") {
            return (.connectionRefused, .never)
        }
        if lower.contains("no route to host")
            || lower.contains("network is unreachable")
            || lower.contains("could not resolve hostname")
            || lower.contains("name or service not known")
            || lower.contains("nodename nor servname provided")
            || lower.contains("operation timed out")
            || lower.contains("connection timed out")
            || lower.contains("host is down") {
            return (.hostUnreachable, .never)
        }

        // A session that was up and went away. This one is genuinely ambiguous: it is what a reboot
        // looks like from here, and it is also what a flaky link looks like.
        if lower.contains("connection closed by remote host")
            || lower.contains("connection reset by peer")
            || lower.contains("client_loop: send disconnect")
            || lower.contains("broken pipe")
            || lower.contains("connection to") && lower.contains("closed by remote host") {
            return (.linkLost, .unknown)
        }

        return nil
    }

    /// The markers that prove the interpreter itself was never found, which is the one remote
    /// failure that is safe to treat as "nothing ran".
    static func interpreterIsMissing(_ text: String) -> Bool {
        let lower = text.lowercased()
        let markers = [
            "command not found", "unknown command", "not recognized as the name",
            "is not recognized as an internal", "commandnotfoundexception",
            "no such file or directory", "cannot find path"
        ]
        return markers.contains { lower.contains($0) }
    }

    /// The markers that prove the interpreter ran and the agent script was not where the config
    /// said it would be.
    static func agentIsMissing(_ text: String) -> Bool {
        let lower = text.lowercased()
        let markers = ["cannot find module", "module_not_found", "err_module_not_found"]
        return markers.contains { lower.contains($0) }
    }
}
