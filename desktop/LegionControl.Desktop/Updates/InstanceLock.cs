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
/// So every copy, the window and the command line alike, holds `<installation>.lock` shared from
/// its start to its exit, and the update helper takes it exclusively before it moves anything. It
/// sits beside the installation rather than in the state directory, because the installation is
/// what it guards: a copy started with `LEGION_CONTROL_HOME` pointing at a second setup runs from
/// the same files as the first. That directory is the one an update renames in, so wherever an
/// update can happen at all, the lock can be had; where this account cannot write it, the app
/// runs without one and no update is started either.
///
/// The window also holds `window.lock` in the state directory, so a second window does not open
/// over the first: two windows would each poll every machine, and either could start an update
/// the other would then be swapped out from under. A second start asks the first window to come
/// forward instead, through `window.show`. None of these files is ever deleted, so two processes
/// can never each lock a different file of the same name.
public static class InstanceLock
{
    public static string SharedPath => PathFor(AppContext.BaseDirectory);

    /// The lock of the installation in [installation]: a file beside it, named after it.
    public static string PathFor(string installation) =>
        installation.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar) + ".lock";

    public static string WindowPath => Path.Combine(AppPaths.Home, "window.lock");

    public static string ShowPath => Path.Combine(AppPaths.Home, "window.show");

    /// The shared lock, for the life of the process, or null when it could not be had within
    /// [patience]. That is an update helper in the middle of a swap, which takes seconds, or a
    /// directory this account cannot write. Either way the app runs without it: refusing to start
    /// over a lock is worse than an update that waits a little less for this copy.
    public static FileStream? Share(TimeSpan patience, string? path = null)
    {
        path ??= SharedPath;
        var until = DateTimeOffset.UtcNow + patience;
        while (true)
        {
            try
            {
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

    /// Asks the window open on this state directory to come forward, for a second start that
    /// found it open. Started again from a menu, the app would otherwise seem to do nothing while
    /// its window sits minimised or behind another; a Windows build has no console to say so in.
    public static void AskWindowToShow(string? path = null)
    {
        try
        {
            File.WriteAllText(path ?? ShowPath, DateTimeOffset.UtcNow.ToString("o"));
        }
        catch (Exception)
        {
            // The window stays where it is, and the log says a second start came and went.
        }
    }

    /// Calls [show] whenever a second start asks, until it is disposed, or null when the state
    /// directory cannot be watched. [show] runs on a thread of the watcher's.
    public static IDisposable? WhenAskedToShow(Action show, string? path = null)
    {
        path ??= ShowPath;
        try
        {
            var directory = Path.GetDirectoryName(path) ?? ".";
            Directory.CreateDirectory(directory);
            var watcher = new FileSystemWatcher(directory, Path.GetFileName(path))
            {
                NotifyFilter = NotifyFilters.FileName | NotifyFilters.LastWrite | NotifyFilters.Size,
            };
            watcher.Changed += (_, _) => show();
            watcher.Created += (_, _) => show();
            watcher.EnableRaisingEvents = true;
            return watcher;
        }
        catch (Exception)
        {
            return null;
        }
    }
}
