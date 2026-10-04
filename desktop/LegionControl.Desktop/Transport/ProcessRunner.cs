using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace LegionControl.Desktop.Transport;

public sealed record CommandResult
{
    public int ExitCode { get; init; }
    public string StandardOutput { get; init; } = "";
    public string StandardError { get; init; } = "";
    /// This app ended the process: our own watchdog fired, or the caller cancelled. The far side
    /// may well still be working, so this is never read as either success or failure anywhere
    /// above this line.
    public bool TimedOut { get; init; }
    /// The caller cancelled, rather than the watchdog firing. Always together with [TimedOut]: a
    /// cancelled command is exactly as unknown as one that ran out of time, and is only told
    /// differently to the person reading about it.
    public bool Cancelled { get; init; }
    public string? LaunchFailure { get; init; }
    /// More came out than is kept, and the rest was discarded. Output that was cut off can never be
    /// read as a reply, however much of one it seems to hold.
    public bool StandardOutputTruncated { get; init; }
    /// What ssh wrote to its own log (`-E`), read back after the run. Only ssh writes there, never
    /// the far side, which is why this and not stderr is the evidence of where a command stopped.
    public string SshLog { get; init; } = "";

    public bool Succeeded => ExitCode == 0 && !TimedOut && LaunchFailure is null;

    /// The most useful line to show a human. Both ssh and node bury the one sentence that matters
    /// under a routing banner, stack frames and caret art, so the lines carrying no message go
    /// first.
    public string FailureText
    {
        get
        {
            if (LaunchFailure is not null) return LaunchFailure;
            if (Cancelled) return "The command was cancelled before it finished.";
            if (TimedOut) return "The command did not finish in time.";
            var fromLog = InformativeLines(SshLog);
            if (fromLog.Length > 0) return fromLog;
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

        // A command that was cancelled before it started never ran, and saying so is the one
        // honest answer there is. Once it has started, a cancellation is reported below as this
        // app ending a command whose outcome is not known.
        cancellationToken.ThrowIfCancellationRequested();

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
        var cancelled = false;
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
            // The watchdog and the caller end the process the same way, and either way the far
            // side may already have acted on what it was sent. A cancelled run is therefore never
            // handed back as an ordinary result with whatever exit status the kill produced: it is
            // a run this app ended, exactly as unknown as one that ran out of time.
            timedOut = true;
            cancelled = cancellationToken.IsCancellationRequested;
            Trace($"wait cancelled watchdog={watchdog.IsCancellationRequested} caller={cancelled}");
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
        // The process only closes the streams it was never asked for. These two were, so they are
        // closed here: a grandchild that inherited the far end (a ProxyCommand, a helper ssh
        // started) would otherwise keep both pipes, and a drain blocked on one, alive after the
        // command is long finished.
        output.Close();
        errors.Close();
        var standardOutput = output.Text;
        var standardError = errors.Text;
        Trace($"return stdoutChars={standardOutput.Length} stderrChars={standardError.Length} truncated={output.Truncated}");
        return new CommandResult
        {
            ExitCode = process.HasExited ? process.ExitCode : -1,
            StandardOutput = standardOutput,
            StandardError = standardError,
            StandardOutputTruncated = output.Truncated,
            TimedOut = timedOut,
            Cancelled = cancelled,
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

    /// One output stream, kept up to [Limit] bytes.
    ///
    /// Counted in the bytes the pipe delivered, the way every other client counts them, so a reply
    /// one of them would cut off is cut off here too; a count of decoded characters let through
    /// up to three times as many bytes of non-ASCII text. The rest is read and thrown away rather
    /// than left in the pipe: a child that cannot write blocks, and a blocked child is a timeout
    /// that says nothing. Sixteen MiB is far more than any agent reply, so a stream that reaches
    /// it is not one this app could have read anyway.
    ///
    /// The kept bytes are decoded once at the end, by the same rules the process's own reader
    /// would have used: its encoding, and a byte order mark that picks another.
    private sealed class OutputCapture
    {
        public const int Limit = 16 * 1024 * 1024;
        private readonly MemoryStream _kept = new();
        private readonly object _gate = new();
        private readonly StreamReader _reader;
        private readonly Encoding _encoding;
        private bool _truncated;
        public Task Completion { get; }
        public bool Truncated { get { lock (_gate) return _truncated; } }
        public OutputCapture(StreamReader reader, CancellationToken token, Action<string> trace, AgentReplyFraming? framing = null)
        {
            _reader = reader;
            // Nothing has been read through the reader, so this is still the encoding it was made
            // with rather than one a byte order mark chose.
            _encoding = reader.CurrentEncoding;
            Completion = DrainAsync(reader.BaseStream, token, trace, framing);
        }

        /// The kept bytes as text.
        public string Text
        {
            get
            {
                byte[] bytes;
                lock (_gate) bytes = _kept.ToArray();
                using var decoder = new StreamReader(new MemoryStream(bytes), _encoding, detectEncodingFromByteOrderMarks: true);
                return decoder.ReadToEnd();
            }
        }

        public void Close()
        {
            try { _reader.Dispose(); }
            catch (Exception error) when (error is IOException or ObjectDisposedException or InvalidOperationException) { }
        }

        private async Task DrainAsync(Stream stream, CancellationToken token, Action<string> trace, AgentReplyFraming? framing)
        {
            var buffer = new byte[4096];
            // Decodes for the framing as the bytes arrive. Stateful, so a character split across
            // two reads is still one character.
            var decoder = _encoding.GetDecoder();
            var characters = new char[_encoding.GetMaxCharCount(buffer.Length)];
            var first = true;
            try
            {
                while (true)
                {
                    var read = await stream.ReadAsync(buffer, token).ConfigureAwait(false);
                    if (read == 0) { trace("EOF"); return; }
                    int kept;
                    lock (_gate)
                    {
                        kept = (int)Math.Min(read, Limit - _kept.Length);
                        _kept.Write(buffer, 0, kept);
                        if (kept < read) _truncated = true;
                    }
                    trace($"read bytes={read}");
                    if (framing is null || kept == 0) continue;
                    var decoded = decoder.GetChars(buffer, 0, kept, characters, 0, flush: false);
                    var text = characters.AsSpan(0, decoded);
                    // The reader would have taken a byte order mark as one, never as text.
                    if (first && text.Length > 0 && text[0] == '\uFEFF') text = text[1..];
                    if (decoded > 0) first = false;
                    framing.Append(text);
                }
            }
            // Closing the reader under a pending read ends the read with one of these too.
            catch (Exception error) when (error is IOException or ObjectDisposedException or OperationCanceledException
                                              or InvalidOperationException) { }
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
