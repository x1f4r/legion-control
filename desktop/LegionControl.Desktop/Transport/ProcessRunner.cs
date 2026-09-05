using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace LegionControl.Desktop.Transport;

public sealed record CommandResult
{
    public int ExitCode { get; init; }
    public string StandardOutput { get; init; } = "";
    public string StandardError { get; init; } = "";
    /// Our own watchdog fired. The far side may well still be working, so this is never read as
    /// either success or failure anywhere above this line.
    public bool TimedOut { get; init; }
    public string? LaunchFailure { get; init; }

    public bool Succeeded => ExitCode == 0 && !TimedOut && LaunchFailure is null;

    /// The most useful line to show a human. Both ssh and node bury the one sentence that matters
    /// under a routing banner, stack frames and caret art, so the lines carrying no message go
    /// first.
    public string FailureText
    {
        get
        {
            if (LaunchFailure is not null) return LaunchFailure;
            if (TimedOut) return "The command did not finish in time.";
            var fromError = InformativeLines(StandardError);
            if (fromError.Length > 0) return fromError;
            var fromOutput = InformativeLines(StandardOutput);
            if (fromOutput.Length > 0) return fromOutput;
            return $"Exit code {ExitCode}.";
        }
    }

    private static string InformativeLines(string text) => string.Join("\n", text
        .Split('\n')
        // Trim newlines too: Windows sends CRLF and the carriage return would survive.
        .Select(line => line.Trim())
        .Where(line =>
        {
            if (line.Length == 0) return false;
            // Node prints one frame per line after the message itself.
            if (line.StartsWith("at ", StringComparison.Ordinal)) return false;
            // fish and node both underline the offending token on its own line.
            if (line.All(character => character is '^' or '~')) return false;
            return true;
        }));
}

/// What a command needs to run. An interface rather than a static call so that every decision
/// above this line can be tested without a machine, a network or a key.
public interface IProcessRunner
{
    Task<CommandResult> RunAsync(
        string executable,
        IReadOnlyList<string> arguments,
        TimeSpan timeout,
        byte[]? input = null,
        CancellationToken cancellationToken = default);
}

public interface IAgentProcessRunner : IProcessRunner
{
    Task<CommandResult> RunAgentAsync(string executable, IReadOnlyList<string> arguments, TimeSpan timeout,
        byte[]? input = null, CancellationToken cancellationToken = default);
}

/// The real one. Everything runs off the UI thread and both output streams are drained on their own
/// tasks so a chatty command cannot deadlock the pipe.
public sealed class ProcessRunner : IAgentProcessRunner
{
    public Task<CommandResult> RunAsync(
        string executable,
        IReadOnlyList<string> arguments,
        TimeSpan timeout,
        byte[]? input = null,
        CancellationToken cancellationToken = default) => RunCoreAsync(executable, arguments, timeout, input, cancellationToken, false);

    public Task<CommandResult> RunAgentAsync(string executable, IReadOnlyList<string> arguments, TimeSpan timeout,
        byte[]? input = null, CancellationToken cancellationToken = default) =>
        RunCoreAsync(executable, arguments, timeout, input, cancellationToken, true);

