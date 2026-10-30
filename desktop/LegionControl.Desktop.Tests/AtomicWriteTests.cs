using LegionControl.Desktop.Config;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// Every file this app keeps is replaced in one rename, and on Windows that rename has to outlast
/// whoever is reading the file at that moment.
public class AtomicWriteTests
{
    /// A reader holding the file open without delete sharing, as the other copy of the app, the
    /// search indexer or an antivirus scanner can, makes Windows refuse the rename until it lets go.
    [WindowsFact("only Windows refuses a rename over a file another process has open")]
    public async Task AWriteWaitsForAReaderThatHoldsTheFile()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "settings.json");
        AtomicWrite.Text(path, "first");
        var reader = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
        var released = Task.Run(async () =>
        {
            await Task.Delay(TimeSpan.FromMilliseconds(500));
            reader.Dispose();
        });

        AtomicWrite.Text(path, "second");
        await released;

        Assert.Equal("second", File.ReadAllText(path));
        Assert.Single(Directory.GetFiles(home.Path, "settings.json*"));
    }

    /// One that never lets go is not waited for forever, and the file it holds stays as it was.
    [WindowsFact("only Windows refuses a rename over a file another process has open")]
    public void AWriteOutlastedByAReaderFailsAndLeavesTheFileAsItWas()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "settings.json");
        AtomicWrite.Text(path, "first");
        using var reader = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);

        var clock = System.Diagnostics.Stopwatch.StartNew();
        Assert.ThrowsAny<Exception>(() => AtomicWrite.Text(path, "second"));
        Assert.InRange(clock.Elapsed, TimeSpan.FromSeconds(2), TimeSpan.FromSeconds(20));

        reader.Dispose();
        Assert.Equal("first", File.ReadAllText(path));
        Assert.Single(Directory.GetFiles(home.Path, "settings.json*"));
    }
}
