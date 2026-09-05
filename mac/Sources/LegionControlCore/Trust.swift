import CryptoKit
import Foundation

/// What this build will accept as a genuine Legion Control release, and how it checks.
///
/// The rule is simple and has no exceptions: nothing is installed, extracted, de-quarantined or
/// uploaded unless a signature made by the release key covers a manifest, and that manifest names
/// the exact artifact by basename with its sha256 and its size. HTTPS and control of a GitHub
/// account are not a substitute for that. A build that has no way to check simply cannot install,
/// and says so.
enum ReleaseTrust {

    /// The release signing key, copied from `contract/release-public-key.pem`.
    ///
    /// Committed on purpose: it is a public key, and pinning it here is what makes a release
    /// verifiable by a copy of the app that has already been installed. The private half is never in
    /// this repository and never on a build machine's disk unencrypted.
    ///
    /// Replacing it is a deliberate act. A new key has to ship in a release signed by the old one,
    /// so a user who has this build can always verify the release that introduces its successor.
    static let publicKeyPEM = """
    -----BEGIN PUBLIC KEY-----
    MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=
    -----END PUBLIC KEY-----
    """

    /// The key to verify with.
    ///
    /// There are exactly two seams in this trust boundary and both are narrow. The first is the
    /// `key` parameter on `verifiedManifest`, which the tests pass so they can show the verifier
    /// accepting a good signature as well as refusing a bad one. The second is an environment
    /// variable the shipping app never sets, for an end-to-end run against a test release. Neither
    /// can switch verification off: the only thing either substitutes is another Ed25519 public key.
    static func verificationKey() throws -> Curve25519.Signing.PublicKey {
        if let override = ProcessInfo.processInfo.environment["LEGION_CONTROL_TEST_RELEASE_KEY"],
           !override.isEmpty {
            return try publicKey(fromPEM: override)
        }
        return try publicKey(fromPEM: publicKeyPEM)
    }

    struct TrustFailure: Error, Sendable, Equatable {
        var message: String
        init(_ message: String) { self.message = message }
    }

    // MARK: - The manifest

    /// The signed release manifest, exactly as the contract defines it.
    struct Manifest: Decodable, Sendable, Equatable {
        var schema: Int
        var version: String
        var agentVersion: String?
        var artifacts: [Artifact]

        struct Artifact: Decodable, Sendable, Equatable {
            /// The exact basename. Matched exactly, never by suffix: "the first zip in the release"
            /// is how a client ends up installing the Windows build on a Mac.
            var name: String
            var sha256: String
            var size: Int64
        }

        func artifact(named name: String) -> Artifact? { artifacts.first { $0.name == name } }
    }

    /// Verify a detached signature over the manifest bytes, then decode it.
    ///
    /// The signature covers the bytes as they were downloaded and never a re-encoding of them: a
    /// manifest that is parsed first and re-serialised afterwards is a different document, and its
    /// signature would be checked against something the signer never saw.
    static func verifiedManifest(bytes: Data, signature: Data,
                                 key: Curve25519.Signing.PublicKey? = nil) throws -> Manifest {
        let key = try key ?? verificationKey()
        let raw = try rawSignature(signature)
        guard key.isValidSignature(raw, for: bytes) else {
            throw TrustFailure("The release manifest is not signed by the Legion Control release key, so nothing was installed.")
        }
        do {
            let manifest = try JSONDecoder().decode(Manifest.self, from: bytes)
            guard manifest.schema == 1 else {
                throw TrustFailure("The release manifest is schema \(manifest.schema) and this build only understands schema 1.")
            }
            return manifest
        } catch let failure as TrustFailure {
            throw failure
        } catch {
            throw TrustFailure("The release manifest is signed but could not be read. \(error.localizedDescription)")
        }
    }

    /// The detached signature file: base64 of the raw 64 bytes, with a trailing newline.
    static func rawSignature(_ file: Data) throws -> Data {
        let text = String(decoding: file, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        guard let raw = Data(base64Encoded: text) else {
            throw TrustFailure("The signature file is not base64, so it cannot be checked.")
        }
        guard raw.count == 64 else {
            throw TrustFailure("The signature is \(raw.count) bytes and an Ed25519 signature is 64.")
        }
        return raw
    }

    /// Check one downloaded file against what the manifest says it should be.
    static func verify(artifact: Manifest.Artifact, fileAt url: URL) throws {
        let attributes = try? FileManager.default.attributesOfItem(atPath: url.path(percentEncoded: false))
        let size = (attributes?[.size] as? NSNumber)?.int64Value ?? -1
        guard size == artifact.size else {
            throw TrustFailure("\(artifact.name) is \(size) bytes and the signed manifest says \(artifact.size), so it was not installed.")
        }
        let digest = try sha256(ofFileAt: url)
        guard digest == artifact.sha256.lowercased() else {
            throw TrustFailure("\(artifact.name) does not match the signed manifest: expected \(artifact.sha256), got \(digest).")
        }
    }

    /// The sha256 of a file, read in chunks so a hundred megabyte archive is not held in memory.
    static func sha256(ofFileAt url: URL) throws -> String {
        guard let handle = try? FileHandle(forReadingFrom: url) else {
            throw TrustFailure("\(url.lastPathComponent) could not be opened to hash it.")
        }
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    static func sha256(of data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    // MARK: - PEM

    /// An Ed25519 public key out of an SPKI PEM.
    ///
    /// CryptoKit wants the 32 raw bytes; SPKI wraps them in twelve bytes of DER that name the
    /// algorithm. The prefix is checked rather than skipped, so a PEM holding an RSA or P-256 key is
    /// refused instead of being read as though its middle were an Ed25519 key.
    static func publicKey(fromPEM pem: String) throws -> Curve25519.Signing.PublicKey {
        let body = pem
            .split(separator: "\n")
            .filter { !$0.hasPrefix("-----") }
            .joined()
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard let der = Data(base64Encoded: body) else {
            throw TrustFailure("The release public key is not a PEM this build can read.")
        }
        // SEQUENCE { SEQUENCE { OID 1.3.101.112 } BIT STRING { 32 bytes } }
        let ed25519SPKIPrefix: [UInt8] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00]
        guard der.count == ed25519SPKIPrefix.count + 32,
              Array(der.prefix(ed25519SPKIPrefix.count)) == ed25519SPKIPrefix else {
            throw TrustFailure("The release public key is not an Ed25519 key in SPKI form.")
        }
        return try Curve25519.Signing.PublicKey(rawRepresentation: der.suffix(32))
    }

    // MARK: - Artifact names

    /// The exact basenames from the contract. Named as constants because "the first .zip attached to
    /// the release" is precisely the guess this whole file exists to stop.
    enum Artifacts {
        static let macApp = "Legion-Control-macos-arm64.zip"
        static let manifest = "Legion-Control-manifest.json"
        static let manifestSignature = "Legion-Control-manifest.json.sig"
        static let agentManifest = "Legion-Control-agent-manifest.json"
        static let agentManifestSignature = "Legion-Control-agent-manifest.json.sig"
        static func agentArchive(version: String) -> String { "legionctl-agent-\(version).tgz" }
    }
}
