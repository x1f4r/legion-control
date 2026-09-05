import Darwin
import Foundation

/// Working out which of the setup's networks this device is on.
///
/// The honest answer is usually "probably that one". A private address prefix is a hint, not proof:
/// two households behind stock routers are both on 192.168.178, and a laptop that moves between
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
