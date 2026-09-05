namespace LegionControl.Desktop.Updates;

/// Writes the proof an update helper waits for, only after the caller has proved the application
/// layer it cares about is ready. The desktop passes a real Avalonia render as that proof.
public static class UpdateHealthMarker
{
    public static bool WriteAfterProof(string path, Action proveReady)
    {
        try
        {
            proveReady();
            Directory.CreateDirectory(Path.GetDirectoryName(path) ?? ".");
            File.WriteAllText(path, DateTimeOffset.UtcNow.ToString("o"));
            return true;
        }
        catch (Exception)
        {
            // No marker means the helper restores the previous build. A partially initialised app
            // must never claim to be healthy merely because its process started.
            return false;
        }
    }
}
