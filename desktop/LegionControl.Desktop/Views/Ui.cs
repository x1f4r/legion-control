using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;

namespace LegionControl.Desktop.Views;

/// The few shapes every screen is built from.
///
/// Deliberately five of them, and deliberately not one of them a box. A control app is a page of
/// facts about machines: headings, rows of "this is called that", quiet notes underneath, a rule
/// between sections, and plain buttons. Wrapping each machine in a bordered card would add a
/// hundred pixels of chrome per machine and say nothing at all.
public static class Ui
{
    public static FontFamily NativeFont => new(OperatingSystem.IsWindows() ? "Segoe UI"
        : OperatingSystem.IsMacOS() ? ".AppleSystemUIFont" : "sans-serif");
    private static IBrush Brush(string key, string fallback) =>
        Application.Current is { } app && app.TryGetResource(key, app.ActualThemeVariant, out var value) && value is IBrush brush
            ? brush : new SolidColorBrush(Color.Parse(fallback));
    public static IBrush Muted => Brush("PageSecondary", "#6A665F");
    public static IBrush Hairline => Brush("PageSeparator", "#E0DCD5");
    public static IBrush Bad => Brush("StatusBad", "#B43B31");
    public static IBrush Unknown => Brush("StatusAttention", "#91620A");
    public static IBrush Good => Brush("StatusGood", "#2E7D32");

    public static TextBlock Title(string text) => new()
    {
        Text = text,
        FontSize = 22,
        FontWeight = FontWeight.SemiBold,
        Margin = new Thickness(0, 0, 0, 2),
        TextWrapping = TextWrapping.Wrap,
    };

    public static TextBlock SectionHeading(string text) => new()
    {
        Text = text,
        FontSize = 15,
        FontWeight = FontWeight.SemiBold,
        Foreground = Muted,
        Margin = new Thickness(0, 10, 0, 6),
    };

    /// One fact: what it is called on the left, what it says on the right.
    public static Control DetailRow(string label, string value, IBrush? colour = null)
    {
        var grid = new Grid
        {
            ColumnDefinitions = new ColumnDefinitions("140,*"),
            Margin = new Thickness(0, 1, 0, 1),
        };
        grid.Children.Add(new TextBlock
        {
            Text = label,
            Foreground = Muted,
            VerticalAlignment = VerticalAlignment.Top,
            TextWrapping = TextWrapping.Wrap,
        });
        var text = new TextBlock
        {
            Text = value,
            TextWrapping = TextWrapping.Wrap,
        };
        // Only when there is one. Setting Foreground to null is not "leave it alone" in Avalonia;
        // it is "no brush", and the row draws as an empty line with a label beside it.
        if (colour is not null) text.Foreground = colour;
        Grid.SetColumn(text, 1);
        grid.Children.Add(text);
        return grid;
    }

    /// Something worth saying that is not a fact about the machine: a warning, an explanation, the
    /// reason a control is not there.
    public static Control Note(string text, IBrush? colour = null) => new TextBlock
    {
        Text = text,
        Foreground = colour ?? Muted,
        TextWrapping = TextWrapping.Wrap,
        Margin = new Thickness(0, 3, 0, 3),
        FontSize = 12,
    };

    public static Control Rule() => new Border
    {
        Height = 1,
        Background = Hairline,
        Margin = new Thickness(0, 18, 0, 4),
    };

    public static Button Action(string text, Action handler, bool enabled = true, string? explanation = null, bool primary = false)
    {
        var button = new Button
        {
            Content = text,
            IsEnabled = enabled,
            Margin = new Thickness(0, 0, 8, 6),
            Padding = new Thickness(10, 4, 10, 4),
        };
        if (primary && enabled) button.Classes.Add("primary");
        if (explanation is not null) ToolTip.SetTip(button, explanation);
        button.Click += (_, _) => handler();
        return button;
    }

    public static Panel Actions(params Control?[] controls)
    {
        var panel = new WrapPanel { Margin = new Thickness(0, 8, 0, 0) };
        foreach (var control in controls)
        {
            if (control is not null) panel.Children.Add(control);
        }
        return panel;
    }

    public static StackPanel Column(double spacing = 0) => new()
    {
        Orientation = Orientation.Vertical,
        Spacing = spacing,
    };

    public static MenuItem MenuAction(string text, Action handler, bool enabled = true, string? explanation = null)
    {
        var item = new MenuItem { Header = text, IsEnabled = enabled };
        item.Click += (_, _) => handler();
        if (explanation is not null) ToolTip.SetTip(item, explanation);
        return item;
    }

    public static Button MenuButton(string text, IEnumerable<MenuItem> items, string accessibleName)
    {
        var menu = new ContextMenu { ItemsSource = items.ToArray() };
        var button = Action(text, () => { });
        button.ContextMenu = menu;
        button.Click += (_, _) => menu.Open(button);
        Avalonia.Automation.AutomationProperties.SetName(button, accessibleName);
        ToolTip.SetTip(button, accessibleName);
        return button;
    }

    public static void ShowDetails(Window owner, string title, Control content)
    {
        content.Margin = new Thickness(16);
        var window = new Window
        {
            Title = title, Width = Math.Min(680, Math.Max(400, owner.Bounds.Width)),
            Height = Math.Min(600, Math.Max(320, owner.Bounds.Height)),
            MinWidth = 360, MinHeight = 260, FontFamily = NativeFont,
            WindowStartupLocation = WindowStartupLocation.CenterOwner,
            Content = new ScrollViewer { Content = content },
        };
        window.Show(owner);
    }
}
