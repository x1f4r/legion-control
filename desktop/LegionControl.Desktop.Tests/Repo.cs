using System.Text;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Tests;

/// Where the shared material lives, found from wherever the test host happens to run.
///
/// The fixtures, the schemas, the hash vectors and the golden trust manifest are shared with the
/// agent and the two native clients. Reading them from the repository rather than copying them into
/// this project is the point: a fixture that changes shape has to break a test here, or it is not a
/// shared contract at all.
public static class Repo
{
    public static string Root { get; } = Find();

    public static string Fixtures => Path.Combine(Root, "contract", "fixtures");

    public static string HashVectors => Path.Combine(Root, "contract", "hash-vectors.json");

    public static string TrustFixtures => Path.Combine(Root, "tests", "fixtures", "trust");

    public static bool HasFixtures => Directory.Exists(Fixtures);

    public static string ReadFixture(string name) => File.ReadAllText(Path.Combine(Fixtures, name));

    private static string Find()
    {
        var directory = new DirectoryInfo(AppContext.BaseDirectory);
        while (directory is not null)
        {
            if (Directory.Exists(Path.Combine(directory.FullName, "contract"))
                && File.Exists(Path.Combine(directory.FullName, "contract", "release-public-key.pem")))
            {
                return directory.FullName;
            }
            directory = directory.Parent;
        }
        throw new DirectoryNotFoundException("The repository root could not be found from " + AppContext.BaseDirectory);
    }
}

/// A temporary state directory that every path this app writes to is moved into.
///
/// The whole point of the LEGION_CONTROL_HOME override is that a test cannot touch the profile of
/// whoever is running it: not the setup document, not the operations list, and not ~/.ssh.
public sealed class TempHome : IDisposable
{
    private readonly string? _previousHome;
    private readonly string? _previousConfig;

    public string Path { get; }

    public TempHome()
    {
        Path = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "legion-desktop-test-" + Guid.NewGuid().ToString("n"));
        Directory.CreateDirectory(Path);
        _previousHome = Environment.GetEnvironmentVariable(AppPaths.HomeOverride);
        _previousConfig = Environment.GetEnvironmentVariable(AppPaths.ConfigOverride);
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, Path);
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
    }

    public string ConfigPath => System.IO.Path.Combine(Path, "config.json");

    public string Write(string name, string contents)
    {
        var path = System.IO.Path.Combine(Path, name);
        Directory.CreateDirectory(System.IO.Path.GetDirectoryName(path)!);
        File.WriteAllText(path, contents);
        return path;
    }

    public void Dispose()
    {
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, _previousHome);
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, _previousConfig);
        try
        {
            Directory.Delete(Path, recursive: true);
        }
        catch (Exception)
        {
        }
    }
}

/// An ssh that answers from a script instead of from a machine.
///
/// Every decision in the transport is made from what a command printed and what it exited with, so
/// a runner that says those two things exercises the real code paths: the same classification, the
/// same retry rules and the same decoding as a real machine, without one.
public sealed class FakeRunner : IProcessRunner
{
    private readonly Func<FakeCall, CommandResult> _answer;

    public FakeRunner(Func<FakeCall, CommandResult> answer) => _answer = answer;

    /// Answers every call with one JSON reply.
    public static FakeRunner Replying(string json) => new(_ => Ok(json));

    public List<FakeCall> Calls { get; } = new();

    public Task<CommandResult> RunAsync(
        string executable, IReadOnlyList<string> arguments, TimeSpan timeout, byte[]? input = null,
        CancellationToken cancellationToken = default)
    {
        var call = new FakeCall(executable, arguments, input);
        Calls.Add(call);
        return Task.FromResult(_answer(call));
    }

    public static CommandResult Ok(string stdout) => new() { ExitCode = 0, StandardOutput = stdout };

    public static CommandResult SshFailure(string stderr) => new() { ExitCode = 255, StandardError = stderr };

    public static CommandResult TimedOutResult() => new() { ExitCode = -1, TimedOut = true };

    public static CommandResult Text(string stdout, string stderr = "", int exitCode = 0) =>
        new() { ExitCode = exitCode, StandardOutput = stdout, StandardError = stderr };
}

public sealed record FakeCall(string Executable, IReadOnlyList<string> Arguments, byte[]? Input)
{
    /// The one string ssh was given: the whole remote command line, after the options.
    public string RemoteCommand => Arguments.Count == 0 ? "" : Arguments[^1];

    public string InputText => Input is null ? "" : Encoding.UTF8.GetString(Input);

    public bool Has(string option) => Arguments.Contains(option);

    public string? After(string option)
    {
        var index = Arguments.ToList().IndexOf(option);
        return index >= 0 && index + 1 < Arguments.Count ? Arguments[index + 1] : null;
    }
}
