namespace LegionControl.Desktop.Config;

/// The app's own log, `legion-control-desktop.log` in the state directory.
///
/// Only for what went wrong in a way nobody planned for: an exception that reached the window, a
/// background task nobody awaited, a state file that had to be set aside. Everything a person is
/// meant to act on is said on screen or on the command line as well; this is where the detail goes,
/// so a crash that was shown as one sentence can still be explained afterwards.
///
/// Writing here never throws. A log that cannot be written is a log line lost, never a second
/// failure on top of the first.
public static class AppLog
{
    /// Past this the log is moved aside to `.1` and started again, so it never grows without bound.
    public const long Limit = 1024 * 1024;

    private static readonly object Gate = new();

    public static string Path => AppPaths.LogFile;

    /// One line about [error], with its stack, under [context]: what was being done when it happened.
    public static void Write(string context, Exception? error) =>
        Write(error is null ? context : $"{context}: {error.GetType().Name}: {error.Message}\n{error}");

    public static void Write(string line)
    {
        try
        {
            lock (Gate)
            {
                var path = Path;
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(path) ?? ".");
                if (new FileInfo(path) is { Exists: true, Length: > Limit }) File.Move(path, path + ".1", overwrite: true);
                // Shared, so the window and a command line run at the same moment both get their line in.
                using var stream = new FileStream(path, FileMode.Append, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete);
                using var writer = new StreamWriter(stream);
                writer.WriteLine($"{DateTimeOffset.UtcNow:O} [{Environment.ProcessId}] {line}");
            }
        }
        catch (Exception)
        {
        }
    }
}
