using System.Text;
using LegionControl.Desktop;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The signature check, against a manifest signed with the key this build actually carries.
///
/// This is the test that catches the cross-language mistakes nothing else would: an SPKI header
/// left on the key bytes, a base64 signature read with the wrong padding, a verifier fed the
/// re-serialised manifest instead of the bytes that were signed. Every one of those produces a
/// verifier that says yes to everything or no to everything, and both are silent.
public class TrustTests
{
    private static byte[] Manifest() => File.ReadAllBytes(Path.Combine(Repo.TrustFixtures, "manifest.json"));

    private static string Signature() => File.ReadAllText(Path.Combine(Repo.TrustFixtures, "manifest.json.sig"));

    [Fact]
    public void ThisBuildCarriesTheReleaseKey()
    {
        Assert.True(Trust.HasKey, "the release public key is embedded from contract/release-public-key.pem");
        Assert.NotNull(Trust.KeyFingerprint);
    }

    [Fact]
    public void VerifiesTheGoldenManifest()
    {
        Assert.True(Trust.Verify(Manifest(), Signature()));
    }

    [Fact]
    public void RefusesTheGoldenManifestWithOneByteChanged()
    {
        var tampered = Manifest();
        // The last byte of the document rather than a field, so the change is as small as a change
        // can be and still be one.
        tampered[^2] = tampered[^2] == (byte)' ' ? (byte)'\t' : (byte)' ';
        Assert.False(Trust.Verify(tampered, Signature()));
    }

    [Fact]
    public void RefusesAChangedVersionInTheManifest()
    {
        var text = Encoding.UTF8.GetString(Manifest()).Replace("\"1.3.0\"", "\"1.3.1\"");
        Assert.False(Trust.Verify(Encoding.UTF8.GetBytes(text), Signature()));
    }

    [Fact]
    public void RefusesASignatureThatIsNotOne()
    {
        Assert.False(Trust.Verify(Manifest(), "not base64 at all"));
        Assert.False(Trust.Verify(Manifest(), Convert.ToBase64String(new byte[64])));
        // Right length, wrong bytes.
        var flipped = Convert.FromBase64String(Signature().Trim());
        flipped[0] ^= 0x01;
        Assert.False(Trust.Verify(Manifest(), Convert.ToBase64String(flipped)));
    }

    [Fact]
    public void ReadsTheManifestOnlyAfterTheSignatureVerifies()
    {
        var verdict = TrustVerdict.Check(Manifest(), Signature());
        var trusted = Assert.IsType<TrustVerdict.Trusted>(verdict);
        Assert.Equal("1.3.0", trusted.Manifest.Version);
        Assert.Equal("3.0.0", trusted.Manifest.AgentVersion);

        var artifact = trusted.Manifest.Artifact("fixture.bin");
        Assert.NotNull(artifact);
        Assert.Equal(3, artifact!.Size);

        // The fixture artifact is "abc", and the manifest is what says so.
        Assert.True(ReleaseManifest.Matches(artifact, "abc"u8.ToArray()));
        Assert.False(ReleaseManifest.Matches(artifact, "abd"u8.ToArray()));
        // Right hash, wrong length: both are checked, because a truncated download can still
        // collide with nothing at all if only one of them is.
        Assert.False(ReleaseManifest.Matches(artifact with { Size = 4 }, "abc"u8.ToArray()));
    }

    [Fact]
    public void AnUntrustedManifestIsNotTheSameAsOneThatCannotBeChecked()
    {
        var tampered = Manifest();
        tampered[^2] = (byte)'\t';
        Assert.IsType<TrustVerdict.Untrusted>(TrustVerdict.Check(tampered, Signature()));
    }

    [Fact]
    public void PicksTheAssetByExactNameAndNeverByShape()
    {
        var verdict = TrustVerdict.Check(Manifest(), Signature());
        var manifest = Assert.IsType<TrustVerdict.Trusted>(verdict).Manifest;
        Assert.Null(manifest.Artifact("Legion-Control-linux-x64.tar.gz"));
        Assert.Null(manifest.Artifact("fixture"));
        Assert.NotNull(manifest.Artifact("fixture.bin"));
    }
}