    private async Task<CommandResult> RunCoreAsync(string executable, IReadOnlyList<string> arguments,
        TimeSpan timeout, byte[]? input, CancellationToken cancellationToken, bool agentProtocol)
    {
        var startInfo = new ProcessStartInfo
        {
            FileName = executable,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            RedirectStandardInput = true,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        foreach (var argument in arguments) startInfo.ArgumentList.Add(argument);
        var tracePath = Environment.GetEnvironmentVariable("LEGION_CONTROL_PROCESS_TRACE");
        var started = Stopwatch.StartNew();
        var traceGate = new object();
        void Trace(string message)
        {
            if (string.IsNullOrWhiteSpace(tracePath)) return;
            try { lock (traceGate) File.AppendAllText(tracePath, $"{DateTimeOffset.UtcNow:O} +{started.ElapsedMilliseconds}ms {Path.GetFileName(executable)} {message}\n"); }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
        }

        // Never wake a machine just by looking at it.
        //
        // An ssh ProxyCommand helper that wakes a machine when it does not answer can honour this
        // and stay quiet. That kind of helper is right for a person opening a shell and wrong here:
        // this app polls, so a sleeping machine would be woken by the next status check and sleep
        // could never stick. The wake action does not go through here at all; it sends its own
        // packet.
        startInfo.Environment["LEGION_NO_WAKE"] = "1";

        using var process = new Process { StartInfo = startInfo };
        try
        {
            process.Start();
            Trace($"started pid={process.Id} arguments={arguments.Count} inputBytes={input?.Length ?? 0}");
        }
        catch (Exception error)
        {
            return new CommandResult
            {
                ExitCode = -1,
                LaunchFailure = $"Could not run {executable}: {error.Message}",
            };
        }

        using var watchdog = new CancellationTokenSource(timeout);
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(watchdog.Token, cancellationToken);
        using var drains = new CancellationTokenSource();
        var reply = agentProtocol ? new AgentReplyFraming() : null;
        var output = new OutputCapture(process.StandardOutput, drains.Token, message => Trace("stdout " + message), reply);
        var errors = new OutputCapture(process.StandardError, drains.Token, message => Trace("stderr " + message));

        // Close an empty stdin synchronously before waiting. On Windows an SSH child can keep its
        // input channel open if this close is queued behind pipe readers on the thread pool.
        // Nonempty input is written concurrently with both drains and obeys the same deadline.
        Task writing;
        if (input is null || input.Length == 0)
        {
            process.StandardInput.Close();
            Trace("stdin closed synchronously");
            writing = Task.CompletedTask;
        }
        else writing = WriteInputAsync(process, input, linked.Token);

        var timedOut = false;
        try
        {
            var exited = process.WaitForExitAsync(linked.Token);
            if (reply is not null && await Task.WhenAny(exited, reply.Completed.Task) == reply.Completed.Task)
            {
                // The agent protocol ends at its complete JSON reply. Some Windows OpenSSH
                // versions keep their transport pipes alive after the remote command has ended.
                // Only close this SSH transport; the decoded operation record determines outcome.
                Trace("complete agent JSON reply; closing transport");
                TryKill(process);
            }
            await exited;
            Trace($"exited code={process.ExitCode}");
        }
        catch (OperationCanceledException)
        {
            timedOut = watchdog.IsCancellationRequested;
            Trace($"wait cancelled watchdog={timedOut}");
            TryKill(process);
            // Give the process a moment to go, then take whatever the readers already have. The
            // pipe stays open as long as anything holds the far end, and ssh hands its own stderr
            // to a ProxyCommand, so an unbounded wait here would make the timeout meaningless.
            try
            {
                using var grace = new CancellationTokenSource(TimeSpan.FromSeconds(3));
                await process.WaitForExitAsync(grace.Token);
            }
            catch (OperationCanceledException)
            {
            }
        }

        await Task.WhenAny(Task.WhenAll(output.Completion, errors.Completion, writing), Task.Delay(TimeSpan.FromSeconds(2), CancellationToken.None));

        drains.Cancel();
        Trace($"return stdoutChars={output.Text.Length} stderrChars={errors.Text.Length}");
        return new CommandResult
        {
            ExitCode = process.HasExited ? process.ExitCode : -1,
            StandardOutput = output.Text,
            StandardError = errors.Text,
            TimedOut = timedOut,
        };
    }

    private static async Task WriteInputAsync(Process process, byte[] input, CancellationToken token)
    {
        try
        {
            await process.StandardInput.BaseStream.WriteAsync(input, token).ConfigureAwait(false);
            await process.StandardInput.BaseStream.FlushAsync(token).ConfigureAwait(false);
        }
        catch (Exception error) when (error is IOException or ObjectDisposedException or OperationCanceledException) { }
        finally
        {
            try { process.StandardInput.Close(); } catch (IOException) { } catch (ObjectDisposedException) { }
        }
    }

    private sealed class OutputCapture
    {
        private readonly StringBuilder _text = new();
        private readonly object _gate = new();
        public Task Completion { get; }
        public string Text { get { lock (_gate) return _text.ToString(); } }
        public OutputCapture(StreamReader reader, CancellationToken token, Action<string> trace, AgentReplyFraming? framing = null) => Completion = DrainAsync(reader, token, trace, framing);
        private async Task DrainAsync(StreamReader reader, CancellationToken token, Action<string> trace, AgentReplyFraming? framing)
        {
            var buffer = new char[4096];
            try
            {
                while (true)
                {
                    var read = await reader.ReadAsync(buffer, token).ConfigureAwait(false);
                    if (read == 0) { trace("EOF"); return; }
                    lock (_gate) _text.Append(buffer, 0, read);
                    trace($"read chars={read}");
                    framing?.Append(buffer.AsSpan(0, read));
                }
            }
            catch (Exception error) when (error is IOException or ObjectDisposedException or OperationCanceledException) { }
        }
    }

    private sealed class AgentReplyFraming
    {
        private const int MaxReplyChars = 16 * 1024 * 1024;
        private readonly StringBuilder _candidate = new();
        private bool _started;
        private bool _lineStart = true;
        private bool _overLimit;
        public TaskCompletionSource<bool> Completed { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public void Append(ReadOnlySpan<char> text)
        {
            if (_overLimit || Completed.Task.IsCompleted) return;
            foreach (var character in text)
            {
                if (!_started)
                {
                    if (character == '\n') { _lineStart = true; continue; }
                    if (_lineStart && char.IsWhiteSpace(character)) continue;
                    if (!_lineStart || character != '{') { _lineStart = false; continue; }
                    _started = true;
                }
                _candidate.Append(character);
                if (_candidate.Length > MaxReplyChars) { _overLimit = true; _candidate.Clear(); return; }
                if (character != '\n') continue;
                try
                {
                    using var document = JsonDocument.Parse(_candidate.ToString());
                    if (document.RootElement.ValueKind == JsonValueKind.Object
                        && document.RootElement.TryGetProperty("ok", out var ok)
                        && ok.ValueKind is JsonValueKind.True or JsonValueKind.False)
                    { Completed.TrySetResult(true); return; }
                }
                catch (JsonException) { }
            }
        }
    }

    private static void TryKill(Process process)
    {
        try
        {
            if (!process.HasExited) process.Kill(entireProcessTree: true);
        }
        catch (Exception)
        {
            // Already gone, or gone between the check and the call. Either way there is nothing to
            // kill and nothing to report.
        }
    }
}

/// A runner that answers from a script rather than from a process. The whole transport is testable
/// through this, including every failure classification, without a machine anywhere.
public sealed class ScriptedRunner(Func<string, IReadOnlyList<string>, byte[]?, CommandResult> answer) : IProcessRunner
{
    public List<(string Executable, IReadOnlyList<string> Arguments, byte[]? Input)> Calls { get; } = new();

    public Task<CommandResult> RunAsync(
        string executable,
        IReadOnlyList<string> arguments,
        TimeSpan timeout,
        byte[]? input = null,
        CancellationToken cancellationToken = default)
    {
        Calls.Add((executable, arguments, input));
        return Task.FromResult(answer(executable, arguments, input));
    }
}
