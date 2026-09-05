import CryptoKit
import Foundation
import Testing
@testable import LegionControlCore

/// What this build will accept as a genuine release.
///
/// The golden fixture matters more than the rest of this file: it is signed with the real release
/// key by the release tooling, and every client checks the same bytes. A mismatch between SPKI and
/// raw key handling, or between base64 conventions, is invisible in any single-language test and
/// fatal when the four implementations have to agree.
struct TrustTests {

    static var repositoryRoot: URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 { url = url.deletingLastPathComponent() }
        return url
    }

    // MARK: - The shared golden fixture

    @Test("the golden manifest signed with the real release key verifies")
    func goldenFixtureVerifies() throws {
        let directory = Self.repositoryRoot.appending(path: "tests/fixtures/trust")
        guard let manifest = try? Data(contentsOf: directory.appending(path: "manifest.json")),
              let signature = try? Data(contentsOf: directory.appending(path: "manifest.json.sig"))
        else {
            Issue.record("tests/fixtures/trust is missing, so the shared signature format was not checked")
            return
        }
        let verified = try ReleaseTrust.verifiedManifest(bytes: manifest, signature: signature)
        #expect(verified.schema == 1)
        #expect(!verified.artifacts.isEmpty)
    }

    @Test("one byte changed anywhere in the golden manifest is refused")
    func goldenFixtureTamper() throws {
        let directory = Self.repositoryRoot.appending(path: "tests/fixtures/trust")
        guard var manifest = try? Data(contentsOf: directory.appending(path: "manifest.json")),
              let signature = try? Data(contentsOf: directory.appending(path: "manifest.json.sig"))
        else {
            Issue.record("tests/fixtures/trust is missing, so the tamper case was not checked")
            return
        }
        // Flip one bit in the middle. Nothing about the document changes visibly and the signature
        // must still refuse it.
        let index = manifest.startIndex + manifest.count / 2
        manifest[index] ^= 0x01
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            _ = try ReleaseTrust.verifiedManifest(bytes: manifest, signature: signature)
        }
    }

    @Test("the pinned key is a well-formed Ed25519 key in SPKI form")
    func pinnedKeyParses() throws {
        let key = try ReleaseTrust.publicKey(fromPEM: ReleaseTrust.publicKeyPEM)
        #expect(key.rawRepresentation.count == 32)

        // The same value as the shared file, so this build and the release tooling cannot drift.
        let shared = Self.repositoryRoot.appending(path: "contract/release-public-key.pem")
        if let text = try? String(contentsOf: shared, encoding: .utf8) {
            let fromFile = try ReleaseTrust.publicKey(fromPEM: text)
            #expect(fromFile.rawRepresentation == key.rawRepresentation)
        }
    }

    @Test("a PEM holding some other kind of key is refused rather than misread")
    func refusesWrongKeyType() {
        // A P-256 SPKI header. Skipping the prefix instead of checking it would read its middle as
        // an Ed25519 key and verify nothing at all.
        let p256 = """
        -----BEGIN PUBLIC KEY-----
        MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEexampleexampleexampleexampleex
        ampleexampleexampleexampleexampleexampleexampleexampleexampleQ==
        -----END PUBLIC KEY-----
        """
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            _ = try ReleaseTrust.publicKey(fromPEM: p256)
        }
    }

    // MARK: - Verification with a key of our own

    /// A signing key made here, so the verifier can be shown accepting a good signature as well as
    /// refusing a bad one. The shipping app has no way to reach this: the override is an environment
    /// variable it never sets, and there is no path that turns verification off.
    static func signed(_ manifest: [String: Any]) throws -> (bytes: Data, signature: Data, key: Curve25519.Signing.PrivateKey) {
        let key = Curve25519.Signing.PrivateKey()
        let bytes = try JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys])
        let signature = try key.signature(for: bytes)
        return (bytes, Data((signature.base64EncodedString() + "\n").utf8), key)
    }

    /// The public half of a test key, in the SPKI PEM form the verifier parses.
    static func pem(for key: Curve25519.Signing.PrivateKey) -> String {
        let spki = Data([0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00])
            + key.publicKey.rawRepresentation
        return "-----BEGIN PUBLIC KEY-----\n\(spki.base64EncodedString())\n-----END PUBLIC KEY-----"
    }

    @Test("a manifest signed by the expected key is accepted, and one signed by another is not")
    func signatureVerification() throws {
        let manifest: [String: Any] = [
            "schema": 1,
            "version": "1.3.0",
            "agentVersion": "3.0.0",
            "artifacts": [["name": "Legion-Control-macos-arm64.zip",
                           "sha256": String(repeating: "a", count: 64),
                           "size": 1024] as [String: Any]]
        ]
        let (bytes, signature, key) = try Self.signed(manifest)

        let verified = try ReleaseTrust.verifiedManifest(bytes: bytes, signature: signature,
                                                         key: key.publicKey)
        #expect(verified.version == "1.3.0")
        #expect(verified.artifact(named: "Legion-Control-macos-arm64.zip")?.size == 1024)

        // The same bytes, a different key.
        let other = Curve25519.Signing.PrivateKey()
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            _ = try ReleaseTrust.verifiedManifest(bytes: bytes, signature: signature, key: other.publicKey)
        }

        // And the PEM the release tooling writes parses back to the same key, which is where a
        // cross-language SPKI mismatch would show up.
        let parsed = try ReleaseTrust.publicKey(fromPEM: Self.pem(for: key))
        #expect(parsed.rawRepresentation == key.publicKey.rawRepresentation)
    }

    @Test("a signature that is not 64 bytes is refused before anything else happens")
    func signatureShape() {
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            _ = try ReleaseTrust.rawSignature(Data("not base64 at all !!".utf8))
        }
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            _ = try ReleaseTrust.rawSignature(Data(Data(repeating: 0, count: 32).base64EncodedString().utf8))
        }
    }

    @Test("a file whose hash or size does not match the manifest is refused")
    func artifactVerification() throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-trust-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        let file = directory.appending(path: "artifact.bin")
        let contents = Data("legion".utf8)
        try contents.write(to: file)
        let realHash = ReleaseTrust.sha256(of: contents)

        let good = ReleaseTrust.Manifest.Artifact(name: "artifact.bin", sha256: realHash,
                                                  size: Int64(contents.count))
        try ReleaseTrust.verify(artifact: good, fileAt: file)

        let wrongHash = ReleaseTrust.Manifest.Artifact(name: "artifact.bin",
                                                       sha256: String(repeating: "0", count: 64),
                                                       size: Int64(contents.count))
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            try ReleaseTrust.verify(artifact: wrongHash, fileAt: file)
        }

        let wrongSize = ReleaseTrust.Manifest.Artifact(name: "artifact.bin", sha256: realHash, size: 9999)
        #expect(throws: ReleaseTrust.TrustFailure.self) {
            try ReleaseTrust.verify(artifact: wrongSize, fileAt: file)
        }
    }

    @Test("streaming the hash of a file agrees with hashing it in memory")
    func streamedHash() throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-trust-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        // Larger than the one megabyte read chunk, so the incremental path is actually exercised.
        let contents = Data((0..<(3 * 1024 * 1024)).map { UInt8($0 % 251) })
        let file = directory.appending(path: "big.bin")
        try contents.write(to: file)
        #expect(try ReleaseTrust.sha256(ofFileAt: file) == ReleaseTrust.sha256(of: contents))
    }

    // MARK: - The agent bundle

    @Test("a build with no signed agent bundle offers the action disabled rather than hiding it")
    func agentBundleAbsent() {
        let empty = URL(fileURLWithPath: NSTemporaryDirectory()).appending(path: "legion-no-agent-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: empty, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: empty) }
        #expect(AgentBundle.at(empty) == nil)
        #expect(AgentBundle.unavailableReason.contains("signed agent archive"))
    }

    @Test("an agent bundle whose archive does not match its signed manifest is refused before upload")
    func agentBundleTampered() throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory())
            .appending(path: "legion-agent-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        let archive = Data("pretend tarball".utf8)
        let archiveName = "legionctl-agent-3.0.0.tgz"
        try archive.write(to: directory.appending(path: archiveName))

        let manifest: [String: Any] = [
            "schema": 1,
            "version": "1.3.0",
            "agentVersion": "3.0.0",
            "artifacts": [["name": archiveName,
                           "sha256": ReleaseTrust.sha256(of: archive),
                           "size": Int64(archive.count)] as [String: Any]]
        ]
        let (bytes, signature, key) = try Self.signed(manifest)
        try bytes.write(to: directory.appending(path: ReleaseTrust.Artifacts.agentManifest))
        try signature.write(to: directory.appending(path: ReleaseTrust.Artifacts.agentManifestSignature))

        AgentBundle.verificationKey = key.publicKey
        defer { AgentBundle.verificationKey = nil }

        let bundle = try #require(AgentBundle.at(directory))
        #expect(bundle.version == "3.0.0")
        #expect(try bundle.verifiedArchive() == archive)

        // Now change the archive underneath the signed manifest. The hash in the manifest no longer
        // describes it, and nothing is uploaded.
        try Data("tampered tarball".utf8).write(to: directory.appending(path: archiveName))
        #expect(throws: ReleaseTrust.TrustFailure.self) { _ = try bundle.verifiedArchive() }
    }
}
