using Avalonia;
using Avalonia.Headless;

namespace LegionControl.Desktop.Tests;

/// The app the window tests run in: the real one, with its theme and its resources, drawn by Skia
/// into memory rather than onto a screen, so a test can look at the pixels it drew. Every
/// [Avalonia.Headless.XUnit.AvaloniaFactAttribute] runs on its one window thread.
public static class HeadlessApp
{
    public static AppBuilder BuildAvaloniaApp() => AppBuilder.Configure<App>()
        .UseSkia()
        .UseHeadless(new AvaloniaHeadlessPlatformOptions { UseHeadlessDrawing = false });
}
