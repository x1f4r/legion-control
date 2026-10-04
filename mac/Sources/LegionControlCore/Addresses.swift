import Foundation

/// The hosts, users and ports this app will dial, scan or pin (section 15.6 of the contract).
///
/// A host string from the shared document used to reach ssh, ssh-keyscan and known_hosts exactly as
/// written. On OpenSSH before 9.6 a host such as `victim.example,*` written into known_hosts pins a
/// key for every host there is, and a host such as `-oProxyCommand=…` is an option rather than an
/// address to any tool not told where its options end. The setup schema refuses only what those
/// tools read as syntax; this is the stricter grammar a client holds a value to before it uses it,
/// and a value that passes the schema but not this is kept in the document and simply never dialled.
///
/// Every class here is ASCII. Lengths are counted the way the reference counts them, in UTF-16 code
/// units, which only matters for strings that are refused anyway.
enum Addresses {
    enum HostForm: String, Sendable, Equatable {
        case dns, ipv4, ipv6
    }

    /// What kind of host this is, or nil when it is not one this app may dial.
    ///
    /// A name is dot-separated labels of letters, digits, `_` and inner `-`; a name whose last label
    /// is all digits has to be a whole dotted quad, because `1234` and `1.2.3` are what a resolver
    /// turns into addresses nobody wrote. IPv6 is the RFC 4291 text form without brackets or a zone.
    /// No trailing dot, no port, no user.
    static func hostForm(_ value: String) -> HostForm? {
        guard (1...253).contains(value.utf16.count) else { return nil }
        if value.unicodeScalars.contains(":") { return isIPv6(value) ? .ipv6 : nil }
        let labels = value.split(separator: ".", omittingEmptySubsequences: false)
        guard labels.allSatisfy(isLabel) else { return nil }
        guard let last = labels.last, last.unicodeScalars.allSatisfy(isDigit) else { return .dns }
        return isIPv4(value) ? .ipv4 : nil
    }

    /// A login name: a local or domain account of 1 to 64 characters, nothing a shell or ssh reads as
    /// syntax, and never two words that could be taken for an option.
    static func isUser(_ value: String) -> Bool {
        let scalars = value.unicodeScalars
        guard (1...64).contains(value.utf16.count), let first = scalars.first, let last = scalars.last else { return false }
        let starts = isAlphanumeric(first) || first == "_"
        let ends = isAlphanumeric(last) || userEnds.contains(last)
        guard starts, ends, scalars.allSatisfy({ isAlphanumeric($0) || userMiddles.contains($0) }) else { return false }
        return !zip(scalars, scalars.dropFirst()).contains { pair in pair.0 == " " && (pair.1 == " " || pair.1 == "-") }
    }

    private static let userEnds: Set<Unicode.Scalar> = [".", "_", "-"]
    private static let userMiddles: Set<Unicode.Scalar> = [".", "_", "@", "\\", " ", "-"]

    /// A TCP port. The document's decoder already refuses anything that is not an integer.
    static func isPort(_ value: Int) -> Bool { (1...65535).contains(value) }

    /// The part of this target this app will not dial, scan or pin, said so it fits in a sentence,
    /// or nil when all of it may be used.
    static func undialablePart(of target: SSHTarget) -> String? {
        if hostForm(target.host) == nil { return "the host \"\(target.host)\"" }
        if let user = target.user, !user.isEmpty, !isUser(user) { return "the user \"\(user)\"" }
        if let port = target.port, !isPort(port) { return "the port \(port)" }
        return nil
    }

    /// Why this target was not dialled, or nil when it may be.
    static func problem(with target: SSHTarget) -> String? {
        undialablePart(of: target).map { "\(target.display) was skipped: \($0) is not one this app dials." }
    }

    // MARK: - Pieces

    private static func isDigit(_ scalar: Unicode.Scalar) -> Bool { ("0"..."9").contains(scalar) }

    private static func isAlphanumeric(_ scalar: Unicode.Scalar) -> Bool {
        isDigit(scalar) || ("a"..."z").contains(scalar) || ("A"..."Z").contains(scalar)
    }

    private static func isHex(_ scalar: Unicode.Scalar) -> Bool {
        isDigit(scalar) || ("a"..."f").contains(scalar) || ("A"..."F").contains(scalar)
    }

    /// `[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?`
    private static func isLabel(_ label: Substring) -> Bool {
        let scalars = label.unicodeScalars
        guard (1...63).contains(scalars.count), let first = scalars.first, let last = scalars.last else { return false }
        let edge = { (scalar: Unicode.Scalar) in isAlphanumeric(scalar) || scalar == "_" }
        return edge(first) && edge(last) && scalars.allSatisfy { edge($0) || $0 == "-" }
    }

    /// Four octets of 0 to 255, without leading zeros.
    private static func isIPv4<S: StringProtocol>(_ value: S) -> Bool {
        let parts = value.split(separator: ".", omittingEmptySubsequences: false)
        return parts.count == 4 && parts.allSatisfy { part in
            let scalars = part.unicodeScalars
            guard (1...3).contains(scalars.count), scalars.allSatisfy(isDigit) else { return false }
            if scalars.count > 1, scalars.first == "0" { return false }
            return Int(part).map { $0 <= 255 } ?? false
        }
    }

    /// Eight groups of one to four hex digits, at most one `::` standing for one or more of them,
    /// and an embedded IPv4 address only as the last thing in the address.
    private static func isIPv6(_ value: String) -> Bool {
        guard value.unicodeScalars.allSatisfy({ isHex($0) || $0 == ":" || $0 == "." }) else { return false }
        let halves = value.components(separatedBy: "::")
        guard halves.count <= 2 else { return false }
        func groups(_ text: String) -> [Substring] {
            text.isEmpty ? [] : text.split(separator: ":", omittingEmptySubsequences: false)
        }
        let head = groups(halves[0])
        let tail = halves.count == 2 ? groups(halves[1]) : []
        let all = head + tail
        var count = 0
        for (index, group) in all.enumerated() {
            if group.unicodeScalars.contains(".") {
                let isLast = index == all.count - 1 && (halves.count == 1 || !tail.isEmpty)
                guard isLast, isIPv4(group) else { return false }
                count += 2
            } else if (1...4).contains(group.unicodeScalars.count), group.unicodeScalars.allSatisfy(isHex) {
                count += 1
            } else {
                return false
            }
        }
        return halves.count == 2 ? count <= 7 : count == 8
    }
}
