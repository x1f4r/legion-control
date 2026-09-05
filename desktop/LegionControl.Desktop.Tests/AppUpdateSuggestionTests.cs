using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using System.Text.Json.Nodes;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class AppUpdateSuggestionTests
{
    [Fact]
    public async Task ConfigurationEditImmediatelyClearsTheOfferAndInvalidatesAnOpenReview()
    {
        using var home = new TempHome();
        var root = JsonNode.Parse(Documents.Make("setup-a", 3, []).Text)!;
        root["appUpdates"] = new JsonObject { ["githubRepo"] = "old/repo" };
        File.WriteAllText(home.ConfigPath, root.ToJsonString());
        var store = new ConfigStore(home.ConfigPath, watch: false);
        var monitor = new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(Offer()));
        using var app = new AppModel(configStore: store, updateSuggestions: monitor);
        var ready = Assert.IsType<UpdateAvailability.Ready>(await app.CheckForAppUpdateAsync());
        var review = Assert.IsType<AppUpdateReview>(app.BeginAppUpdateReview(ready));
        Assert.True(app.IsCurrentAppUpdateReview(review));
        var (_, problem) = store.ApplyEdit(store.Document!.Text.Replace("old/repo", "new/repo"));
        Assert.Null(problem);
        Assert.Null(app.AppUpdate);
        Assert.Null(app.BeginAppUpdateReview(ready));
        Assert.False(app.IsCurrentAppUpdateReview(review));
    }

    [Fact]
    public async Task ReturningToTheSameRepositoryCannotReviveAnEarlierReviewOrLateRequest()
    {
        var late = new TaskCompletionSource<UpdateAvailability>();
        var calls = 0;
        var monitor = new AppUpdateSuggestions((_, _) => ++calls == 2 ? late.Task : Task.FromResult<UpdateAvailability>(Offer()));
        var ready = Assert.IsType<UpdateAvailability.Ready>(await monitor.CheckAsync("old/repo"));
        var review = Assert.IsType<AppUpdateReview>(monitor.BeginReview("old/repo", ready));
        var oldRequest = monitor.CheckAsync("old/repo", force: true);
        monitor.SelectRepository("new/repo");
        Assert.Null(monitor.Availability);
        monitor.SelectRepository("old/repo");
        late.SetResult(Offer("9.0.0"));
        await oldRequest;
        Assert.Null(monitor.Availability);
        Assert.False(monitor.IsCurrent(review, "old/repo"));
        var fresh = Assert.IsType<UpdateAvailability.Ready>(await monitor.CheckAsync("old/repo"));
        Assert.NotNull(monitor.BeginReview("old/repo", fresh));
        Assert.False(monitor.IsCurrent(review, "old/repo"));
    }

    [Fact]
    public async Task ReviewSurvivesOfflineChecksButFailsEveryBoundaryAfterRepositoryChange()
    {
        UpdateAvailability reply = Offer();
        var monitor = new AppUpdateSuggestions((_, _) => Task.FromResult(reply));
        var ready = Assert.IsType<UpdateAvailability.Ready>(await monitor.CheckAsync("old/repo"));
        var review = Assert.IsType<AppUpdateReview>(monitor.BeginReview("old/repo", ready));
        reply = new UpdateAvailability.Unavailable("Offline");
        await monitor.CheckAsync("old/repo", force: true);
        Assert.True(monitor.IsCurrent(review, "old/repo"));
        monitor.SelectRepository("new/repo");
        Assert.False(monitor.IsCurrent(review, "new/repo"));
        Assert.False(monitor.IsCurrent(review, "old/repo"));
    }

    private static UpdateAvailability.Ready Offer(string version = "2.0.0")
    {
        var artifact = new ReleaseArtifact(AppUpdates.WindowsAsset, new string('a', 64), 3);
        return new(new ReleaseManifest(1, version, null, [artifact], []), artifact, "https://example.invalid/app.zip");
    }

    [Fact]
    public async Task StartupChecksImmediatelyAndRepeatedForegroundEventsAreThrottled()
    {
        var now = DateTimeOffset.UtcNow;
        var calls = 0;
        var monitor = new AppUpdateSuggestions((_, _) =>
        {
            calls++;
            return Task.FromResult<UpdateAvailability>(new UpdateAvailability.UpToDate("1.3.0"));
        }, () => now);
        await monitor.CheckAsync("owner/repo");
        now += TimeSpan.FromMinutes(14);
        await monitor.CheckAsync("owner/repo");
        Assert.Equal(1, calls);
        now += TimeSpan.FromMinutes(1);
        await monitor.CheckAsync("owner/repo");
        Assert.Equal(2, calls);
        now += AppUpdateSuggestions.PeriodicInterval;
        await monitor.CheckAsync("owner/repo");
        Assert.Equal(3, calls);
    }

    [Fact]
    public async Task StartupForegroundAndManualCheckShareAnInFlightRequest()
    {
        var reply = new TaskCompletionSource<UpdateAvailability>();
        var calls = 0;
        var monitor = new AppUpdateSuggestions((_, _) => { calls++; return reply.Task; });
        var startup = monitor.CheckAsync("owner/repo");
        var foreground = monitor.CheckAsync("owner/repo");
        var manual = monitor.CheckAsync("owner/repo", force: true);
        Assert.Same(startup, foreground);
        Assert.Same(startup, manual);
        reply.SetResult(Offer());
        await Task.WhenAll(startup, foreground, manual);
        Assert.Equal(1, calls);
    }

    [Fact]
    public async Task TransientFailureKeepsTheVerifiedOfferButManualCheckReportsTheFailure()
    {
        UpdateAvailability reply = Offer();
        var monitor = new AppUpdateSuggestions((_, _) => Task.FromResult(reply));
        var verified = await monitor.CheckAsync("owner/repo");
        reply = new UpdateAvailability.Unavailable("Offline");
        Assert.Same(reply, await monitor.CheckAsync("owner/repo", force: true));
        Assert.Same(reply, monitor.LastResult);
        Assert.Same(verified, monitor.Availability);
        reply = new UpdateAvailability.UpToDate("2.0.0");
        await monitor.CheckAsync("owner/repo", force: true);
        Assert.Same(reply, monitor.Availability);
    }

    [Fact]
    public async Task ANewVerifiedReleaseReplacesTheEarlierOffer()
    {
        UpdateAvailability reply = Offer();
        var monitor = new AppUpdateSuggestions((_, _) => Task.FromResult(reply));
        await monitor.CheckAsync("owner/repo");
        reply = Offer("2.1.0");
        await monitor.CheckAsync("owner/repo", force: true);
        Assert.Same(reply, monitor.Availability);
    }

    [Fact]
    public async Task RepositoryChangeClearsTheOldOfferAndIgnoresItsLateAnswer()
    {
        var oldReply = new TaskCompletionSource<UpdateAvailability>();
        var newReply = new TaskCompletionSource<UpdateAvailability>();
        var monitor = new AppUpdateSuggestions((repo, _) => repo == "old/repo" ? oldReply.Task : newReply.Task);
        var oldRequest = monitor.CheckAsync("old/repo");
        var newRequest = monitor.CheckAsync("new/repo");
        var expected = Offer("3.0.0");
        newReply.SetResult(expected);
        await newRequest;
        oldReply.SetResult(Offer());
        await oldRequest;
        Assert.Same(expected, monitor.Availability);
    }

    [Fact]
    public async Task LateOldAnswerCannotClearTheNewRepositorysInFlightCheck()
    {
        var oldReply = new TaskCompletionSource<UpdateAvailability>();
        var newReply = new TaskCompletionSource<UpdateAvailability>();
        var monitor = new AppUpdateSuggestions((repo, _) => repo == "old/repo" ? oldReply.Task : newReply.Task);
        var oldRequest = monitor.CheckAsync("old/repo");
        monitor.SelectRepository("new/repo");
        var newRequest = monitor.CheckAsync("new/repo");
        oldReply.SetResult(Offer());
        await oldRequest;
        Assert.Null(monitor.Availability);
        Assert.Same(newRequest, monitor.CheckAsync("new/repo", force: true));
        var expected = Offer("3.0.0");
        newReply.SetResult(expected);
        await newRequest;
        Assert.Same(expected, monitor.Availability);
    }

    [Fact]
    public async Task RepositoryChangeCannotRetainAnOfferFromAnotherSource()
    {
        var monitor = new AppUpdateSuggestions((repo, _) => Task.FromResult<UpdateAvailability>(
            repo == "old/repo" ? Offer() : new UpdateAvailability.Unavailable("Offline")));
        await monitor.CheckAsync("old/repo");
        await monitor.CheckAsync("new/repo");
        Assert.IsType<UpdateAvailability.Unavailable>(monitor.Availability);
    }

    [Fact]
    public async Task CancellationEndsTheRequestAndAllowsAnExplicitRetry()
    {
        var calls = 0;
        var monitor = new AppUpdateSuggestions(async (_, token) =>
        {
            if (++calls == 1) await Task.Delay(Timeout.InfiniteTimeSpan, token);
            return Offer();
        });
        using var source = new CancellationTokenSource();
        var first = monitor.CheckAsync("owner/repo", cancellationToken: source.Token);
        source.Cancel();
        Assert.IsType<UpdateAvailability.Unavailable>(await first);
        Assert.IsType<UpdateAvailability.Ready>(await monitor.CheckAsync("owner/repo", force: true));
    }
}
