using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Updates;

/// The lock that says a copy of this app is running.
///
/// An update replaces the installation by renaming it, and nothing is renamed while a build runs
/// from it: on Windows the rename fails outright, and on Linux the running copy would carry on
/// from files that are no longer installed while its next start ran the other build. Waiting for
/// the process that started the update is not enough, because that is never the only copy that
/// can run: a window can be open while a scheduled command line runs.
///
/// So every copy, the window and the command line alike, holds `instance.lock` shared from its
/// start to its exit, and the update helper takes it exclusively before it moves anything. The
/// file sits in the state directory and is never deleted, so two processes can never each lock a
/// different file of the same name.
public static class InstanceLock
{
    public static string SharedPath => Path.Combine(AppPaths.Home, "instance.lock");

    /// The shared lock, for the life of the process, or null when it could not be had within
    /// [patience]. That is an update helper in the middle of a swap, which takes seconds, or a
    /// state directory this account cannot write. Either way the app runs without it: refusing to
    /// start over a lock is worse than an update that waits a little less for this copy.
    public static FileStream? Share(TimeSpan patience, string? path = null)
    {
        path ??= SharedPath;
        var until = DateTimeOffset.UtcNow + patience;
        while (true)
        {
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(path) ?? ".");
                return new FileStream(path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.ReadWrite);
            }
            catch (IOException) when (DateTimeOffset.UtcNow < until)
            {
                Thread.Sleep(100);
            }
            catch (Exception)
            {
                return null;
            }
        }
    }
}
