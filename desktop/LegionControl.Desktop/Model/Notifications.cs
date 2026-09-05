using System.Diagnostics;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Model;

/// One thing worth telling somebody who is not looking at the window.
public sealed record Announcement(string Title, string Body, bool IsBad);

/// Where an announcement goes.
///
/// Opt in, and honest about being unavailable. A notification setting that silently does nothing on
/// a system with no notification service is worse than one that says so: the user turns it on,
/// walks away, and finds out days later that the thing they asked to be told about was never told.
public interface INotificationSink
{
    /// Whether anything would actually appear. Read by the settings row, which says so.
    bool IsAvailable { get; }

    string Describe();

    void Announce(Announcement announcement);
}

public static class Notifications
{
    /// The sink for this system, or one that says why there is none.
    public static INotificationSink ForThisSystem() =>
        AppPaths.IsWindows ? new WindowsToastSink() : new NotifySendSink();
}

/// Linux, through the desktop notification service every desktop environment ships.
public sealed class NotifySendSink : INotificationSink
{
    private readonly string? _binary = Which("notify-send");

    public bool IsAvailable => _binary is not null;

    public string Describe() => IsAvailable
        ? "Desktop notifications through notify-send."
        : "notify-send is not installed, so nothing can be shown outside this window.";

    public void Announce(Announcement announcement)
    {
        if (_binary is null) return;
        try
        {
            var startInfo = new ProcessStartInfo(_binary)
            {
                UseShellExecute = false,
                CreateNoWindow = true,
            };
            startInfo.ArgumentList.Add("--app-name=Legion Control");
            startInfo.ArgumentList.Add($"--urgency={(announcement.IsBad ? "critical" : "normal")}");
            startInfo.ArgumentList.Add(announcement.Title);
            startInfo.ArgumentList.Add(announcement.Body);
            Process.Start(startInfo);
        }
        catch (Exception)
        {
            // A notification that could not be shown is not worth interrupting anything for. The
            // same text is already in the operations list in the window.
        }
    }

    private static string? Which(string name)
    {
        var path = Environment.GetEnvironmentVariable("PATH");
        if (path is null) return null;
        foreach (var directory in path.Split(Path.PathSeparator))
        {
            if (directory.Length == 0) continue;
            var candidate = Path.Combine(directory, name);
            if (File.Exists(candidate)) return candidate;
        }
        return null;
    }
}

/// Windows, through the toast API that ships with the system.
///
/// PowerShell is the only way to reach it without carrying a WinRT dependency that would have to be
/// excluded from the Linux build. The script is fixed and takes its two strings through the
/// environment, so nothing a machine reported can become PowerShell to run.
public sealed class WindowsToastSink : INotificationSink
{
    public bool IsAvailable => AppPaths.IsWindows;

    public string Describe() => IsAvailable
        ? "Windows notifications through the system toast service."
        : "Windows notifications are only available on Windows.";

    public void Announce(Announcement announcement)
    {
        if (!IsAvailable) return;
        const string script = """
            $ErrorActionPreference = 'Stop'
            [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
            $template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
            $texts = $template.GetElementsByTagName('text')
            $texts.Item(0).AppendChild($template.CreateTextNode($env:LEGION_TOAST_TITLE)) | Out-Null
            $texts.Item(1).AppendChild($template.CreateTextNode($env:LEGION_TOAST_BODY)) | Out-Null
            $toast = [Windows.UI.Notifications.ToastNotification]::new($template)
            [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Legion Control').Show($toast)
            """;
        try
        {
            var startInfo = new ProcessStartInfo("powershell.exe")
            {
                UseShellExecute = false,
                CreateNoWindow = true,
            };
            startInfo.ArgumentList.Add("-NoProfile");
            startInfo.ArgumentList.Add("-NonInteractive");
            startInfo.ArgumentList.Add("-Command");
            startInfo.ArgumentList.Add(script);
            startInfo.Environment["LEGION_TOAST_TITLE"] = announcement.Title;
            startInfo.Environment["LEGION_TOAST_BODY"] = announcement.Body;
            Process.Start(startInfo);
        }
        catch (Exception)
        {
        }
    }
}

/// For tests and for the headless mode, where an announcement is a line rather than a toast.
public sealed class RecordingSink : INotificationSink
{
    public List<Announcement> Announcements { get; } = new();

    public bool IsAvailable => true;

    public string Describe() => "Announcements are recorded rather than shown.";

    public void Announce(Announcement announcement) => Announcements.Add(announcement);
}
