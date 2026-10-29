using Avalonia;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Updates;

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
        // Whatever escapes everything else is at least written down. The process may still end,
        // but not without a line saying why.
        AppDomain.CurrentDomain.UnhandledException += (_, unhandled) =>
            AppLog.Write("unhandled", unhandled.ExceptionObject as Exception);
        TaskScheduler.UnobservedTaskException += (_, unobserved) =>
        {
            AppLog.Write("a background task nobody waited for", unobserved.Exception);
            unobserved.SetObserved();
        };

        // Every copy holds this until it exits, the window and the command line alike, so an
        // update helper never renames the installation while one runs from it.
        using var instance = InstanceLock.Share(TimeSpan.FromSeconds(30));

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
