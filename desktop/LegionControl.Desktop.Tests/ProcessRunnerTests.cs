using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class ProcessRunnerTests
{
    [Fact]
    public async Task AgentReplyCompletesAfterAFullNewlineTerminatedObjectEvenWhenChildKeepsPipesOpen()
    {
        var script = "process.stdout.write(JSON.stringify({ok:true,contract:3,data:'x'.repeat(12000)})+'\\n');setInterval(()=>{},1000);";
        var watch = System.Diagnostics.Stopwatch.StartNew();
        var result = await new ProcessRunner().RunAgentAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(8));
        Assert.False(result.TimedOut);
        Assert.True(watch.Elapsed < TimeSpan.FromSeconds(4));
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
    public async Task LargeInputAndBothOutputsDrainWithoutADeadlock()
    {
        var script = "let n=0;process.stdin.on('data',x=>{n+=x.length;process.stdout.write('x'.repeat(8192));process.stderr.write('y'.repeat(8192));});process.stdin.on('end',()=>process.stdout.write('bytes='+n));";
        var result = await new ProcessRunner().RunAsync("node", new[] { "-e", script }, TimeSpan.FromSeconds(8), new byte[200000]);
        Assert.True(result.Succeeded, result.FailureText);
        Assert.Contains("bytes=200000", result.StandardOutput);
        Assert.Contains("yyyy", result.StandardError);
    }
}
