using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;

namespace LegionControl.Desktop.Views;

/// Brings what the window shows up to date with the same screen built afresh, changing only what
/// differs.
///
/// Every screen is built by plain code from the model, top to bottom, on every change: that keeps
/// each one a page of facts in the order it reads. Putting each new build in place of the last
/// threw every control away on every poll, and with them the keyboard focus, a screen reader's
/// place and an open menu. So the new build is laid over the one shown instead. A control still
/// there stays in the window and takes the new build's text, state and handlers; only what is new
/// is added, and only what has gone is removed.
///
/// Children are matched in order: first those that say the same thing as before, then, in each gap
/// between two of those, the rest of the same type and key. A key, given by [Ui.Keyed], is what
/// tells rows of one shape apart, one service from another. Controls whose keys differ are never
/// matched, so focus on one service's button never ends up on another's.
public static class LiveTree
{
    /// What a kept control takes from the new build: everything the screens set on a control, and
    /// nothing a control keeps for itself while it is shown.
    private static readonly AvaloniaProperty[] Synced =
    {
        Layoutable.MarginProperty, Layoutable.WidthProperty, Layoutable.HeightProperty,
        Layoutable.MinWidthProperty, Layoutable.MinHeightProperty, Layoutable.MaxWidthProperty, Layoutable.MaxHeightProperty,
        Layoutable.HorizontalAlignmentProperty, Layoutable.VerticalAlignmentProperty,
        Visual.IsVisibleProperty, InputElement.IsEnabledProperty,
        TextBlock.TextProperty, TextBlock.FontSizeProperty, TextBlock.FontWeightProperty, TextBlock.ForegroundProperty,
        TextBlock.TextWrappingProperty,
        TemplatedControl.PaddingProperty,
        StackPanel.OrientationProperty, StackPanel.SpacingProperty, Panel.BackgroundProperty,
        Border.BorderBrushProperty, Border.BorderThicknessProperty, Border.BackgroundProperty,
        Grid.ColumnProperty, Grid.RowProperty, Grid.ColumnSpanProperty, Grid.RowSpanProperty,
        ToolTip.TipProperty, AutomationProperties.NameProperty, AutomationProperties.HelpTextProperty,
        Ui.HandlerProperty,
    };

    /// Lays the children of [built] over those of [shown]. [built] is left empty: whatever of it
    /// is new now lives in [shown].
    public static void Update(Panel shown, Panel built) => UpdateChildren(shown.Children, built.Children);

    private static void UpdateChildren(Avalonia.Controls.Controls shown, Avalonia.Controls.Controls built)
    {
        var fresh = built.ToList();
        built.Clear();
        var kept = Align(shown.ToList(), fresh);
        var keep = new HashSet<Control>(kept.OfType<Control>(), ReferenceEqualityComparer.Instance);
        for (var index = shown.Count - 1; index >= 0; index--)
            if (!keep.Contains(shown[index])) shown.RemoveAt(index);
        // What is kept is in the order it was, which is the new build's order too, so each one is
        // already where it belongs once everything before it is.
        for (var index = 0; index < fresh.Count; index++)
        {
            if (kept[index] is { } control) Merge(control, fresh[index]);
            else shown.Insert(index, fresh[index]);
        }
    }

    /// For each control of [fresh], the control of [shown] it replaces, or null for a new one.
    private static Control?[] Align(IReadOnlyList<Control> shown, IReadOnlyList<Control> fresh)
    {
        var kept = new Control?[fresh.Count];
        var anchors = new List<(int Fresh, int Shown)>();
        var from = 0;
        for (var index = 0; index < fresh.Count; index++)
        {
            var identity = Identity(fresh[index]);
            for (var candidate = from; candidate < shown.Count; candidate++)
            {
                if (Identity(shown[candidate]) != identity) continue;
                kept[index] = shown[candidate];
                anchors.Add((index, candidate));
                from = candidate + 1;
                break;
            }
        }
        // The gaps: what lies between two anchors on each side, matched by kind, in order.
        anchors.Add((fresh.Count, shown.Count));
        var (freshStart, shownStart) = (0, 0);
        foreach (var (freshEnd, shownEnd) in anchors)
        {
            var next = shownStart;
            for (var index = freshStart; index < freshEnd; index++)
            {
                var kind = Kind(fresh[index]);
                for (var candidate = next; candidate < shownEnd; candidate++)
                {
                    if (Kind(shown[candidate]) != kind) continue;
                    kept[index] = shown[candidate];
                    next = candidate + 1;
                    break;
                }
            }
            (freshStart, shownStart) = (freshEnd + 1, shownEnd + 1);
        }
        return kept;
    }

    /// Which controls may stand for each other: the same type, the same key, and a button with a
    /// menu only for another with one.
    private static string Kind(Control control) =>
        $"{control.GetType().FullName}\u001f{control.GetValue(Ui.KeyProperty)}\u001f{control.ContextMenu is not null}";

