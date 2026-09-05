import Foundation

/// One request to an agent, built once and used by both transports.
///
/// The local Mac and the machines over ssh run exactly the same agent and take exactly the same
/// arguments, so the argv is built in one place and neither transport gets to have its own idea of
/// what a command looks like. This is also the file the tests assert against: what goes over the
/// wire is a pure function of the request, and can be checked without a machine.
struct AgentRequest: Sendable, Equatable {
    var arguments: [String]
    var timeout: TimeInterval
    var safety: CommandSafety
    /// The bytes to send on stdin, for the two commands that hand over a document.
    var input: Data?

    /// A refusal built here rather than sent. An id that does not match the contract's grammar is a
    /// sign of a mangled config, and it is caught before it can reach a shell.
    struct Invalid: Error, Sendable, Equatable {
        var message: String
    }

    private init(_ arguments: [String], timeout: TimeInterval, safety: CommandSafety, input: Data? = nil) {
        self.arguments = arguments
        self.timeout = timeout
        self.safety = safety
        self.input = input
    }

    private static func checked(_ token: String, what: String) throws -> String {
        guard AgentToken.isValidID(token) else {
            throw Invalid(message: "\(what) \"\(token)\" is invalid. Use letters, digits, dots, underscores or hyphens, starting with a letter or digit.")
        }
        return token
    }

    private static func checkedOperation(_ id: String) throws -> String {
        guard AgentToken.isValidOperationId(id) else {
            throw Invalid(message: "\"\(id)\" is not a valid operation id.")
        }
        return id
    }

    // MARK: - Reads

    /// The status snapshot. The budget is the agent's own, and it is deliberately smaller than the
    /// client timeout: the agent answering late with everything is worse than answering on time with
    /// what it had and saying the rest is not known.
    static func status(dialect: AgentDialect, budgetMs: Int = 20_000) -> AgentRequest {
        var arguments = ["status"]
        if dialect.supportsBudget { arguments += ["--budget-ms", String(budgetMs)] }
        return AgentRequest(arguments, timeout: 30, safety: .readOnly)
    }

    static func doctor(deep: Bool) -> AgentRequest {
        AgentRequest(deep ? ["doctor", "--deep"] : ["doctor"], timeout: deep ? 120 : 60, safety: .readOnly)
    }

    static func bundle() -> AgentRequest {
        AgentRequest(["bundle"], timeout: 90, safety: .readOnly)
    }

    static func history(limit: Int) -> AgentRequest {
        AgentRequest(["history", "--limit", String(min(200, max(1, limit)))], timeout: 40, safety: .readOnly)
    }

    static func logs(lines: Int, operation: String?) throws -> AgentRequest {
        var arguments = ["logs", "--lines", String(min(500, max(1, lines)))]
        if let operation { arguments += ["--op", try checkedOperation(operation)] }
        return AgentRequest(arguments, timeout: 60, safety: .readOnly)
    }

    /// Ask what became of an operation. `--wait` long-polls on the far side, which turns "poll every
    /// two seconds for ten minutes" into one call that returns the moment the work finishes.
    static func operation(id: String, wait: Int?) throws -> AgentRequest {
        var arguments = ["op", try checkedOperation(id)]
        if let wait, wait > 0 { arguments += ["--wait", String(wait)] }
        // The command budget is deliberately larger than the long poll, so the agent's own deadline
        // is what ends the call and a timeout here really does mean the link is the problem.
        return AgentRequest(arguments, timeout: TimeInterval((wait ?? 0) + 20), safety: .readOnly)
    }

    static func policy(service: String?) throws -> AgentRequest {
        var arguments = ["policy"]
        if let service { arguments += ["--service", try checked(service, what: "The service id")] }
        return AgentRequest(arguments, timeout: 30, safety: .readOnly)
    }

    static func configRead() -> AgentRequest {
        AgentRequest(["config"], timeout: 40, safety: .readOnly)
    }

    static func configMeta() -> AgentRequest {
        AgentRequest(["config", "meta"], timeout: 30, safety: .readOnly)
    }

    // MARK: - Mutations

