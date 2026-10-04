import Foundation

/// Everything one attempt left behind, which is everything section 15.4 of the contract may read.
struct AttemptEvidence: Sendable, Equatable {
    /// ssh, or the local interpreter, could not be started at all.
    var launchFailed: Bool
    /// This app ended the process: its watchdog fired or the request was cancelled.
    var timedOut: Bool
    /// Nil when the process ended without an exit status.
    var exitCode: Int32?
    var stdout: String
    var stderr: String
    /// What ssh itself wrote with `-E`. Empty when there is none, as for the local agent.
    var sshLog: String
    /// False when stdout was cut off at the cap. A reply that did not arrive whole is not a reply.
    var stdoutComplete = true

    init(launchFailed: Bool, timedOut: Bool, exitCode: Int32?, stdout: String, stderr: String, sshLog: String,
         stdoutComplete: Bool = true) {
        self.launchFailed = launchFailed
        self.timedOut = timedOut
        self.exitCode = exitCode
        self.stdout = stdout
        self.stderr = stderr
        self.sshLog = sshLog
        self.stdoutComplete = stdoutComplete
    }

    /// What a finished process says.
    init(_ result: CommandResult, sshLog: String) {
        self.init(launchFailed: result.launchFailure != nil, timedOut: result.timedOut, exitCode: result.exitCode,
                  stdout: result.standardOutput, stderr: result.standardError, sshLog: sshLog,
                  stdoutComplete: !result.standardOutputTruncated)
    }
}

/// What one attempt says: a reply, or a failure with how far the command may have got.
///
/// The port of `classify` in `contract/tools/transport.mjs`, held to it by the shared transport
/// vectors. Before it the Mac read a bare exit 255 as "never sent" and tried the same mutation on
/// the next route, so a restart whose link dropped after it had started could be run twice.
enum AttemptClassifier {
    struct Verdict: Sendable, Equatable {
        /// Nil for a reply.
        var kind: AgentFailure.Kind?
        var dispatch: Dispatch
        /// Whether the whole route is now known to be no good, so its other systems are skipped.
        var routeLevel: Bool

        static let reply = Verdict(kind: nil, dispatch: .acknowledged, routeLevel: false)

        static func failure(_ kind: AgentFailure.Kind, _ dispatch: Dispatch) -> Verdict {
            Verdict(kind: kind, dispatch: dispatch, routeLevel: kind.isRouteLevel)
        }

        var isReply: Bool { kind == nil }
    }

    /// The rows of section 15.4, in order; the order is the rule.
    ///
    /// A reply wins over everything, even a timeout, because it is the agent's own account of what
    /// happened. `never` is only ever claimed on evidence that the command cannot have run: ssh could
    /// not start, ssh's own log names a failure before the channel opened, or a shell exited 126, 127
    /// or 9009 with nothing on stdout, which only a shell that could not start the interpreter does
    /// (the agent itself exits 0 or 1). Every other ending is `unknown`, and a mutation that ended
    /// `unknown` is never sent again.
    ///
    /// `timeout` only names the watchdog in a `timedOut` kind for the sentence shown to a person;
    /// it decides nothing.
    static func classify(_ evidence: AttemptEvidence, timeout: Int = 0) -> Verdict {
        if evidence.launchFailed { return .failure(.launchFailed, .never) }
        if evidence.stdoutComplete, jsonObjectSlice(evidence.stdout) != nil { return .reply }
        if evidence.timedOut { return .failure(.timedOut(seconds: timeout), .unknown) }
        guard let exitCode = evidence.exitCode else { return .failure(.linkLost, .unknown) }
        let blank = isBlank(evidence.stdout)
        if exitCode == 255 {
            if blank, let kind = preDispatchKind(evidence.sshLog) { return .failure(kind, .never) }
            // An exit 255 nothing explains: the far side may have run the command and the link
            // dropped afterwards.
            return .failure(.linkLost, .unknown)
        }
        if blank, [126, 127, 9009].contains(exitCode) { return .failure(.interpreterMissing, .never) }
        // From here on stderr is read, but only to name the failure for a person. It never makes an
        // attempt `never`: a program that printed "Cannot find module" may have done half of its
        // work first.
        let stderr = asciiLowercased(evidence.stderr)
        if stderr.contains("cannot find module") || stderr.contains("err_module_not_found") {
            return .failure(.agentMissing, .unknown)
        }
        if ["is not recognized as", "commandnotfoundexception", "cannot find the path specified"].contains(where: { stderr.contains($0) }) {
            return .failure(.interpreterMissing, .unknown)
        }
        return .failure(.unreadableOutput, .unknown)
    }

