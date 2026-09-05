import CryptoKit
import Foundation
import Testing
@testable import LegionControlCore

struct HostIdentityTests {
    static func key(_ seed: UInt8, algorithm: String = "ssh-ed25519") -> HostPublicKey {
        let name = Data(algorithm.utf8)
        var data = Data([0, 0, 0, UInt8(name.count)])
        data.append(name)
        data.append(Data([0, 0, 0, 32]))
        data.append(Data(repeating: seed, count: 32))
        return .init(algorithm: algorithm, blob: data.base64EncodedString())
    }
    static let systems = [HostIdentityDocument.System(id: "windows", name: "Windows"),
                          HostIdentityDocument.System(id: "linux", name: "Linux")]

    func temporary() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appending(path: "host-identities-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
    func context(_ dir: URL, existing: [HostPublicKey] = []) -> HostIdentityContext {
        let known = dir.appending(path: "known_hosts")
        return .init(address: "[tower-alias]:2222:2222", lookup: "tower-alias", scanHost: "192.168.178.20", port: 2222,
                     knownHosts: known, pinFiles: [known: (try? Data(contentsOf: known)) ?? Data()], existing: existing)
    }

    @Test("two algorithms use one named OS reservation and a third OS key is refused")
    func reservations() throws {
        let dir = try temporary(); defer { try? FileManager.default.removeItem(at: dir) }
        let store = HostIdentityStore(url: dir.appending(path: "identities.json"))
        let windows = [Self.key(1), Self.key(2, algorithm: "ssh-rsa")]
        let linux = [Self.key(3, algorithm: "ecdsa-sha2-nistp256")]
        try store.approve(context: context(dir), expectedRevision: 0, systems: Self.systems,
                          selected: "windows", offered: windows, legacyAssignments: [:])
        #expect(try store.load().endpoints[0].systems.filter { !$0.keys.isEmpty }.count == 1)
        try store.approve(context: context(dir, existing: windows), expectedRevision: 1, systems: Self.systems,
                          selected: "linux", offered: linux, legacyAssignments: [:])
        let pins = try Data(contentsOf: dir.appending(path: "known_hosts"))
        #expect(throws: HostIdentityFailure.self) {
            try store.approve(context: context(dir, existing: windows + linux), expectedRevision: 2,
                              systems: Self.systems + [.init(id: "third", name: "Added in shared config")],
                              selected: "third", offered: [Self.key(4)], legacyAssignments: [:])
        }
        #expect(try Data(contentsOf: dir.appending(path: "known_hosts")) == pins)
        #expect(try store.load().endpoints[0].systems.count == 2)
    }

