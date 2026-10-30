using System.Runtime.InteropServices;

namespace LegionControl.Desktop.Cli;

/// The console of the shell that started the app, on Windows.
///
/// The app is built as a Windows GUI program, so that starting it from the Start menu opens the
/// window and nothing else. Windows gives such a program no console and no standard output, and
/// everything the command line printed went nowhere: `legion-control --command status` in cmd or
/// PowerShell showed nothing at all, as if it had done nothing. A run that prints therefore joins
/// the console of whatever started it, before anything is written.
///
/// Output the shell redirected to a file or a pipe stays where it was sent. Windows hands a GUI
/// program those handles, and only a stream with nowhere to go is pointed at the console.
///
/// Whether the shell waits is still the shell's choice: cmd at its prompt and PowerShell go back to
/// the prompt at once for a GUI program, so its exit code is theirs to read only when they waited
/// for it. The README says how to make them.
internal static class ParentConsole
{
    private const int AttachParentProcess = -1;
    private const int StandardOutput = -11;
    private const int StandardError = -12;
    private const uint GenericRead = 0x80000000;
    private const uint GenericWrite = 0x40000000;
    private const uint ShareReadWrite = 0x1 | 0x2;
    private const uint OpenExisting = 3;
    private const uint FileTypeUnknown = 0;
    private static readonly IntPtr InvalidHandle = new(-1);

    /// Joins the parent's console for every standard stream that has nowhere to go. Does nothing
    /// anywhere but Windows, and nothing when there is no console to join: a start from Explorer,
    /// a scheduled task or a service then prints nowhere, as it always has.
    public static void Attach()
    {
        if (!OperatingSystem.IsWindows()) return;
        var output = !Usable(GetStdHandle(StandardOutput));
        var error = !Usable(GetStdHandle(StandardError));
        if (!output && !error) return;
        if (!AttachConsole(AttachParentProcess)) return;
        if (output) PointAtConsole(StandardOutput);
        if (error) PointAtConsole(StandardError);
    }

    /// Joining may already have given the stream the console. When it has not, the console's
    /// screen buffer is opened and handed to it, before .NET first asks for it.
    private static void PointAtConsole(int stream)
    {
        if (Usable(GetStdHandle(stream))) return;
        var console = CreateFileW("CONOUT$", GenericRead | GenericWrite, ShareReadWrite, IntPtr.Zero, OpenExisting, 0, IntPtr.Zero);
        if (console != InvalidHandle) SetStdHandle(stream, console);
    }

    /// Whether a standard handle leads anywhere. A GUI program started without redirection has
    /// none at all, and a stale one is a handle of no known type.
    private static bool Usable(IntPtr handle) =>
        handle != IntPtr.Zero && handle != InvalidHandle && GetFileType(handle) != FileTypeUnknown;

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AttachConsole(int processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr GetStdHandle(int stream);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetStdHandle(int stream, IntPtr handle);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint GetFileType(IntPtr handle);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security, uint creation,
        uint flags, IntPtr template);
}