    /// Everything the three "how" flags mean, in one place.
    ///
    /// This is finding 06 written as a type. `force` skips the busy check and nothing else; the
    /// schedule policy is ignored by every manual request without any flag at all; and `whenIdle`
    /// is the third answer that used to be missing entirely — neither interrupt the work nor give up.
    struct Intent: Sendable, Equatable {
        /// Skip the busy gate. Never sent on a first attempt; only ever after the agent has looked
        /// at the machine as it is now and said no.
        var force: Bool = false
        /// Hold the request until the machine is idle, expiring if that never happens.
        var whenIdle: Bool = false
        /// How long a queued request stays alive.
        var expires: TimeInterval = 4 * 60 * 60
        /// Run it in the background over there and answer straight away. Long operations must use
        /// this: an update can run for a quarter of an hour, and a client that holds an ssh session
        /// open for that long is a client that loses the answer to a dropped link.
        var detach: Bool = false
        /// The durable id. The same id is reused only to retry the identical request; a different
        /// intent always gets a new one, because the agent binds an id to its intent and would
        /// answer a changed one with a conflict rather than a replay.
        var operationId: String?

        static let manual = Intent()
    }

    private static func intentArguments(_ intent: Intent, dialect: AgentDialect) throws -> [String] {
        var arguments: [String] = []
        if intent.force { arguments.append("--force") }
        if dialect.supportsOperations, let id = intent.operationId {
            arguments += ["--op", try checkedOperation(id)]
        }
        if dialect.supportsQueue, intent.whenIdle {
            arguments.append("--when-idle")
            arguments += ["--expires", AgentToken.duration(seconds: intent.expires)]
        }
        if dialect.supportsDetach, intent.detach, !intent.whenIdle {
            arguments.append("--detach")
        }
        return arguments
    }

    static func update(service: String?, intent: Intent, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["update"]
        if let service { arguments += ["--service", try checked(service, what: "The service id")] }
        arguments += try intentArguments(intent, dialect: dialect)
        // A detached update answers immediately; a synchronous one against a 2.x agent can genuinely
        // take a quarter of an hour, and cutting it off early is what produced unknown outcomes.
        let timeout: TimeInterval = (intent.detach && dialect.supportsDetach) || intent.whenIdle ? 60 : 900
        return AgentRequest(arguments, timeout: timeout, safety: .mutation)
    }

    static func restart(service: String?, intent: Intent, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["restart"]
        if let service { arguments += ["--service", try checked(service, what: "The service id")] }
        arguments += try intentArguments(intent, dialect: dialect)
        let timeout: TimeInterval = (intent.detach && dialect.supportsDetach) || intent.whenIdle ? 60 : 240
        return AgentRequest(arguments, timeout: timeout, safety: .mutation)
    }

    static func boot(target: String, intent: Intent, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["boot", try checked(target, what: "The boot target")]
        arguments += try intentArguments(intent, dialect: dialect)
        // Short on purpose: the reboot cuts the connection while ssh is still waiting. Nothing above
        // this line reads a cut connection as success — only the JSON the agent printed first does.
        return AgentRequest(arguments, timeout: 60, safety: .mutation)
    }

    static func sleep(intent: Intent, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["sleep"]
        arguments += try intentArguments(intent, dialect: dialect)
        return AgentRequest(arguments, timeout: 60, safety: .mutation)
    }

    static func run(action: String, intent: Intent, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["run", try checked(action, what: "The action id")]
        arguments += try intentArguments(intent, dialect: dialect)
        let timeout: TimeInterval = (intent.detach && dialect.supportsDetach) || intent.whenIdle ? 60 : 300
        return AgentRequest(arguments, timeout: timeout, safety: .mutation)
    }

    /// The maintenance cycle across every eligible service. The same command the schedulers run, so
    /// asking for it by hand and letting the timer do it take exactly the same path.
    static func cycle(intent: Intent, dryRun: Bool, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["cycle"]
        if dryRun { arguments.append("--dry-run") }
        if dialect.supportsOperations, let id = intent.operationId, !dryRun {
            arguments += ["--op", try checkedOperation(id)]
        }
        return AgentRequest(arguments, timeout: dryRun ? 60 : 1800, safety: dryRun ? .readOnly : .mutation)
    }

