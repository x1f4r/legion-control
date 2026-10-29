using System.Net;
using System.Text;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Updates;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// What this build reads from a release, and how much of it. The repository is one this device
/// approved, but whoever owns it decides what its release holds, so every part of it is read only
/// as far as a real one could need and refused past that.
public class ReleaseDownloadTests
{
    private const string Repository = "owner/repo";
    private const string ReleaseList = "https://api.github.com/repos/owner/repo/releases/latest";
    private const string ManifestUrl = "https://example.invalid/manifest.json";
    private const string SignatureUrl = "https://example.invalid/manifest.json.sig";

    /// A release host that serves what a test put at each address and records what was asked.
    private sealed class Host : HttpMessageHandler
    {
        public Dictionary<string, Func<HttpContent>> Bodies { get; } = new(StringComparer.Ordinal);
        public List<string> Asked { get; } = new();

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            var url = request.RequestUri!.ToString();
            Asked.Add(url);
            return Task.FromResult(Bodies.TryGetValue(url, out var body)
                ? new HttpResponseMessage(HttpStatusCode.OK) { Content = body() }
                : new HttpResponseMessage(HttpStatusCode.NotFound));
        }
    }

    /// A body that says nothing about its length up front, as a chunked response does.
    private sealed class Unannounced(byte[] bytes, long? announced = null) : HttpContent
    {
        protected override Task SerializeToStreamAsync(Stream stream, TransportContext? context) => stream.WriteAsync(bytes).AsTask();

        protected override bool TryComputeLength(out long length)
        {
            length = announced ?? 0;
            return announced is not null;
        }
    }

    private static byte[] Manifest() => File.ReadAllBytes(Path.Combine(Repo.TrustFixtures, "manifest.json"));

    private static byte[] Signature() => File.ReadAllBytes(Path.Combine(Repo.TrustFixtures, "manifest.json.sig"));

    /// A release whose list names a manifest, its signature and this platform's archive.
    private static Host ARelease(Func<HttpContent>? manifest = null, Func<HttpContent>? signature = null, Func<HttpContent>? list = null)
    {
        var host = new Host();
        var assets = new JsonArray(
            new JsonObject { ["name"] = AppUpdates.ManifestAsset, ["browser_download_url"] = ManifestUrl },
            new JsonObject { ["name"] = AppUpdates.SignatureAsset, ["browser_download_url"] = SignatureUrl },
            new JsonObject { ["name"] = AppUpdates.AssetForThisPlatform() ?? "none", ["browser_download_url"] = "https://example.invalid/app" });
        var release = Encoding.UTF8.GetBytes(new JsonObject { ["tag_name"] = "v9.0.0", ["assets"] = assets }.ToJsonString());
        host.Bodies[ReleaseList] = list ?? (() => new Unannounced(release));
        host.Bodies[ManifestUrl] = manifest ?? (() => new Unannounced(Manifest()));
        host.Bodies[SignatureUrl] = signature ?? (() => new Unannounced(Signature()));
        return host;
    }

    private static async Task<string> Check(Host host, string repository = Repository) =>
        (await new AppUpdates(new HttpClient(host)).CheckAsync(repository, "1.0.0")).Sentence_;

    /// Whether this platform has an update to check at all. The checks below are about reading a
    /// release, which a platform with no archive of its own never starts.
    private static bool HasAnUpdate => AppUpdates.AssetForThisPlatform() is not null;

    [Theory]
    [InlineData("../../users/me")]
    [InlineData("owner/..")]
    [InlineData("owner/repo/releases")]
    [InlineData("-owner/repo")]
    [InlineData("owner/re po")]
    [InlineData("owner/repo\n")]
    [InlineData("owner")]
    public async Task ARepositoryGitHubCouldNotNameIsNeverAsked(string repository)
    {
        var host = ARelease();
        Assert.Contains("owner/name", await Check(host, repository));
        Assert.Empty(host.Asked);
    }

    [Fact]
    public async Task AReleaseListLargerThanAnyRealOneIsNotRead()
    {
        if (!HasAnUpdate) return;
        var huge = new byte[AppUpdates.ReleaseLimit + 1];
        Assert.Contains("larger than 1048576 bytes", await Check(ARelease(list: () => new Unannounced(huge))));
        // Refused on what the response announced, before a byte of the body is read.
        Assert.Contains("larger than 1048576 bytes", await Check(ARelease(list: () => new Unannounced(Array.Empty<byte>(), 2 * AppUpdates.ReleaseLimit))));
    }

    [Fact]
    public async Task AManifestOrSignatureLargerThanTheReleaseToolingSignsIsNotRead()
    {
        if (!HasAnUpdate) return;
        var manifest = await Check(ARelease(manifest: () => new Unannounced(new byte[AppUpdates.ManifestLimit + 1])));
        Assert.Contains("The signed manifest could not be read: it is larger than 262144 bytes", manifest);
        var signature = await Check(ARelease(signature: () => new Unannounced(new byte[AppUpdates.SignatureLimit + 1])));
        Assert.Contains("The signed manifest could not be read: it is larger than 4096 bytes", signature);
        // At the limits themselves the release is read, and the fixture names no archive for here.
        Assert.Contains("has no", await Check(ARelease()));
    }

    [Fact]
    public void AManifestOfAnyOtherSchemaIsNotReadAsThisOne()
    {
        var text = Encoding.UTF8.GetString(Manifest());
        Assert.NotNull(ReleaseManifest.Parse(Manifest()));
        Assert.Null(ReleaseManifest.Parse(Encoding.UTF8.GetBytes(text.Replace("\"schema\": 1", "\"schema\": 2"))));
        Assert.Null(ReleaseManifest.Parse(Encoding.UTF8.GetBytes(text.Replace("\"schema\": 1", "\"schema\": \"1\""))));
        Assert.Null(ReleaseManifest.Parse(Encoding.UTF8.GetBytes(text.Replace("\"schema\": 1,", ""))));
    }

    /// The fixture's own artifact, offered at an address this host serves [body] from.
    private static (AppUpdates Updates, UpdateAvailability.Ready Ready, Host Host) Offer(Func<HttpContent> body, long? size = null)
    {
        var manifest = ReleaseManifest.Parse(Manifest())!;
        var artifact = manifest.Artifact("fixture.bin")!;
        if (size is not null) artifact = artifact with { Size = size.Value };
        var host = new Host();
        host.Bodies["https://example.invalid/fixture.bin"] = body;
        return (new AppUpdates(new HttpClient(host)), new UpdateAvailability.Ready(manifest, artifact, "https://example.invalid/fixture.bin"), host);
    }

    private static IEnumerable<string> Leftovers(string directory) =>
        Directory.Exists(directory) ? Directory.EnumerateFiles(directory).Select(Path.GetFileName).OfType<string>() : Array.Empty<string>();

    [Fact]
    public async Task TheArchiveIsStreamedToDiskAndKeptOnlyWhenItIsWhatTheManifestSigned()
    {
        using var home = new TempHome();
        var destination = Path.Combine(home.Path, "updates", "fixture.bin");

        var (updates, ready, _) = Offer(() => new Unannounced("abc"u8.ToArray()));
        Assert.Null(await updates.DownloadAsync(ready, destination));
        Assert.Equal("abc", File.ReadAllText(destination));
        Assert.Equal(new[] { "fixture.bin" }, Leftovers(Path.GetDirectoryName(destination)!));
        File.Delete(destination);

        (updates, ready, _) = Offer(() => new Unannounced("abd"u8.ToArray()));
        Assert.Contains("does not match the signed manifest", await updates.DownloadAsync(ready, destination));
        Assert.Empty(Leftovers(Path.GetDirectoryName(destination)!));
    }

    [Fact]
    public async Task AnArchiveLargerThanTheManifestSaysIsStoppedAndNothingOfItKept()
    {
        using var home = new TempHome();
        var destination = Path.Combine(home.Path, "updates", "fixture.bin");

        // Streamed with no length said: it stops at the first byte past the signed size.
        var (updates, ready, _) = Offer(() => new Unannounced(new byte[10 * 1024 * 1024]));
        Assert.Contains("larger than the 3 bytes the signed manifest says", await updates.DownloadAsync(ready, destination));
        Assert.Empty(Leftovers(Path.GetDirectoryName(destination)!));

        // With a length said up front, it is refused before any of it is written.
        (updates, ready, _) = Offer(() => new Unannounced("abc"u8.ToArray(), 1_000_000_000));
        Assert.Contains("more than the 3 the signed manifest says", await updates.DownloadAsync(ready, destination));
        Assert.Empty(Leftovers(Path.GetDirectoryName(destination)!));
    }

    [Fact]
    public async Task AnArtifactTheManifestGivesNoSizeIsNotDownloaded()
    {
        using var home = new TempHome();
        var (updates, ready, host) = Offer(() => new Unannounced("abc"u8.ToArray()), size: 0);
        Assert.Contains("no size", await updates.DownloadAsync(ready, Path.Combine(home.Path, "fixture.bin")));
        Assert.Empty(host.Asked);
    }
}