    @Test("existing pins need explicit OS assignments and remain byte-for-byte intact")
    func legacyPins() throws {
        let dir = try temporary(); defer { try? FileManager.default.removeItem(at: dir) }
        let existing = Self.key(1)
        let original = "# other entry\n|1|hashed|entry \(existing.algorithm) \(existing.blob)"
        try Data(original.utf8).write(to: dir.appending(path: "known_hosts"))
        let store = HostIdentityStore(url: dir.appending(path: "identities.json"))
        #expect(throws: HostIdentityFailure.self) {
            try store.approve(context: context(dir, existing: [existing]), expectedRevision: 0,
                              systems: Self.systems, selected: "linux", offered: [Self.key(2)], legacyAssignments: [:])
        }
        try store.approve(context: context(dir, existing: [existing]), expectedRevision: 0,
                          systems: Self.systems, selected: "linux", offered: [Self.key(2)],
                          legacyAssignments: [existing.id: "windows"])
        #expect(try String(contentsOf: dir.appending(path: "known_hosts"), encoding: .utf8).hasPrefix(original + "\n"))
    }

    @Test("a mixed scan cannot spend an empty OS reservation on a new algorithm")
    func partialOverlap() throws {
        let dir = try temporary(); defer { try? FileManager.default.removeItem(at: dir) }
        let store = HostIdentityStore(url: dir.appending(path: "identities.json"))
        let old = Self.key(1)
        try store.approve(context: context(dir), expectedRevision: 0, systems: Self.systems,
                          selected: "windows", offered: [old], legacyAssignments: [:])
        #expect(throws: HostIdentityFailure.self) {
            try store.approve(context: context(dir, existing: [old]), expectedRevision: 1, systems: [],
                              selected: "linux", offered: [old, Self.key(2, algorithm: "ssh-rsa")], legacyAssignments: [:])
        }
        #expect(try store.load().endpoints[0].systems[1].keys.isEmpty)
    }

    @Test("a stale approval cannot replace an occupied group or lose changed pin files")
    func staleApproval() throws {
        let dir = try temporary(); defer { try? FileManager.default.removeItem(at: dir) }
        let store = HostIdentityStore(url: dir.appending(path: "identities.json"))
        let stale = context(dir)
        try store.approve(context: stale, expectedRevision: 0, systems: Self.systems,
                          selected: "windows", offered: [Self.key(1)], legacyAssignments: [:])
        #expect(throws: HostIdentityFailure.self) {
            try store.approve(context: stale, expectedRevision: 0, systems: Self.systems,
                              selected: "windows", offered: [Self.key(2)], legacyAssignments: [:])
        }
        #expect(try store.load().endpoints[0].systems[0].keys == [Self.key(1)])
    }

    @Test("interrupted pin append leaves occupied metadata and exact approval can be retried")
    func appendFailure() throws {
        let dir = try temporary(); defer { try? FileManager.default.removeItem(at: dir) }
        let occupied = dir.appending(path: "known_hosts")
        let store = HostIdentityStore(url: dir.appending(path: "identities.json")) { data, url in
            if url == occupied { throw HostIdentityFailure("Simulated append failure") }
            try AtomicFile.write(data, to: url)
        }
        #expect(throws: (any Error).self) {
            try store.approve(context: context(dir), expectedRevision: 0, systems: Self.systems,
                              selected: "windows", offered: [Self.key(1)], legacyAssignments: [:])
        }
        #expect(try store.load().endpoints[0].systems[0].keys == [Self.key(1)])
        try HostIdentityStore(url: store.url).approve(context: context(dir), expectedRevision: 0, systems: [], selected: "windows",
                          offered: [Self.key(1)], legacyAssignments: [:])
        #expect(try String(contentsOf: occupied, encoding: .utf8).contains(Self.key(1).blob))
    }

    @Test("malformed scans and duplicate algorithms cannot be approved")
    func malformedScans() throws {
        let one = Self.key(1); let two = Self.key(2)
        #expect(throws: HostIdentityFailure.self) { try HostPublicKey.parse("host ssh-ed25519 not-a-key", uniqueAlgorithms: true) }
        #expect(throws: HostIdentityFailure.self) {
            try HostPublicKey.parse("host \(one.id)\nhost \(two.id)", uniqueAlgorithms: true)
        }
        #expect(try HostPublicKey.parse("# comment\nhost \(one.id)", uniqueAlgorithms: true) == [one])
    }

    @Test("effective SSH aliases and hashed pins preserve custom known_hosts and nondefault ports")
    func nativeLookup() async throws {
        let dir = try temporary(); defer { try? FileManager.default.removeItem(at: dir) }
        let known = dir.appending(path: "custom_known_hosts")
        let key = Self.key(1)
        try Data("|1|hashed|entry \(key.id)\n".utf8).write(to: known)
        let target = SSHTarget(host: "private-alias", user: nil, port: nil, identityFile: nil)
        let result = try await HostIdentityContext.resolve(target) { executable, arguments, _ in
            if executable == "/usr/bin/ssh" {
                #expect(arguments.first == "-G")
                #expect(arguments.last == "private-alias")
                return TransportTests.result(out: "hostname 192.168.178.20\nport 2222\nhostkeyalias shared-tower\nuserknownhostsfile \(known.path)\nglobalknownhostsfile /dev/null\n")
            }
            #expect(arguments == ["-F", "shared-tower", "-f", known.path])
            return TransportTests.result(out: "# Host shared-tower found\n|1|hashed|entry \(key.id)\n")
        }
        #expect(result.lookup == "shared-tower")
        #expect(result.scanHost == "192.168.178.20")
        #expect(result.port == 2222)
        #expect(result.knownHosts == known)
        #expect(result.existing == [key])
    }

    @Test("complex SSH key providers retain native verification and disable app enrollment")
    func externalProvider() async {
        await #expect(throws: HostIdentityFailure.self) {
            try await HostIdentityContext.resolve(.init(host: "alias", user: nil, port: nil, identityFile: nil)) { _, _, _ in
                TransportTests.result(out: "hostname 192.168.178.20\nport 22\nproxycommand helper --connect\n")
            }
        }
    }
}
