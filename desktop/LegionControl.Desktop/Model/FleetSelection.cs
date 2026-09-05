namespace LegionControl.Desktop.Model;

public sealed class FleetSelection
{
    public string? SelectedId { get; private set; }

    public void Select(string machineId) => SelectedId = machineId;

    public void Reconcile(IReadOnlyList<string> machineIds)
    {
        if (SelectedId is null || !machineIds.Contains(SelectedId, StringComparer.Ordinal))
            SelectedId = machineIds.FirstOrDefault();
    }
}
