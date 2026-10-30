using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Updates;

/// The locks that say a copy of this app is running.
///
/// An update replaces the installation by renaming it, and nothing is renamed while a build runs
/// from it: on Windows the rename fails outright, and on Linux the running copy would carry on
/// from files that are no longer installed while its next start ran the other build. Waiting for
/// the process that started the update is not enough, because that is never the only copy that
/// can run: a window can be open while a scheduled command line runs.
///
/// So every copy, the window and the command line alike, holds `instance.lock` shared from its
/// start to its exit, and the update helper takes it exclusively before it moves anything. The
/// window also holds `window.lock` on its own, so a second window does not open over the first:
/// two windows would each poll every machine, and either could start an update the other would
/// then be swapped out from under. Both files sit in the state directory and are never deleted, so
/// two processes can never each lock a different file of the same name.
public static class InstanceLock
{
    public static string SharedPath => Path.Combine(AppPaths.Home, "instance.lock");

    public static string WindowPath => Path.Combine(AppPaths.Home, "window.lock");

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
            catch (IOException error) when (IsHeldElsewhere(error) && DateTimeOffset.UtcNow < until)
            {
                Thread.Sleep(100);
            }
            catch (Exception)
            {
                return null;
            }
        }
    }

    /// The window's own lock, held until it is disposed, or null when another window holds it. A
    /// state directory this account cannot write, or one on a disk that is read-only, full or
    /// failing, holds no lock for anyone, so the window then opens without one rather than not at
    /// all.
    public static IDisposable? Window(string? path = null)
    {
        path ??= WindowPath;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path) ?? ".");
            return new FileStream(path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
        }
        catch (IOException error) when (IsHeldElsewhere(error))
        {
            return null;
        }
        catch (Exception)
        {
            return new Unheld();
        }
    }

    private sealed class Unheld : IDisposable
    {
        public void Dispose() { }
    }

    /// Whether a window is open on this state directory now.
    public static bool IsWindowOpen(string? path = null)
    {
        path ??= WindowPath;
        try
        {
            using var held = new FileStream(path, FileMode.Open, FileAccess.ReadWrite, FileShare.None);
            return false;
        }
        catch (IOException error) when (IsHeldElsewhere(error))
        {
            return true;
        }
        catch (Exception)
        {
            return false;
        }
    }

    /// Whether opening a lock failed because another process holds it, rather than for anything
    /// else. A read-only, full or failing disk fails with an IOException too, and holds no lock
    /// for anyone: taken for one, it would keep the window shut and every update refused.
    public static bool IsHeldElsewhere(IOException error) => OperatingSystem.IsWindows()
        // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION, as an HRESULT.
        ? error.HResult is unchecked((int)0x80070020) or unchecked((int)0x80070021)
        // EWOULDBLOCK from the flock .NET takes for a FileShare, which it gives as the error.
        : error.HResult == (OperatingSystem.IsLinux() ? 11 : 35);
}