    /// What a control says by itself, without its children: two with the same are the same
    /// control as far as anybody using the window can tell.
    private static string Identity(Control control) => string.Join('\u001f', Kind(control),
        control switch
        {
            TextBlock text => text.Text,
            ContentControl { Content: string content } => content,
            _ => null,
        },
        AutomationProperties.GetName(control));

    private static void Merge(Control shown, Control fresh)
    {
        Sync(shown, fresh);
        // The classes a screen gives, never the pseudo-classes such as :focus that the control
        // keeps for itself.
        var classes = fresh.Classes.Where(name => !name.StartsWith(':')).ToList();
        if (!shown.Classes.Where(name => !name.StartsWith(':')).SequenceEqual(classes)) shown.Classes.Replace(classes);
        switch (shown, fresh)
        {
            case (Grid grid, Grid built):
                var columns = built.ColumnDefinitions.Select(column => column.Width).ToList();
                if (!grid.ColumnDefinitions.Select(column => column.Width).SequenceEqual(columns))
                {
                    grid.ColumnDefinitions = new ColumnDefinitions();
                    foreach (var width in columns) grid.ColumnDefinitions.Add(new ColumnDefinition(width));
                }
                var rows = built.RowDefinitions.Select(row => row.Height).ToList();
                if (!grid.RowDefinitions.Select(row => row.Height).SequenceEqual(rows))
                {
                    grid.RowDefinitions = new RowDefinitions();
                    foreach (var height in rows) grid.RowDefinitions.Add(new RowDefinition(height));
                }
                UpdateChildren(grid.Children, built.Children);
                break;
            case (Panel panel, Panel built):
                UpdateChildren(panel.Children, built.Children);
                break;
            case (Decorator decorator, Decorator built):
                if (built.Child is { } child && decorator.Child is { } inside && Kind(inside) == Kind(child)) Merge(inside, child);
                else
                {
                    var replacement = built.Child;
                    built.Child = null;
                    decorator.Child = replacement;
                }
                break;
            case (ContentControl content, ContentControl built):
                if (built.Content is Control || content.Content is Control)
                {
                    if (built.Content is Control next && content.Content is Control current && Kind(current) == Kind(next)) Merge(current, next);
                    else
                    {
                        var value = built.Content;
                        built.Content = null;
                        content.Content = value;
                    }
                }
                else if (!Equals(content.Content, built.Content)) content.Content = built.Content;
                break;
        }
        MergeMenu(shown, fresh);
    }

    private static void Sync(AvaloniaObject shown, AvaloniaObject fresh)
    {
        foreach (var property in Synced)
        {
            if (fresh.IsSet(property))
            {
                var value = fresh.GetValue(property);
                if (!shown.IsSet(property) || !Same(shown.GetValue(property), value)) shown.SetValue(property, value);
            }
            else if (shown.IsSet(property)) shown.ClearValue(property);
        }
    }

    private static bool Same(object? shown, object? fresh) => (shown, fresh) switch
    {
        (ISolidColorBrush a, ISolidColorBrush b) => a.Color == b.Color && a.Opacity == b.Opacity,
        _ => Equals(shown, fresh),
    };

    /// A button's menu stays the one shown, so a menu open while a poll arrives stays open. Its
    /// items are kept, with the new build's handlers, while they say what they said; otherwise
    /// they are replaced by the new build's.
    private static void MergeMenu(Control shown, Control fresh)
    {
        if (fresh.ContextMenu is not { } built)
        {
            if (shown.ContextMenu is not null) shown.ContextMenu = null;
            return;
        }
        if (shown.ContextMenu is not { } menu)
        {
            fresh.ContextMenu = null;
            shown.ContextMenu = built;
            return;
        }
        MergeItems(menu, built);
    }

    private static void MergeItems(ItemsControl shown, ItemsControl fresh)
    {
        var items = (shown.ItemsSource ?? Array.Empty<object>()).OfType<MenuItem>().ToList();
        var built = (fresh.ItemsSource ?? Array.Empty<object>()).OfType<MenuItem>().ToList();
        if (items.Count != built.Count || !items.Select(Says).SequenceEqual(built.Select(Says)))
        {
            var source = fresh.ItemsSource;
            fresh.ItemsSource = null;
            shown.ItemsSource = source;
            return;
        }
        foreach (var (item, next) in items.Zip(built))
        {
            Sync(item, next);
            MergeItems(item, next);
        }
    }

    /// What a menu item says, explanation and all: [Ui.MenuAction] shows one under a disabled
    /// item, and puts its label in the item's accessible name.
    private static string Says(MenuItem item) => string.Join('\u001f',
        AutomationProperties.GetName(item) ?? item.Header as string, item.IsEnabled, AutomationProperties.GetHelpText(item));
}
