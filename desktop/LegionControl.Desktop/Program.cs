using Avalonia;
using LegionControl.Desktop.Cli;

namespace LegionControl.Desktop;

internal static class Program
{
    /// The flags that mean "do not open a window".
    ///
    /// Checked before Avalonia is initialised, so the headless mode runs on a machine with no
    /// display at all: a Windows service session, a Linux box over ssh, or a CI runner.
    private static readonly string[] HeadlessFlags =
    {
        "--smoke", "--command", "--reconcile-once", "--version", "--help", "-h",
    };

    [STAThread]
    public static int Main(string[] args)
    {
        // A bare verb is part of the documented CLI too. Treat an unknown bare verb as a CLI
        // error rather than quietly opening a window and ignoring it.
        if (args.Any(argument => HeadlessFlags.Contains(argument))
            || args.FirstOrDefault() is { } first && !first.StartsWith("--", StringComparison.Ordinal))
        {
            return Runner.RunAsync(args, Console.Out).GetAwaiter().GetResult();
        }

        return BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    }

    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .WithInterFont()
            .LogToTrace();
}
