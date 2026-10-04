using System.Text;

namespace LegionControl.Desktop.Transport;

/// ssh's own log for one attempt: `-o LogLevel=ERROR -E <file>`, a fresh file each time.
///
/// ssh writes the reason it gave up there, and nothing else does. The far side's stderr comes back
/// on ssh's stderr, and the agent and the launcher pass raw text through to it, so a remote program
/// can print "Connection refused" as easily as ssh can. Reading stderr as evidence is how a
/// mutation that ran gets sent a second time; reading this file is how it does not.
public static class SshLog
{
    /// The most of a log that is read. ssh at LogLevel=ERROR writes a line or two.
    public const int ReadLimit = 64 * 1024;

    private const UnixFileMode Private = UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute;

    /// Runs ssh with a fresh log of its own, and hands back what it did with the log read in.
    /// [run] is given the log's path for `-E`.
    public static async Task<CommandResult> RunAsync(string directory, Func<string, Task<CommandResult>> run)
    {
        string log;
        try
        {
            log = NewPath(directory);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            // ssh refuses to start without its log file, so this is a launch that never happened.
            return new CommandResult { ExitCode = -1, LaunchFailure = $"ssh's log could not be prepared: {error.Message}" };
        }
        CommandResult result;
        try
        {
            result = await run(log);
        }
        catch
        {
            ReadAndDelete(log);
            throw;
        }
        return result with { SshLog = ReadAndDelete(log) };
    }

    /// A path for a new log in [directory], which is created readable by this user only. ssh
    /// creates the file itself, with its own private mode.
    public static string NewPath(string directory)
    {
        if (OperatingSystem.IsWindows())
        {
            // The state directory sits in the user's own profile, which is already private.
            Directory.CreateDirectory(directory);
        }
        else
        {
            Directory.CreateDirectory(directory, Private);
            // CreateDirectory leaves an existing directory's mode alone.
            File.SetUnixFileMode(directory, Private);
        }
        return Path.Combine(directory, $"{Guid.NewGuid():N}.log");
    }

    /// The first [ReadLimit] bytes of the log, then the file is gone. A log that is missing or
    /// cannot be read is an empty one, which reads as an exit nothing explains: not known, and
    /// never "nothing ran".
    public static string ReadAndDelete(string path)
    {
        var text = "";
        try
        {
            using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            var buffer = new byte[ReadLimit];
            var length = 0;
            int read;
            while (length < buffer.Length && (read = stream.Read(buffer, length, buffer.Length - length)) > 0) length += read;
            text = Encoding.UTF8.GetString(buffer, 0, length);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
        try { File.Delete(path); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
        return text;
    }
}
