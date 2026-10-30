namespace LegionControl.Desktop.Config;

/// The two things every state file the window and the command line both write has to do.
///
/// Each change to such a file takes its `.lock`, reads the file again, applies only the change it
/// was asked to make and writes the whole, the way [Transport.HostIdentityStore] and
/// [LaunchApprovals] do: a process that wrote what it read at its start would put back everything
/// the other one changed since. And a file that cannot be read is moved aside before anything is
/// written over it, so a person can still see what was in it, rather than being replaced by
/// defaults without a word.
public static class StateFile
{
    /// The exclusive handle that serialises writers of [path] across processes. The lock file is
    /// never unlinked, so two processes can never each hold a lock on a different file of the same
    /// name. Throws when it cannot be had within ten seconds.
    public static FileStream Lock(string path)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path) ?? ".");
        var until = DateTimeOffset.UtcNow.AddSeconds(10);
        while (true)
        {
            try
            {
                return new FileStream(path + ".lock", FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
            }
            catch (IOException) when (DateTimeOffset.UtcNow < until)
            {
                Thread.Sleep(50);
            }
        }
    }

    /// Moves [path] to `<path>.corrupt-<unix time>` and says so in the log. Returns where it went.
    public static string SetAside(string path, string why)
    {
        var stamp = DateTimeOffset.UtcNow.ToUnixTimeSeconds();
        var target = $"{path}.corrupt-{stamp}";
        for (var attempt = 2; File.Exists(target); attempt += 1) target = $"{path}.corrupt-{stamp}-{attempt}";
        File.Move(path, target);
        AppLog.Write($"{path} could not be read ({why}), so it was moved to {target} and started again");
        return target;
    }
}
