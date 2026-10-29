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

    /// Where this device thinks it is (section 10.1).
    struct Placement: Sendable, Equatable {
        /// Sites, in the order the setup lists them, with a valid prefix that holds one of this
        /// device's addresses.
        var matching: [String]
        /// The site to act as though we are on, or nil when there is no safe answer.
        var effective: String?
        /// True when several sites match and nobody has said which this is.
        var isAmbiguous: Bool
        /// The site the user picked, when they picked one, whether or not it was taken.
        var confirmed: String?
        /// The addresses it was worked out from. The first one inside the prefix that placed a
        /// machine is the one its directed broadcast is derived from.
        var addresses: [LocalAddress]

        static let nowhere = Placement(matching: [], effective: nil, isAmbiguous: false, confirmed: nil, addresses: [])

        /// What the machine section says under the wake row.
        func description(sites: [Site]) -> String {
            func name(_ id: String) -> String { sites.first { $0.id == id }?.name ?? id }
            if let confirmed, !confirmed.isEmpty, sites.contains(where: { $0.id == confirmed }) {
                return effective == confirmed
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

    /// The IPv4 addresses this device could send a broadcast from: on interfaces that are up and
    /// can carry one, never loopback and never a tunnel or point-to-point link, each with the length
    /// of its network when the interface gives a contiguous mask.
    ///
    /// Every address used to count, a VPN's included, so a laptop on the train with a tunnel into
    /// the office matched the office's prefix and broadcast into a network it was not on.
    static func localAddresses() -> [LocalAddress] {
        var addresses: [LocalAddress] = []
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return addresses }
        defer { freeifaddrs(head) }

        var cursor: UnsafeMutablePointer<ifaddrs>? = first
        while let entry = cursor {
            defer { cursor = entry.pointee.ifa_next }
            guard let raw = entry.pointee.ifa_addr,
                  isCandidate(flags: entry.pointee.ifa_flags, family: raw.pointee.sa_family),
                  let address = ipv4(raw)
            else { continue }
            let length = entry.pointee.ifa_netmask.flatMap(ipv4).flatMap(LocalAddress.prefixLength(ofMask:))
            if let local = LocalAddress(address: address, prefixLength: length) { addresses.append(local) }
        }
        return addresses
    }

    /// Whether an interface's address can place this device: IPv4, on an interface that is up and
    /// can broadcast, and neither loopback nor point-to-point. A VPN's `utun` is up and
    /// point-to-point and cannot broadcast, so its address, however much it looks like the office's
    /// network, never says this device is there.
    static func isCandidate(flags: UInt32, family: sa_family_t) -> Bool {
        family == sa_family_t(AF_INET)
            && flags & UInt32(IFF_UP) != 0
            && flags & UInt32(IFF_BROADCAST) != 0
            && flags & UInt32(IFF_LOOPBACK) == 0
            && flags & UInt32(IFF_POINTOPOINT) == 0
    }

    /// The IPv4 address a `sockaddr` holds, as a 32-bit number in host order.
    ///
    /// Only the `sa_len` bytes the kernel filled in are read. A netmask often comes back shortened
    /// to its significant bytes, and the bytes after them are not its to read.
    private static func ipv4(_ pointer: UnsafeMutablePointer<sockaddr>) -> UInt32? {
        let length = min(Int(pointer.pointee.sa_len), MemoryLayout<sockaddr_in>.size)
        guard length > 0 else { return nil }
        var storage = sockaddr_in()
        withUnsafeMutableBytes(of: &storage) { target in
            target.copyMemory(from: UnsafeRawBufferPointer(start: pointer, count: length))
        }
        return UInt32(bigEndian: storage.sin_addr.s_addr)
    }

    /// The valid prefixes of a site. Anything else never matches, and the setup warns about it.
    static func prefixes(of site: Site) -> [LanPrefix] { site.lanPrefixes.compactMap(LanPrefix.init) }

    /// The first of the addresses inside any of the prefixes: the placing address.
    static func firstInside(_ prefixes: [LanPrefix], _ addresses: [LocalAddress]) -> LocalAddress? {
        addresses.first { candidate in prefixes.contains { $0.contains(candidate.address) } }
    }

    /// Which sites the given addresses could belong to, and which of them, if any, this device is
    /// on.
    ///
    /// A `currentSite` that names a site decides, but only while that site has no valid prefix or
    /// one of its prefixes matches: the person's answer tells two sites with the same subnet apart,
    /// it does not put a device on a network it is not on. It used to win outright, so a laptop
    /// set to "home" broadcast at home's network from a café. A `currentSite` that names no site
    /// is ignored. Otherwise exactly one matching site is effective, and several are ambiguous: a
    /// device never picks the first of two homes behind the same stock router.
    static func placement(sites: [Site], addresses: [LocalAddress], confirmed: String?) -> Placement {
        let matching = sites.filter { firstInside(prefixes(of: $0), addresses) != nil }.map(\.id)
        let picked = confirmed.flatMap { id in sites.first { $0.id == id } }
        if let picked {
            let valid = prefixes(of: picked)
            let corroborated = valid.isEmpty || firstInside(valid, addresses) != nil
            return Placement(matching: matching, effective: corroborated ? picked.id : nil, isAmbiguous: false,
                             confirmed: confirmed, addresses: addresses)
        }
        return Placement(matching: matching, effective: matching.count == 1 ? matching[0] : nil,
                         isAmbiguous: matching.count > 1, confirmed: confirmed, addresses: addresses)
    }

    /// Whether a machine is on this device's network: the one test route order and direct wake
    /// share (section 10.1).
    enum Network: Sendable, Equatable {
        /// On it. `placing` is the address inside the prefix that placed it, nil for a site taken
        /// on the person's word with no prefixes at all.
        case on(placing: LocalAddress?)
        /// Its site is one of several this device's addresses match.
        case ambiguous
        /// It names no site in the setup and has no valid `wake.lanPrefix` either.
        case unplaced
        case elsewhere

        var isOn: Bool { if case .on = self { true } else { false } }
    }

    /// A machine that names a site in the setup is on this device's network exactly when that site
    /// is effective. Only one that names no such site falls back to its deprecated `wake.lanPrefix`,
    /// matched as a prefix and never as text.
    static func network(of machine: Machine, sites: [Site], placement: Placement) -> Network {
        if let id = machine.site, let site = sites.first(where: { $0.id == id }) {
            if placement.effective == site.id {
                return .on(placing: firstInside(prefixes(of: site), placement.addresses))
            }
            return placement.isAmbiguous && placement.matching.contains(site.id) ? .ambiguous : .elsewhere
        }
        guard let prefix = machine.wake?.lanPrefix.flatMap(LanPrefix.init) else { return .unplaced }
        return firstInside([prefix], placement.addresses).map { .on(placing: $0) } ?? .elsewhere
    }
}

/// One of this device's own IPv4 addresses, with the length of its network when the interface
/// said, as section 10.1 reads one.
struct LocalAddress: Sendable, Equatable {
    var address: UInt32
    var prefixLength: Int?

    private static let loopback = LanPrefix(network: 0x7F00_0000, length: 8, form: .cidr)

    /// Nil for loopback, which is on no site's network however its prefixes read, and for a length
    /// outside 0 to 32.
    init?(address: UInt32, prefixLength: Int?) {
        guard !Self.loopback.contains(address), prefixLength.map({ (0...32).contains($0) }) ?? true else { return nil }
        self.address = address
        self.prefixLength = prefixLength
    }

    /// `a.b.c.d` or `a.b.c.d/length`, as the wake vectors write this device's addresses.
    init?(_ text: String) {
        let parts = text.split(separator: "/", maxSplits: 1, omittingEmptySubsequences: false)
        guard let address = LanPrefix.ipv4(String(parts[0])) else { return nil }
        var length: Int?
        if parts.count == 2 {
            let digits = parts[1]
            guard (1...2).contains(digits.utf8.count), digits.utf8.allSatisfy({ (0x30...0x39).contains($0) }),
                  digits.first != "0" || digits.count == 1, let value = Int(digits) else { return nil }
            length = value
        }
        self.init(address: address, prefixLength: length)
    }

    /// The length of a contiguous netmask, or nil for one that is not.
    static func prefixLength(ofMask mask: UInt32) -> Int? {
        let length = (~mask).leadingZeroBitCount
        return mask == (length == 0 ? 0 : UInt32.max << (32 - length)) ? length : nil
    }

    /// The broadcast address of this address's own network, with every host bit set, when the
    /// length is known and leaves at least two host bits.
    var directedBroadcast: String? {
        guard let prefixLength, prefixLength <= 30 else { return nil }
        let mask: UInt32 = prefixLength == 0 ? 0 : UInt32.max << (32 - prefixLength)
        return LanPrefix.text(address | ~mask)
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

    /// How an editor writes this prefix: an octet-aligned one of up to three octets as dotted
    /// octets with the trailing dot, any other as CIDR.
    var normalised: String {
        guard length % 8 == 0, length <= 24 else { return "\(Self.text(network))/\(length)" }
        return Self.text(network).split(separator: ".").prefix(length / 8).joined(separator: ".") + "."
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

    /// A 32-bit IPv4 address as dotted-quad text.
    static func text(_ address: UInt32) -> String {
        [24, 16, 8, 0].map { String((address >> UInt32($0)) & 0xFF) }.joined(separator: ".")
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
