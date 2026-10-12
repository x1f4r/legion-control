using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class ProcessRunnerTests
{
    [Fact]
    public async Task AgentReplyCompletesAfterAFullNewlineTerminatedObjectEvenWhenChildKeepsPipesOpen()
    {
        // The child never exits, so only the framing can end this run before the deadline does.
        var script = "process.stdout.write(JSON.stringify({ok:true,contract:3,data:'x'.repeat(12000)})+'\\n');setInterval(()=>{},1000);";
        var result = await new ProcessRunner().RunAgentAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(8));
        Assert.False(result.TimedOut);
        using var json = System.Text.Json.JsonDocument.Parse(result.StandardOutput);
        Assert.True(json.RootElement.GetProperty("ok").GetBoolean());
        Assert.Equal(12000, json.RootElement.GetProperty("data").GetString()!.Length);
    }

    [Theory]
    [InlineData("{\"ok\":true}")]
    [InlineData("{\"ok\":true,\n")]
    [InlineData("{\"progress\":true}\n")]
    [InlineData("banner {\"ok\":true}\n")]
    public async Task PartialOrUnframedJsonDoesNotCompleteAgentProtocol(string text)
    {
        var script = "process.stdout.write(" + System.Text.Json.JsonSerializer.Serialize(text) + ");setInterval(()=>{},1000);";
        var result = await new ProcessRunner().RunAgentAsync("node", new[] { "-e", script }, TimeSpan.FromMilliseconds(350));
        Assert.True(result.TimedOut);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task AnEmptyInputClosesStdinBeforeWaitingForTheChild(bool explicitEmptyInput)
    {
        // A blocking descriptor read tests the parent's EOF directly, without a child stream's
        // end-event scheduling or process.exit racing an asynchronous stdout write on Windows.
        var script = "const fs=require('node:fs');fs.writeSync(2,'reading stdin '+process.version+'\\n');" +
                     "const input=fs.readFileSync(0);fs.writeSync(1,'EOF received:'+input.length);";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(8),
            explicitEmptyInput ? Array.Empty<byte>() : null);
        Assert.True(result.Succeeded, $"{result.FailureText}\nstdout: {result.StandardOutput}\nstderr: {result.StandardError}");
        Assert.Equal("EOF received:0", result.StandardOutput);
        Assert.Contains("reading stdin v", result.StandardError);
    }

    [Fact]
    public async Task ARealChildTimeoutRetainsItsPartialOutput()
    {
        var script = "process.stdout.write('{\\\"ok\\\":true}\\n');process.stderr.write('still running');setInterval(()=>{},1000);";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromMilliseconds(350));
        Assert.True(result.TimedOut);
        Assert.Contains("ok", result.StandardOutput);
        Assert.Contains("still running", result.StandardError);
    }

    [Fact]
    public async Task ACallerCancellationIsReportedAsARunThisAppEndedNotAsAnOrdinaryResult()
    {
        // The kill leaves an exit status behind, and handing that back as a normal result made a
        // cancelled change look like one that had failed on the far side.
        //
        // The child says so on stdout and then leaves a file behind, and the run is cancelled only
        // once that file is there. A timer could fire before a slow machine had even started node,
        // and a run cancelled before it starts is a different rule (the test below).
        var marker = Path.Combine(Path.GetTempPath(), $"legion-started-{Guid.NewGuid():N}");
        var script = $"const fs=require('fs');fs.writeSync(1,'started');fs.writeFileSync({System.Text.Json.JsonSerializer.Serialize(marker)},'');" +
                     "setInterval(()=>{},1000);";
        using var cancel = new CancellationTokenSource();
        var running = new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(60), null, cancel.Token);
        try
        {
            var waited = System.Diagnostics.Stopwatch.StartNew();
            while (!File.Exists(marker) && !running.IsCompleted && waited.Elapsed < TimeSpan.FromSeconds(30)) await Task.Delay(50);
            Assert.True(File.Exists(marker), "the child never started");
            cancel.Cancel();
            var result = await running;
            Assert.True(result.TimedOut);
            Assert.True(result.Cancelled);
            Assert.Contains("cancelled", result.FailureText);
            Assert.Contains("started", result.StandardOutput);
            var reading = AttemptClassifier.Classify(result);
            Assert.Equal(FailureKind.TimedOut, reading.Kind);
            Assert.Equal(Dispatch.Unknown, reading.Dispatch);
        }
        finally
        {
            cancel.Cancel();
            File.Delete(marker);
        }
    }

    [Fact]
    public async Task ACommandCancelledBeforeItStartsIsNeverStarted()
    {
        var marker = Path.Combine(Path.GetTempPath(), $"legion-never-started-{Guid.NewGuid():N}");
        using var cancel = new CancellationTokenSource();
        cancel.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => new ProcessRunner().RunAsync("node",
            new[] { "-e", $"require('fs').writeFileSync({System.Text.Json.JsonSerializer.Serialize(marker)},'ran')" },
            TimeSpan.FromSeconds(5), null, cancel.Token));
        Assert.False(File.Exists(marker));
    }

    [Fact]
    public async Task OutputPastTheCapIsDiscardedRatherThanKeptWithoutLimit()
    {
        var script = "process.stdout.write('{\"ok\":true}');process.stdout.write('x'.repeat(17*1024*1024));";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(30));
        Assert.True(result.Succeeded, result.FailureText);
        Assert.True(result.StandardOutputTruncated);
        Assert.Equal(16 * 1024 * 1024, result.StandardOutput.Length);
        // The kept part starts with what looks like a whole reply, and it is still not one.
        Assert.False(AttemptClassifier.Classify(result).IsReply);
    }

    [Fact]
    public async Task TheCapCountsTheBytesThePipeDeliveredNotTheCharactersTheyDecodeTo()
    {
        // Every other client counts bytes. Nine Mi two-byte characters are 18 MiB, so they are cut
        // off at 16 MiB here too, rather than kept whole for being fewer than 16 Mi characters.
        var script = "process.stdout.write('\\u00e9'.repeat(9*1024*1024));";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(30));
        Assert.True(result.Succeeded, result.FailureText);
        Assert.True(result.StandardOutputTruncated);
        // How many characters that is depends on the encoding this platform's reader uses, which on
        // Windows may be a one-byte code page.
        if (!OperatingSystem.IsWindows()) Assert.Equal(8 * 1024 * 1024, result.StandardOutput.Length);
    }

    [Fact]
    public async Task AReplyInTwoByteTextIsFramedAcrossReadsThatSplitItsCharacters()
    {
        // 4096-byte reads of an odd-length prefix and two-byte characters split one of them on every
        // read. The framing decodes statefully, so the reply still completes as soon as it is whole;
        // the child never exits, so a reply the framing missed ends in the deadline instead.
        var script = "process.stdout.write(JSON.stringify({ok:true,data:'\\u00e9'.repeat(12000)})+'\\n');setInterval(()=>{},1000);";
        var result = await new ProcessRunner().RunAgentAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(8));
        Assert.False(result.TimedOut);
        using var json = System.Text.Json.JsonDocument.Parse(result.StandardOutput);
        Assert.True(json.RootElement.GetProperty("ok").GetBoolean());
    }

    // The grandchild's EPIPE is how this test sees this side's end of the pipe close. Windows
    // reports a write to a closed pipe differently, so there it is reported as skipped rather than
    // passing without having looked. The rule is the same code path on both.
    [UnixFact("The closed pipe is observed through a POSIX EPIPE in a grandchild, which Windows does not report the same way.")]
    public async Task ThePipesAreClosedEvenWhenAGrandchildStillHoldsThem()
    {
        var marker = Path.Combine(Path.GetTempPath(), $"legion-pipe-closed-{Guid.NewGuid():N}");
        var markerJson = System.Text.Json.JsonSerializer.Serialize(marker);
        // The child leaves behind a grandchild that inherited its stdout and keeps writing to it.
        // Once this side has closed its end, a write fails, and the grandchild records why.
        var grandchild = "const fs=require('fs');let n=0;const t=setInterval(()=>{try{fs.writeSync(1,'tick\\n')}" +
                         $"catch(e){{fs.writeFileSync({markerJson},e.code);clearInterval(t)}}if(++n>80)clearInterval(t)}},100);";
        var child = "require('child_process').spawn(process.execPath,['-e'," + System.Text.Json.JsonSerializer.Serialize(grandchild) +
                    "],{stdio:['ignore','inherit','inherit'],detached:true}).unref();";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", child }, TimeSpan.FromSeconds(10));
        Assert.True(result.Succeeded, result.FailureText);

        var waited = System.Diagnostics.Stopwatch.StartNew();
        while (!File.Exists(marker) && waited.Elapsed < TimeSpan.FromSeconds(5)) await Task.Delay(100);
        try
        {
            Assert.True(File.Exists(marker), "the grandchild could still write after the run returned");
            Assert.Equal("EPIPE", File.ReadAllText(marker));
        }
        finally
        {
            File.Delete(marker);
        }
    }

    [Fact]
    public async Task LargeInputAndBothOutputsDrainWithoutADeadlock()
    {
        var script = "let n=0;process.stdin.on('data',x=>{n+=x.length;process.stdout.write('x'.repeat(8192));process.stderr.write('y'.repeat(8192));});process.stdin.on('end',()=>process.stdout.write('bytes='+n));";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(8), new byte[200000]);
        Assert.True(result.Succeeded, result.FailureText);
        Assert.Contains("bytes=200000", result.StandardOutput);
        Assert.Contains("yyyy", result.StandardError);
    }
}

/// A fact only Unix can observe. Reported as skipped on Windows, with the reason, rather than
/// counted as a pass there.
public sealed class UnixFactAttribute : FactAttribute
{
    public UnixFactAttribute(string reason)
    {
        if (OperatingSystem.IsWindows()) Skip = reason;
    }
}
