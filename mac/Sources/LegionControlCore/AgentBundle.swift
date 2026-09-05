import CryptoKit
import Foundation

/// The signed archive for upgrading an authenticated existing agent.
///
/// Three files live in `Contents/Resources/agent/`: the tarball, the signed release-schema manifest
/// that names it, and the detached signature over that manifest. They are put there by the release
/// build. A development build made without them still runs and still offers the action — disabled,
/// with the reason spelled out, because an action that quietly disappears is one nobody can ask
/// about.
struct AgentBundle: Sendable {
    var archiveURL: URL
    var manifestURL: URL
    var signatureURL: URL
    var version: String

    typealias TrustFailure = ReleaseTrust.TrustFailure

    /// The bundle this build shipped with, or nil when it shipped without one.
    static let shared: AgentBundle? = locate()

    /// The key the archive's manifest is checked against. Set only by the tests, which have to be
    /// able to produce a signed bundle of their own; the shipping app leaves it nil and the pinned
    /// release key is used.
    nonisolated(unsafe) static var verificationKey: Curve25519.Signing.PublicKey?

    /// Why there is nothing to install, for the disabled action's explanation.
    static let unavailableReason = "The signed agent archive is not included in this build."

    private static func locate(in resources: URL? = nil) -> AgentBundle? {
        let directory = resources ?? Bundle.main.resourceURL?.appending(path: "agent")
        guard let directory else { return nil }
        let manifest = directory.appending(path: ReleaseTrust.Artifacts.agentManifest)
        let signature = directory.appending(path: ReleaseTrust.Artifacts.agentManifestSignature)
        guard FileManager.default.isReadableFile(atPath: manifest.path(percentEncoded: false)),
              FileManager.default.isReadableFile(atPath: signature.path(percentEncoded: false))
        else { return nil }

        // The version comes out of the manifest rather than out of a file name, so the app never
        // claims to be shipping a version the signature does not cover.
        guard let bytes = try? Data(contentsOf: manifest),
              let signatureBytes = try? Data(contentsOf: signature),
              let verified = try? ReleaseTrust.verifiedManifest(bytes: bytes, signature: signatureBytes,
                                                                key: verificationKey),
              let version = verified.agentVersion ?? verified.artifacts.first.map({ agentVersion(from: $0.name) }) ?? nil
        else { return nil }

        let archive = directory.appending(path: ReleaseTrust.Artifacts.agentArchive(version: version))
        guard FileManager.default.isReadableFile(atPath: archive.path(percentEncoded: false)) else { return nil }
        return AgentBundle(archiveURL: archive, manifestURL: manifest, signatureURL: signature, version: version)
    }

    /// For the tests: a bundle rooted anywhere.
    static func at(_ directory: URL) -> AgentBundle? { locate(in: directory) }

    /// `legionctl-agent-3.0.0.tgz` to `3.0.0`.
    static func agentVersion(from name: String) -> String? {
        guard name.hasPrefix("legionctl-agent-"), name.hasSuffix(".tgz") else { return nil }
        return String(name.dropFirst("legionctl-agent-".count).dropLast(".tgz".count))
    }

    /// The archive bytes, checked against the signed manifest before they leave this Mac.
    ///
    /// A tampered archive is refused before a single byte is uploaded, including when upgrading
    /// a legacy agent that cannot perform this verification itself.
    func verifiedArchive() throws -> Data {
        let manifestBytes = try read(manifestURL, what: "the agent manifest")
        let signatureBytes = try read(signatureURL, what: "the agent manifest signature")
        let manifest = try ReleaseTrust.verifiedManifest(bytes: manifestBytes, signature: signatureBytes,
                                                         key: Self.verificationKey)

        let name = archiveURL.lastPathComponent
        guard let artifact = manifest.artifact(named: name) else {
            throw TrustFailure("The signed agent manifest does not name \(name), so it was not sent.")
        }
        let payload = try read(archiveURL, what: "the agent archive")
        guard payload.count == artifact.size, ReleaseTrust.sha256(of: payload) == artifact.sha256 else {
            throw TrustFailure("The agent archive does not match its signed size and hash, so it was not sent.")
        }
        return payload
    }

    private func read(_ url: URL, what: String) throws -> Data {
        guard let data = try? Data(contentsOf: url) else {
            throw TrustFailure("\(what) could not be read from this app's own resources.")
        }
        return data
    }
}
