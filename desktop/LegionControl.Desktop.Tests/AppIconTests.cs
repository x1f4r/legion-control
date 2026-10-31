using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The mark the tray and the title bar show, which the app draws for itself.
public class AppIconTests
{
    [AvaloniaFact]
    public void TheIconIsALaptopWithACoolAndAWarmSystemOnADarkTile()
    {
        using var icon = AppIcon.Render(256);
        Assert.Equal(new PixelSize(256, 256), icon.PixelSize);
        var at = Pixels(icon);

        // Points of the original's 1024-unit square, a quarter of the way down to 256 pixels.
        Assert.Equal(0, at(512, 16).A);
        Assert.Equal(0, at(8, 8).A);
        var tile = at(512, 120);
        Assert.Equal(255, tile.A);
        Assert.True(tile is { R: < 60, G: < 70, B: < 100 }, $"the tile is dark: {tile}");
        var frame = at(512, 232);
        Assert.True(frame is { R: > 180, G: > 190, B: > 190 }, $"the frame is light: {frame}");
        var cool = at(320, 460);
        Assert.True(cool.B > cool.R + 60, $"the left half of the screen is blue: {cool}");
        var warm = at(704, 460);
        Assert.True(warm.R > warm.B + 60, $"the right half of the screen is orange: {warm}");
        var screen = at(512, 460);
        Assert.True(screen is { R: < 40, G: < 40, B: < 50 }, $"a dark seam parts the two: {screen}");
        var stand = at(512, 740);
        Assert.True(stand is { R: > 150, G: > 160, B: > 160 }, $"the base is light: {stand}");

        // What the tray takes, at the size it asks for.
        Assert.Equal(new PixelSize(64, 64), AppIcon.Render(64).PixelSize);
        Assert.NotNull(AppIcon.Icon(64));
    }

    /// Reads [bitmap] pixel by pixel, at a point given in the original's 1024-unit square.
    private static Func<int, int, Color> Pixels(Bitmap bitmap)
    {
        using var copy = new WriteableBitmap(bitmap.PixelSize, new Vector(96, 96), PixelFormat.Bgra8888, AlphaFormat.Unpremul);
        byte[] bytes;
        int stride;
        using (var frame = copy.Lock())
        {
            bitmap.CopyPixels(frame);
            stride = frame.RowBytes;
            bytes = new byte[stride * frame.Size.Height];
            Marshal.Copy(frame.Address, bytes, 0, bytes.Length);
        }
        var scale = bitmap.PixelSize.Width / 1024.0;
        return (x, y) =>
        {
            var index = (int)(y * scale) * stride + (int)(x * scale) * 4;
            return Color.FromArgb(bytes[index + 3], bytes[index + 2], bytes[index + 1], bytes[index]);
        };
    }
}
