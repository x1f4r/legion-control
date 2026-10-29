import Darwin
import Foundation

/// Working out which of the setup's networks this device is on.
///
/// The honest answer is usually "probably that one". A private address prefix is a hint, not proof:
/// two households behind stock routers are both on 192.168.50, and a laptop that moves between
/// them sees the same addresses in both. So this reports what matches, says when more than one
/// thing matches, and never picks a winner on its own.
///
/// What *is* proof of which machine answered is the pinned host key, checked on every connection.
/// Site detection only decides whether it is worth trying a magic packet from here and which
/// addresses to dial first; it never decides who a machine is.
enum SiteAwareness {

    /// Where this device thinks it is.
    struct Placement: Sendable, Equatable {
        /// Sites whose prefixes match one of this device's addresses.
        var matching: [String]
        /// The site the user picked, when they picked one.
        var confirmed: String?

        /// The site to act as though we are on, or nil when there is no safe answer.
        ///
        /// A confirmed choice always wins. One match and nothing else is good enough to try a direct
        /// packet. Two matches means the prefixes cannot tell those sites apart, and the safe move is
        /// to behave as though we are somewhere else entirely and use a helper — which works either
        /// way, whereas broadcasting into the wrong house does not.
        var effective: String? {
            if let confirmed, !confirmed.isEmpty { return confirmed }
            return matching.count == 1 ? matching.first : nil
        }

        var isAmbiguous: Bool { confirmed == nil && matching.count > 1 }

        /// What the machine section says under the wake row.
        func description(sites: [Site]) -> String {
            func name(_ id: String) -> String { sites.first { $0.id == id }?.name ?? id }
            if let confirmed, !confirmed.isEmpty {
                let agrees = matching.contains(confirmed)
                return agrees
                    ? "On \(name(confirmed)), confirmed."
                    : "Set to \(name(confirmed)), though this device's addresses do not match it. Site unconfirmed."
            }
            switch matching.count {
            case 0:
                return "Not on any configured network, so a magic packet from here would not reach anything."
            case 1:
                return "Looks like \(name(matching[0])), from this device's addresses. Site unconfirmed: a private address range is a hint, not proof."
            default:
                let names = matching.map(name).joined(separator: " and ")
                return "This device's addresses match \(names). Overlapping private ranges cannot be told apart, so wake goes through a helper unless you say which site this is."
            }
        }
    }

    /// Every IPv4 address this device currently has, excluding the loopback.
    static func localAddresses() -> [String] {
        var addresses: [String] = []
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return addresses }
        defer { freeifaddrs(head) }

        var cursor: UnsafeMutablePointer<ifaddrs>? = first
        while let entry = cursor {
            defer { cursor = entry.pointee.ifa_next }
            guard let raw = entry.pointee.ifa_addr,
                  raw.pointee.sa_family == UInt8(AF_INET),
                  entry.pointee.ifa_flags & UInt32(IFF_UP) != 0,
                  entry.pointee.ifa_flags & UInt32(IFF_LOOPBACK) == 0
            else { continue }

            var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let result = getnameinfo(raw, socklen_t(raw.pointee.sa_len),
                                     &buffer, socklen_t(buffer.count),
                                     nil, 0, NI_NUMERICHOST)
            if result == 0 {
                let bytes = buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }
                let text = String(decoding: bytes, as: UTF8.self)
                if !text.isEmpty { addresses.append(text) }
            }
        }
        return addresses
    }

    /// Which sites the given addresses could belong to.
    static func placement(sites: [Site], addresses: [String], confirmed: String?) -> Placement {
        var matching: [String] = []
        for site in sites {
            let hit = site.lanPrefixes.contains { prefix in
                !prefix.isEmpty && addresses.contains { $0.hasPrefix(prefix) }
            }
            if hit { matching.append(site.id) }
        }
        return Placement(matching: matching, confirmed: confirmed)
    }

    /// Whether this device is on the network a machine without a site named, using the old
    /// per-machine prefix.
    static func matchesLANPrefix(_ prefix: String?, addresses: [String]) -> Bool {
        guard let prefix, !prefix.isEmpty else { return false }
        return addresses.contains { $0.hasPrefix(prefix) }
    }
}

/// A LAN prefix as section 10.1 of the contract reads one: a network and a length, never text.
///
/// Every client used to match a prefix with "starts with" on the address text, so `192.168.1`
/// matched 192.168.10.7 and a device two subnets away believed it was at home (A9). A prefix is a
/// dotted form of one to three octets with an optional trailing dot, `192.168.1` and `192.168.1.`
/// being the same /24, or a CIDR network with no host bits set. Anything else, a full address
/// without a length, IPv6, a wildcard, a space, is no prefix and never matches anything. This is the
/// grammar of `contract/tools/wake-plan.mjs`; the setup's warnings are written on it.
struct LanPrefix: Sendable, Equatable {
    enum Form: Sendable, Equatable {
        case dotted
        case cidr
    }

    var network: UInt32
    var length: Int
    var form: Form

    init?(_ text: String) {
        guard !text.isEmpty else { return nil }
        if let slash = text.firstIndex(of: "/") {
            let lengthText = text[text.index(after: slash)...]
            guard let network = Self.ipv4(String(text[..<slash])),
                  lengthText.utf8.allSatisfy({ (0x30...0x39).contains($0) }),
                  lengthText.count <= 2, lengthText.first != "0" || lengthText.count == 1,
                  let length = Int(lengthText), (8...32).contains(length),
                  network & ~Self.mask(length) == 0
            else { return nil }
            self.init(network: network, length: length, form: .cidr)
            return
        }
        let body = text.hasSuffix(".") ? text.dropLast() : Substring(text)
        let parts = body.split(separator: ".", omittingEmptySubsequences: false)
        guard (1...3).contains(parts.count) else { return nil }
        var network: UInt32 = 0
        for part in parts {
            guard let octet = Self.octet(part) else { return nil }
            network = network << 8 | octet
        }
        self.init(network: network << (8 * (4 - parts.count)), length: 8 * parts.count, form: .dotted)
    }

    init(network: UInt32, length: Int, form: Form) {
        self.network = network
        self.length = length
        self.form = form
    }

    /// Whether an address, as a 32-bit number, is inside this prefix.
    func contains(_ address: UInt32) -> Bool {
        address & Self.mask(length) == network & Self.mask(length)
    }

    /// Whether two prefixes share any address: the shorter one contains the other's network.
    func overlaps(_ other: LanPrefix) -> Bool {
        let mask = Self.mask(min(length, other.length))
        return network & mask == other.network & mask
    }

    /// A dotted-quad IPv4 address as a 32-bit number, or nil. Exactly four decimal octets from 0 to
    /// 255 without leading zeros: `192.168.01.1` is octal to some parsers and decimal to others, so
    /// it is neither.
    static func ipv4(_ text: String) -> UInt32? {
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return nil }
        var value: UInt32 = 0
        for part in parts {
            guard let octet = octet(part) else { return nil }
            value = value << 8 | octet
        }
        return value
    }

    private static func octet(_ text: Substring) -> UInt32? {
        guard (1...3).contains(text.utf8.count), text.utf8.allSatisfy({ (0x30...0x39).contains($0) }),
              text == "0" || text.first != "0", let value = UInt32(text), value <= 255
        else { return nil }
        return value
    }

    private static func mask(_ length: Int) -> UInt32 {
        length == 0 ? 0 : UInt32.max << (32 - length)
    }
}
