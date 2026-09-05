using System.Text;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Contract;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class ClientVersionTests
{
    [Fact]
    public async Task ReportedVersionMatchesTheCompiledAssemblyAndEmbeddedWindowsManifest()
    {
        var assembly = typeof(AgentContract).Assembly;
        var version = assembly.GetName().Version!;
        Assert.Equal(version.ToString(3), AgentContract.ClientVersion);
        var output = new StringWriter();
        Assert.Equal(Runner.Ok, await Runner.RunAsync(["--version"], output));
        Assert.Contains($"Legion Control {version.ToString(3)}", output.ToString());

        // Check the manifest the compiler actually embedded, not the unexpanded source template.
        var image = Encoding.UTF8.GetString(File.ReadAllBytes(assembly.Location));
        Assert.Contains($"assemblyIdentity version=\"{version}\" name=\"LegionControl.Desktop\"", image);
    }
}