    /// The reply in stdout: from its first `{` to its last `}`, when that parses as a JSON object.
    /// Windows PowerShell can put a `#< CLIXML` banner in front of anything a native program prints,
    /// and a login script can print a line or two; the slice is what survives both.
    static func jsonObjectSlice(_ stdout: String) -> Data? {
        let bytes = Array(stdout.utf8)
        guard let start = bytes.firstIndex(of: UInt8(ascii: "{")), let end = bytes.lastIndex(of: UInt8(ascii: "}")),
              start < end else { return nil }
        let slice = Data(bytes[start...end])
        guard (try? JSONSerialization.jsonObject(with: slice)) is [String: Any] else { return nil }
        return slice
    }

    /// Blank means only tab, line feed, carriage return and space: not whatever a language's own
    /// trim takes away.
    static func isBlank(_ text: String) -> Bool {
        text.unicodeScalars.allSatisfy { ["\t", "\n", "\r", " "].contains($0) }
    }

    // MARK: - ssh's own log

    /// The pre-dispatch table: the log lines that prove the command never left this device. The
    /// first row with any matching line decides.
    ///
    /// Only ssh's log is read here. The agent writes raw text to stderr and the launcher passes it
    /// through, so a remote program can print any ssh-looking line it likes; reading remote stderr
    /// as evidence is how a mutation that ran gets sent a second time.
    ///
    /// The patterns are the contract's, written in lower case and matched against a line with only
    /// its ASCII letters lowered, which is exactly the contract's ASCII case-insensitivity. `\z`
    /// stands for its `$`, because an ICU `$` also matches before a final line terminator, and
    /// `[^\r  ]` for its `.`, because ICU's dot also refuses a few characters that
    /// JavaScript's takes.
    private static let preDispatch: [(AgentFailure.Kind, [NSRegularExpression])] = {
        let any = "[^\\r\\u2028\\u2029]"
        func patterns(_ sources: [String]) -> [NSRegularExpression] {
            sources.map { try! NSRegularExpression(pattern: $0.replacingOccurrences(of: "(any)", with: any)) }
        }
        return [
            (.hostKeyUnknown, patterns([
                "^no [^ ]+ host key is known for (any)+ and you have requested strict checking\\.\\z",
            ])),
            (.hostKeyChanged, patterns([
                "remote host identification has changed",
                "^host key for (any)+ has changed and you have requested strict checking\\.\\z",
                "^host key verification failed\\.\\z",
            ])),
            (.authenticationFailed, patterns([
                "^(?:(any)+: )?permission denied \\([^()]+\\)\\.?\\z",
                ": too many authentication failures\\z",
                "^no more authentication methods to try\\.\\z",
            ])),
            (.connectionRefused, patterns([
                "^ssh(?:\\.exe)?: connect to host (any)+ port [0-9]+: connection refused\\z",
                "^(?:kex|ssh)_exchange_identification: ",
                "^banner exchange: ",
                "^unable to negotiate with (any)+: no matching (any)+\\z",
                "^channel 0: open failed: ",
                "^exec request failed on channel 0\\z",
            ])),
            (.hostUnreachable, patterns([
                "^ssh(?:\\.exe)?: connect to host (any)+ port [0-9]+: (any)+\\z",
                "^ssh(?:\\.exe)?: could not resolve hostname (any)+: (any)+\\z",
                "^connection timed out during banner exchange\\z",
            ])),
            (.launchFailed, patterns([
                "^bad owner or permissions on (any)+\\z",
                ": terminating, [0-9]+ bad configuration options?\\z",
                "^command-line line [0-9]+: (any)+\\z",
                "^can't open user config file (any)+\\z",
                "^cannot execute command-line and remote command\\.\\z",
            ])),
        ]
    }()

