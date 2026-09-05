using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// A change to the setup document, worked out in full before anything is written.
///
/// Every edit here produces one of these first: the document as it would be, what this app would
/// refuse about it, and what it would change in the machines the user can see. Nothing is written
/// from a control that did not first show this.
public sealed record EditPlan(
    string Description,
    string Text,
    IReadOnlyList<string> Problems,
    SetupConflictPreview Preview)
{
    public bool IsApplicable => Problems.Count == 0;
}

/// Structured edits to the setup document, made on the document as it is rather than on the model
/// this app decoded from it.
///
/// The distinction matters: the document may carry keys written for the phone, for a future agent,
/// or by hand, and re-serialising this app's own idea of the setup would quietly drop every one of
/// them. So the JSON tree is edited in place and everything untouched stays exactly as it was,
/// including the user's key order.
public static class ControllerEditor
{
    private static readonly JsonSerializerOptions Writing = new() { WriteIndented = true };

    /// Applies a change to the document text and works out what it would mean.
    public static EditPlan Plan(string currentText, ControllerConfig? current, string description, Action<JsonObject> change)
    {
        JsonObject root;
        try
        {
            root = JsonNode.Parse(string.IsNullOrWhiteSpace(currentText) ? "{}" : currentText) as JsonObject
                   ?? new JsonObject();
        }
        catch (JsonException error)
        {
            return new EditPlan(description, currentText, new[] { $"The document is not valid JSON: {error.Message}" },
                SetupConflictPreview.Between(current, new ControllerConfig()));
        }

        try
        {
            change(root);
        }
        catch (Exception error)
        {
            return new EditPlan(description, currentText, new[] { error.Message },
                SetupConflictPreview.Between(current, new ControllerConfig()));
        }

        var text = root.ToJsonString(Writing);
        var decoded = ControllerConfig.From(Value.Parse(text));
        return new EditPlan(description, text, decoded.Problems(), SetupConflictPreview.Between(current, decoded));
    }

    /// Adds or replaces one machine.
    public static Action<JsonObject> UpsertMachine(JsonObject machine) => root =>
    {
        var id = machine["id"]?.GetValue<string>();
        if (string.IsNullOrWhiteSpace(id)) throw new InvalidOperationException("A machine needs an id.");
        var machines = Array(root, "machines");
        for (var index = 0; index < machines.Count; index += 1)
        {
            if (machines[index] is JsonObject existing && existing["id"]?.GetValue<string>() == id)
            {
                machines[index] = machine.DeepClone();
                return;
            }
        }
        machines.Add(machine.DeepClone());
    };

    public static Action<JsonObject> RemoveMachine(string id) => root =>
    {
        var machines = Array(root, "machines");
        for (var index = machines.Count - 1; index >= 0; index -= 1)
        {
            if (machines[index] is JsonObject existing && existing["id"]?.GetValue<string>() == id)
            {
                machines.RemoveAt(index);
            }
        }
    };

    public static Action<JsonObject> UpsertSystem(string machineId, JsonObject system) => root =>
    {
        var machine = FindMachine(root, machineId);
        var id = system["id"]?.GetValue<string>();
        if (string.IsNullOrWhiteSpace(id)) throw new InvalidOperationException("A system needs an id.");
        var systems = Array(machine, "systems");
        for (var index = 0; index < systems.Count; index += 1)
        {
            if (systems[index] is JsonObject existing && existing["id"]?.GetValue<string>() == id)
            {
                systems[index] = system.DeepClone();
                return;
            }
        }
        systems.Add(system.DeepClone());
    };

    public static Action<JsonObject> RemoveSystem(string machineId, string systemId) => root =>
    {
        var machine = FindMachine(root, machineId);
        var systems = Array(machine, "systems");
        for (var index = systems.Count - 1; index >= 0; index -= 1)
        {
            if (systems[index] is JsonObject existing && existing["id"]?.GetValue<string>() == systemId)
            {
                systems.RemoveAt(index);
            }
        }
    };

    public static Action<JsonObject> UpsertEndpoint(string machineId, JsonObject endpoint) => root =>
    {
        var machine = FindMachine(root, machineId);
        var host = endpoint["host"]?.GetValue<string>();
        if (string.IsNullOrWhiteSpace(host)) throw new InvalidOperationException("An address needs a host.");
        var id = endpoint["id"]?.GetValue<string>() ?? host;
        var endpoints = Array(machine, "endpoints");
        for (var index = 0; index < endpoints.Count; index += 1)
        {
            if (endpoints[index] is JsonObject existing && (existing["id"]?.GetValue<string>() ?? existing["host"]?.GetValue<string>()) == id)
            {
                endpoints[index] = endpoint.DeepClone();
                return;
            }
        }
        endpoints.Add(endpoint.DeepClone());
    };

