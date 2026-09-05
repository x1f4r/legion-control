import Foundation

/// Which version of the agent contract the far side speaks, and what that allows the app to send.
///
/// Sending a flag an older agent has never heard of is not a harmless mistake: the agent refuses the
/// whole command, so a user with one out-of-date machine would lose the ability to update it at all.
/// Every v3-only flag and command therefore asks here first, and the answer comes from the agent's
/// own `contract` field rather than from a version number this app would have to keep a table of.
struct AgentDialect: Sendable, Equatable {
    var contract: Int

    /// What is assumed before anything has answered, and what a 2.x agent gets.
    static let legacy = AgentDialect(contract: 2)
    static let v3 = AgentDialect(contract: 3)

    init(contract: Int) { self.contract = contract }

    init(_ status: AgentStatus?) {
        self.contract = status?.contractVersion ?? 2
    }

    /// Durable operation ids, phases, `op`, `history`, `logs`, `cancel`, `doctor`, `bundle`,
    /// `cycle`, `--when-idle`, `--detach`, the policy model and the controller revision rules all
    /// arrived together in contract 3.
    var supportsOperations: Bool { contract >= 3 }
    var supportsDetach: Bool { contract >= 3 }
    var supportsQueue: Bool { contract >= 3 }
    var supportsPolicy: Bool { contract >= 3 }
    var supportsDoctor: Bool { contract >= 3 }
    var supportsLogs: Bool { contract >= 3 }
    var supportsHistory: Bool { contract >= 3 }
    var supportsCycle: Bool { contract >= 3 }
    var supportsBudget: Bool { contract >= 3 }
    var supportsControllerIdentity: Bool { contract >= 3 }
    var supportsServicePolicy: Bool { contract >= 3 }

    /// Whether the agent is new enough for the safety properties this app depends on.
    var meetsRequirement: Bool { contract >= requiredContract }

    /// Whether the agent is ahead of this app. Not a problem: the contract only ever adds keys, so
    /// the app keeps working and simply says so.
    var isAhead: Bool { contract > requiredContract }

    var description: String { "contract \(contract)" }
}

/// The token grammar every argument sent over ssh has to match.
///
/// Two separate defences, and both are wanted. The command is quoted for the remote shell, so a
/// metacharacter could not escape it anyway; and every id is checked against this before it is
/// quoted, so a service id that could only have come from a mangled config never reaches the far
/// side at all. Belt and braces is the right amount for something that ends in `sudo systemctl`.
enum AgentToken {
    static func isValidID(_ value: String) -> Bool {
        value.range(of: "^[A-Za-z0-9][A-Za-z0-9._-]*\\z", options: .regularExpression) != nil
    }

    static func isValidSetupID(_ value: String) -> Bool {
        value.range(of: "^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}\\z", options: .regularExpression) != nil
    }
    /// `^[A-Za-z0-9][A-Za-z0-9._:@/\~=+-]*$`, from the contract.
    static func isValid(_ token: String) -> Bool {
        guard let first = token.unicodeScalars.first else { return false }
        let alphanumeric = CharacterSet.alphanumerics
        guard alphanumeric.contains(first), first.isASCII else { return false }
        let allowed = CharacterSet(charactersIn: "._:@/\\~=+-")
        return token.unicodeScalars.allSatisfy { scalar in
            scalar.isASCII && (alphanumeric.contains(scalar) || allowed.contains(scalar))
        }
    }

    /// An operation id: a lowercase UUIDv4 or `^[a-z0-9-]{8,64}$`.
    static func isValidOperationId(_ id: String) -> Bool {
        guard (8...64).contains(id.count) else { return false }
        return id.unicodeScalars.allSatisfy { scalar in
            scalar == "-" || ("a"..."z").contains(scalar) || ("0"..."9").contains(scalar)
        }
    }

    /// A fresh operation id in the form the contract asks for.
    static func newOperationId() -> String { UUID().uuidString.lowercased() }

    /// `^\d+(m|h|d)$`, the duration form the agent accepts.
    static func duration(seconds: TimeInterval) -> String {
        let total = max(60, Int(seconds.rounded()))
        if total % 86_400 == 0 { return "\(total / 86_400)d" }
        if total % 3_600 == 0 { return "\(total / 3_600)h" }
        return "\(max(1, total / 60))m"
    }
}
