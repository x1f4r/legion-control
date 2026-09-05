using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using LegionControl.Desktop.Contract;
using Org.BouncyCastle.Crypto.Parameters;
using Org.BouncyCastle.Crypto.Signers;

namespace LegionControl.Desktop;

/// The one key this build trusts, and what it will and will not accept.
///
/// The public key is compiled into the binary from contract/release-public-key.pem and read from
/// the assembly, never from disk. A key on the machine is a key an attacker can put there, and a
/// verifier that would accept one is a verifier that verifies nothing.
///
/// There is deliberately no way to skip a check from here. An artifact that does not verify is not
/// installed, and this app says so rather than offering to go ahead anyway.
public static class Trust
{
    private const string ResourceName = "release-public-key.pem";

    private static readonly Lazy<byte[]?> PublicKey = new(ReadPublicKey);

    /// Whether this build has a usable trust key at all. False means every signed operation is
    /// unavailable and says why, which is the only honest thing to do about a build whose key did
    /// not make it in.
    public static bool HasKey => PublicKey.Value is not null;

    /// The key's own fingerprint, for the about screen. Nothing depends on it; it is there so a
    /// person can see which key a build carries without unpacking it.
    public static string? KeyFingerprint => PublicKey.Value is { } key
        ? Convert.ToHexString(SHA256.HashData(key))[..16].ToLowerInvariant()
        : null;

    /// Verifies a detached ed25519 signature over exactly these bytes.
    ///
    /// The signature file is base64 of the raw 64 bytes, with a trailing newline allowed, over the
    /// exact bytes of the manifest as they arrived. Nothing is re-serialised before verifying: a
    /// signature is over bytes, and a re-encoded document is different bytes.
    public static bool Verify(byte[] message, string signatureBase64)
    {
        if (PublicKey.Value is not { } key) return false;
        byte[] signature;
        try
        {
            signature = Convert.FromBase64String(signatureBase64.Trim());
        }
        catch (FormatException)
        {
            return false;
        }
        if (signature.Length != 64) return false;

        try
        {
            var verifier = new Ed25519Signer();
            verifier.Init(false, new Ed25519PublicKeyParameters(key, 0));
            verifier.BlockUpdate(message, 0, message.Length);
            return verifier.VerifySignature(signature);
        }
        catch (Exception)
        {
            return false;
        }
    }

    /// The 32 raw key bytes out of the SPKI PEM. Returns null when the resource is missing or is
    /// not an ed25519 SPKI key, which is treated everywhere as "this build cannot verify anything".
    private static byte[]? ReadPublicKey()
    {
        try
        {
            using var stream = Assembly.GetExecutingAssembly().GetManifestResourceStream(ResourceName);
            if (stream is null) return null;
            using var reader = new StreamReader(stream);
            var pem = reader.ReadToEnd();
            var body = string.Concat(pem
                .Split('\n')
                .Select(line => line.Trim())
                .Where(line => line.Length > 0 && !line.StartsWith("-----", StringComparison.Ordinal)));
            var der = Convert.FromBase64String(body);
            // SPKI for ed25519 is a fixed 44 bytes: a 12 byte header and the 32 byte key.
            if (der.Length != 44) return null;
            return der[^32..];
        }
        catch (Exception)
        {
            return null;
        }
    }
}

/// One artifact in a release, as the signed manifest describes it.
public sealed record ReleaseArtifact(string Name, string Sha256, long Size);

/// A signed release manifest: what is in a release, and the hash and size of each part.
///
/// Nothing is installed from a release without one of these verifying first. The signature covers
/// the manifest bytes, the manifest covers every artifact by hash and size, and an artifact is
/// checked against its entry before it is opened.
public sealed record ReleaseManifest(
    int Schema,
    string Version,
    string? AgentVersion,
    IReadOnlyList<ReleaseArtifact> Artifacts,
    byte[] Bytes)
{
    public static ReleaseManifest? Parse(byte[] bytes)
    {
        var json = Value.Parse(Encoding.UTF8.GetString(bytes));
        if (!json.IsObject) return null;
        var version = json["version"].AsText();
        if (version is null) return null;
        var artifacts = new List<ReleaseArtifact>();
        foreach (var entry in json["artifacts"].AsArray())
        {
            var name = entry["name"].AsText();
            var sha = entry["sha256"].AsText();
            var size = entry["size"].AsLong();
            if (name is null || sha is null || size is null) return null;
            artifacts.Add(new ReleaseArtifact(name, sha.ToLowerInvariant(), size.Value));
        }
        return new ReleaseManifest(
            json["schema"].AsInt() ?? 1,
            version,
            json["agentVersion"].AsText(),
            artifacts,
            bytes);
    }

    /// The artifact with exactly this name. Never the first one that looks close: a client that
    /// picks the first zip in a release is a client that installs the wrong platform's build.
    public ReleaseArtifact? Artifact(string name) =>
        Artifacts.FirstOrDefault(artifact => artifact.Name == name);

    /// Whether these bytes are the artifact the manifest describes.
    public static bool Matches(ReleaseArtifact artifact, byte[] bytes) =>
        bytes.LongLength == artifact.Size
        && Convert.ToHexString(SHA256.HashData(bytes)).Equals(artifact.Sha256, StringComparison.OrdinalIgnoreCase);
}

/// What a verification concluded. Deliberately three states: a signature that could not be checked
/// because this build has no key is not the same as one that failed, and neither is "fine".
public abstract record TrustVerdict
{
    public sealed record Trusted(ReleaseManifest Manifest) : TrustVerdict;
    public sealed record Untrusted(string Sentence) : TrustVerdict;
    public sealed record CannotCheck(string Sentence) : TrustVerdict;

    public string Sentence_ => this switch
    {
        Trusted trusted => $"Signed manifest for {trusted.Manifest.Version} verified.",
        Untrusted untrusted => untrusted.Sentence,
        CannotCheck cannotCheck => cannotCheck.Sentence,
        _ => "",
    };

    /// Verifies a manifest and its detached signature against this build's key.
    public static TrustVerdict Check(byte[] manifestBytes, string signature)
    {
        if (!Trust.HasKey)
        {
            return new CannotCheck("This build carries no release trust key, so nothing signed can be verified or installed.");
        }
        if (!Trust.Verify(manifestBytes, signature))
        {
            return new Untrusted("The manifest signature does not verify against this build's release key. Nothing was installed.");
        }
        var manifest = ReleaseManifest.Parse(manifestBytes);
        if (manifest is null)
        {
            return new Untrusted("The manifest signature verified but the manifest itself is not the shape this build understands.");
        }
        return new Trusted(manifest);
    }
}