    static func cancel(id: String) throws -> AgentRequest {
        AgentRequest(["cancel", try checkedOperation(id)], timeout: 40, safety: .mutation)
    }

    /// The simple switch. `policy set` can do the same thing, but this is the command a 2.x agent
    /// also understands, so it is what the plain on/off toggle sends.
    static func autoUpdate(_ enabled: Bool, service: String?, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["auto-update", enabled ? "on" : "off"]
        if let service, dialect.supportsServicePolicy {
            arguments += ["--service", try checked(service, what: "The service id")]
        }
        return AgentRequest(arguments, timeout: 40, safety: .mutation)
    }

    /// The full policy patch: automatic, pauseUntil and maintenanceWindows, as JSON on stdin.
    static func policySet(service: String?, patch: Data) throws -> AgentRequest {
        var arguments = ["policy", "set"]
        if let service { arguments += ["--service", try checked(service, what: "The service id")] }
        return AgentRequest(arguments, timeout: 40, safety: .mutation, input: patch)
    }

    /// Hand the machine the controller document.
    ///
    /// The canonical bytes go in on stdin and the identity goes in as flags, because the agent
    /// refuses a document that would move a revision backwards and cannot do that without being
    /// told which revision this is. `--replace` is the explicit "yes, this really is a different
    /// setup and I mean to overwrite it", and is only ever sent after the user has said so.
    static func configSet(_ document: ControllerDocument, replace: Bool, dialect: AgentDialect) throws -> AgentRequest {
        var arguments = ["config", "set"]
        if dialect.supportsControllerIdentity, let id = document.identity.id {
            guard AgentToken.isValidSetupID(id) else { throw Invalid(message: "The setup id is invalid.") }
            arguments += ["--controller-id", id]
            arguments += ["--revision", String(document.identity.revisionNumber)]
        }
        if replace { arguments.append("--replace") }
        return AgentRequest(arguments, timeout: 40, safety: .mutation, input: document.bytes)
    }

    /// Send the signed agent tarball in on stdin and let the far side verify and install it.
    static func selfUpdateFromStdin(operation: String) throws -> AgentRequest {
        AgentRequest(["self-update", "--stdin", "--op", try checkedOperation(operation)], timeout: 300, safety: .mutation)
    }

    static func selfUpdate(from path: String) throws -> AgentRequest {
        guard !path.isEmpty, !path.contains("\0"), !path.hasPrefix("-") else { throw Invalid(message: "The staged path is invalid.") }
        return AgentRequest(["self-update", "--from", path],
                     timeout: 300, safety: .mutation)
    }

    /// Used by the bootstrap over a 2.x agent, which has no `self-update` of its own: the freshly
    /// unpacked tree installs itself.
    static func selfUpdateInstall() -> AgentRequest {
        AgentRequest(["self-update", "--install"], timeout: 300, safety: .mutation)
    }

    static func version() -> AgentRequest {
        AgentRequest(["version"], timeout: 30, safety: .readOnly)
    }

    static func serviceConfig(_ verb: String, input: Data? = nil) throws -> AgentRequest {
        guard ["get", "validate", "set"].contains(verb), (verb == "get") == (input == nil),
              input.map({ $0.count <= 1_048_576 }) ?? true else {
            throw Invalid(message: "Invalid or oversized service configuration request.")
        }
        return AgentRequest(["service-config", verb] + (verb == "get" ? [] : ["--stdin"]),
                            timeout: 40, safety: verb == "set" ? .mutation : .readOnly, input: input)
    }

    /// Anything else, for the diagnostics page. Every token is checked.
    static func raw(_ arguments: [String], timeout: TimeInterval, safety: CommandSafety) throws -> AgentRequest {
        for token in arguments where !AgentToken.isValid(token) { throw Invalid(message: "Invalid raw argument: \(token)") }
        return AgentRequest(arguments, timeout: timeout, safety: safety)
    }

    /// A copy of this request with a different stdin, for the two commands that stream a file.
    func sending(_ input: Data) -> AgentRequest {
        AgentRequest(arguments, timeout: timeout, safety: safety, input: input)
    }
}
