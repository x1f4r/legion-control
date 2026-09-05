using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Config;

public static class ImportAgentDefaults
{
    public static string[] For(RemoteShell shell, string? user, bool restricted)
    {
        var account = string.IsNullOrWhiteSpace(user) ? "me" : user.Trim();
        // The Windows profile folder can differ from the SSH account; this remains an editable suggestion.
        var home = shell == RemoteShell.Posix ? $"/home/{account}" : $"C:/Users/{account}";
        if (restricted)
            return ["node", $"{home}/.legion-control/agent/src/index.mjs"];
        if (shell == RemoteShell.Posix)
            // The outer SSH serializer quotes every argument, so a literal ~/path would not expand.
            // This fixed script resolves the remote home and forwards only the appended agent arguments.
            return ["/bin/sh", "-c", "exec \"$HOME/.legion-control/bin/legionctl\" \"$@\"", "legionctl"];
        return ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File",
                $"{home}/.legion-control/bin/legionctl.ps1"];
    }
}