    public static Action<JsonObject> RemoveEndpoint(string machineId, string endpointId) => root =>
    {
        var machine = FindMachine(root, machineId);
        var endpoints = Array(machine, "endpoints");
        for (var index = endpoints.Count - 1; index >= 0; index -= 1)
        {
            if (endpoints[index] is JsonObject existing
                && (existing["id"]?.GetValue<string>() ?? existing["host"]?.GetValue<string>()) == endpointId)
            {
                endpoints.RemoveAt(index);
            }
        }
    };

    public static Action<JsonObject> UpsertSite(JsonObject site) => root =>
    {
        var id = site["id"]?.GetValue<string>();
        if (string.IsNullOrWhiteSpace(id)) throw new InvalidOperationException("A site needs an id.");
        var sites = Array(root, "sites");
        for (var index = 0; index < sites.Count; index += 1)
        {
            if (sites[index] is JsonObject existing && existing["id"]?.GetValue<string>() == id)
            {
                sites[index] = site.DeepClone();
                return;
            }
        }
        sites.Add(site.DeepClone());
    };

    public static Action<JsonObject> RemoveSite(string id) => root =>
    {
        var sites = Array(root, "sites");
        for (var index = sites.Count - 1; index >= 0; index -= 1)
        {
            if (sites[index] is JsonObject existing && existing["id"]?.GetValue<string>() == id) sites.RemoveAt(index);
        }
    };

    public static Action<JsonObject> SetMachineSite(string machineId, string? siteId) => root =>
    {
        var machine = FindMachine(root, machineId);
        if (siteId is null) machine.Remove("site");
        else machine["site"] = siteId;
    };

    public static Action<JsonObject> SetAlwaysOn(string machineId, bool? alwaysOn) => root =>
    {
        var machine = FindMachine(root, machineId);
        if (alwaysOn is null) machine.Remove("alwaysOn");
        else machine["alwaysOn"] = alwaysOn.Value;
    };

    /// Writes the ordered helper list, and the singular alias beside it.
    ///
    /// Both, always. A client from before the list exists reads only `helper`, and leaving it
    /// behind would silently take the wake away from every device that has not been updated yet.
    public static Action<JsonObject> SetWakeHelpers(string machineId, IReadOnlyList<WakeHelper> helpers) => root =>
    {
        var machine = FindMachine(root, machineId);
        if (machine["wake"] is not JsonObject wake)
        {
            throw new InvalidOperationException($"Machine \"{machineId}\" has no wake block to put helpers in.");
        }
        if (helpers.Count == 0)
        {
            wake.Remove("helpers");
            wake.Remove("helper");
            return;
        }
        wake["helpers"] = new JsonArray(helpers
            .Select(helper => (JsonNode)new JsonObject
            {
                ["machine"] = helper.Machine,
                ["action"] = helper.Action,
            })
            .ToArray());
        wake["helper"] = new JsonObject
        {
            ["machine"] = helpers[0].Machine,
            ["action"] = helpers[0].Action,
        };
    };

    /// Takes this device's private settings out of the shared document.
    ///
    /// Only ever run when a person asks for it. Removing a key changes the bytes, and therefore the
    /// hash, so doing it automatically would make one device's copy differ from everyone else's for
    /// a reason nobody chose.
    public static Action<JsonObject> MovePrivateSettingsOut() => root =>
    {
        root.Remove("local");
        foreach (var entry in Array(root, "machines"))
        {
            if (entry is not JsonObject machine) continue;
            if (machine["ssh"] is JsonObject ssh)
            {
                ssh.Remove("identityFile");
                if (ssh.Count == 0) machine.Remove("ssh");
            }
        }
    };

    private static JsonObject FindMachine(JsonObject root, string id)
    {
        foreach (var entry in Array(root, "machines"))
        {
            if (entry is JsonObject machine && machine["id"]?.GetValue<string>() == id) return machine;
        }
        throw new InvalidOperationException($"There is no machine \"{id}\" in the document.");
    }

    private static JsonArray Array(JsonObject owner, string name)
    {
        if (owner[name] is JsonArray existing) return existing;
        var created = new JsonArray();
        owner[name] = created;
        return created;
    }
}
