using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Media.Imaging;

namespace LegionControl.Desktop.Views;

/// The app's mark, drawn rather than shipped as a file: the laptop of the Mac app's icon
/// (mac/Resources/icon.svg), its screen split between a cool system and a warm one, on a dark tile.
///
/// It is drawn from the same geometry at whatever size it is asked for, so the tray and the title
/// bar each get one made for them and the repository holds no bitmap. Only the shapes that still
/// read at 16 to 256 pixels are kept: the original's shadows, sheen and hairline strokes vanish at
/// those sizes.
public static class AppIcon
{
    // In the original's 1024-unit square.
    private const string Tile = "M512 48C768 48 848 48 920 120C976 192 976 272 976 512C976 752 976 832 920 904C848 976 768 976 512 976C256 976 176 976 104 904C48 832 48 752 48 512C48 272 48 192 104 120C176 48 256 48 512 48Z";
    private const string Base = "M92 704H932C940 704 944 714 937 722L849 788C840 795 829 798 815 798H209C195 798 184 795 175 788L87 722C80 714 84 704 92 704Z";
    private static readonly Rect Frame = new(124, 214, 776, 522);
    private static readonly Rect Screen = new(168, 258, 688, 402);
    private static readonly Rect Cool = new(168, 258, 332, 402);
    private static readonly Rect Warm = new(524, 258, 332, 402);

    /// The mark at [size] pixels square, on a transparent background.
    public static Bitmap Render(int size)
    {
        var bitmap = new RenderTargetBitmap(new PixelSize(size, size), new Vector(96, 96));
        using (var context = bitmap.CreateDrawingContext())
        using (context.PushTransform(Matrix.CreateScale(size / 1024.0, size / 1024.0)))
        {
            context.DrawGeometry(Gradient(0.11, 0.05, 0.87, 0.98, ("#26334B", 0), ("#111B2E", 0.48), ("#07101E", 1)), null, Geometry.Parse(Tile));
            context.DrawRectangle(Gradient(0.14, 0, 0.89, 1, ("#F2F6F8", 0), ("#D2DDE4", 0.5), ("#93A4B2", 1)), null, Frame, 64, 64);
            context.DrawRectangle(new SolidColorBrush(Color.Parse("#09111E")), null, Screen, 30, 30);
            using (context.PushClip(new RoundedRect(Screen, 30)))
            {
                context.DrawRectangle(Gradient(0.02, 0, 0.96, 1, ("#58C2E1", 0), ("#347DC5", 1)), null, Cool);
                context.DrawRectangle(Gradient(0.04, 0, 0.98, 1, ("#EEAC59", 0), ("#C75F4F", 1)), null, Warm);
            }
            context.DrawGeometry(Gradient(0.5, 0, 0.5, 0.94, ("#F5F7F8", 0), ("#CAD5DC", 0.42), ("#8E9DAA", 1)), null, Geometry.Parse(Base));
        }
        return bitmap;
    }

    /// The mark as a tray icon or a window takes it.
    public static WindowIcon Icon(int size) => new(Render(size));

    /// A gradient from one point to another of the shape it fills, each a fraction of its bounds.
    private static LinearGradientBrush Gradient(double x1, double y1, double x2, double y2, params (string Colour, double Offset)[] stops)
    {
        var brush = new LinearGradientBrush
        {
            StartPoint = new RelativePoint(x1, y1, RelativeUnit.Relative),
            EndPoint = new RelativePoint(x2, y2, RelativeUnit.Relative),
        };
        foreach (var (colour, offset) in stops) brush.GradientStops.Add(new GradientStop(Color.Parse(colour), offset));
        return brush;
    }
}