    /// The kind ssh's log proves, when a line in it matches the pre-dispatch table.
    static func preDispatchKind(_ log: String) -> AgentFailure.Kind? {
        // Split on the scalar, not the Character: to Swift a CR LF pair is one Character, so a
        // Windows log split on "\n" would come back as a single line.
        let lines = log.unicodeScalars.split(separator: "\n", omittingEmptySubsequences: false).map { line in
            asciiLowercased(String(String.UnicodeScalarView(line.last == "\r" ? line.dropLast() : line[...])))
        }
        for (kind, patterns) in preDispatch {
            let matched = lines.contains { line in
                let range = NSRange(line.startIndex..., in: line)
                return patterns.contains { $0.firstMatch(in: line, range: range) != nil }
            }
            if matched { return kind }
        }
        return nil
    }

    private static func asciiLowercased(_ text: String) -> String {
        var scalars = String.UnicodeScalarView()
        for scalar in text.unicodeScalars {
            scalars.append(("A"..."Z").contains(scalar) ? Unicode.Scalar(scalar.value + 32)! : scalar)
        }
        return String(scalars)
    }
}

/// Which attempts a command is sent on (section 15.5 of the contract).
///
/// A read goes on after any failure. A mutation goes on only after a failure that is `never`;
/// anything else may have run, so it stops at the first such attempt and is reconciled with
/// `op <id>`, never resent. A failure that is about the whole route skips the rest of that route's
/// systems, because they share its connection. `RemoteAgent` takes each step from `step`, and the
/// shared walk vectors run the same function through `simulate`.
enum AttemptWalk {
    enum Step: Sendable, Equatable {
        /// The agent answered; the walk is over.
        case reply
        /// A mutation that may have run. Nothing more is sent.
        case stop
        /// Go on to the next attempt, skipping the rest of this route when it is known to be dead.
        case next(routeIsDead: Bool)
    }

    enum Ending: String, Sendable, Equatable {
        case reply, stopped, exhausted
    }

    static func step(after verdict: AttemptClassifier.Verdict, safety: CommandSafety) -> Step {
        if verdict.isReply { return .reply }
        if safety == .mutation, !verdict.dispatch.isSafeToRetryMutation { return .stop }
        return .next(routeIsDead: verdict.routeLevel)
    }

    /// The attempts a walk makes, by index, given what each one would end in, and why it stopped.
    static func simulate(safety: CommandSafety,
                         results: [(route: String, verdict: AttemptClassifier.Verdict)]) -> (sent: [Int], ending: Ending) {
        var sent: [Int] = []
        var dead = Set<String>()
        for (index, attempt) in results.enumerated() where !dead.contains(attempt.route) {
            sent.append(index)
            switch step(after: attempt.verdict, safety: safety) {
            case .reply: return (sent, .reply)
            case .stop: return (sent, .stopped)
            case .next(let routeIsDead): if routeIsDead { dead.insert(attempt.route) }
            }
        }
        return (sent, .exhausted)
    }
}

/// The `--budget-ms` a status attempt sends (section 15.5, step 3).
///
/// Five seconds stay in hand for ssh and the reply, the agent never gets more than twenty, and never
/// less than one: a first attempt always goes out, a later one only when there is time to use its
/// answer. Before this every attempt sent 20000 whatever was left of the 30 s, so after two dead
/// routes the working one was cut off by this app's own deadline before the agent's ran out.
enum StatusBudget {
    /// The budget for the next attempt in whole milliseconds, or nil when no attempt should be made.
    static func next(remainingMs: Int, attemptsMade: Int) -> Int? {
        let budget = min(20_000, remainingMs - 5_000)
        if budget >= 1_000 { return budget }
        return attemptsMade == 0 ? 1_000 : nil
    }
}
