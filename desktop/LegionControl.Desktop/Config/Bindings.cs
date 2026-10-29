using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// What is true about this device and nobody else's.
///
/// The shared document describes the fleet and travels to every peer. None of the following can
/// travel: a key path that exists on one laptop, an ssh alias out of one `~/.ssh/config`, which
/// machine in the document happens to be the box this app is running on, and how to run an agent
/// here without ssh. Publishing any of it would hand every other device a path that is not there.
///
/// So the shared `machine.ssh.identityFile` and top-level `local` are read for documents written
/// before this split and are never written again, and never stripped either: removing a key would
/// change the bytes, and the bytes are the hash.
public sealed record Bindings
{
    /// The human name this device puts on the setup revisions it writes.
    public string? DeviceName { get; init; }
    /// Which machine in the document this device is, when it is one of them.
    public SelfBinding? Self { get; init; }
    /// How to run the agent here, without ssh. The argv is spawned directly: no shell, no quoting,
    /// nothing to serialise, because there is no far side to parse it.
    public IReadOnlyList<string> LocalAgent { get; init; } = Array.Empty<string>();
    /// The key offered to every machine that does not name its own.
    public string? IdentityFile { get; init; }
    /// Where this device keeps its pinned host keys. Set by a test harness through
    /// LEGION_CONTROL_HOME so an isolated run cannot touch the real one. Entries are never removed
    /// automatically, in a test run or out of one.
    public string? KnownHostsFile { get; init; }
    /// Per machine overrides, keyed by machine id.
    public IReadOnlyDictionary<string, MachineBinding> Machines { get; init; } =
        new Dictionary<string, MachineBinding>(StringComparer.Ordinal);
    /// Which site this device is at, when the user has said.
    ///
    /// Only ever a routing hint. Two houses behind two stock routers have the same private subnet,
    /// so an address prefix cannot prove where anything is; what proves which machine answered is
    /// the pinned host key, and that is checked on every connection regardless of this.
    public string? CurrentSite { get; init; }

    public static Bindings Empty => new();

    public static string DefaultPath => Path.Combine(AppPaths.Home, "bindings.json");

    /// Bindings that cannot be read are bindings at their defaults: ssh falls back to the user's
    /// own configuration, which is what an unconfigured install does anyway. A file that is there
    /// and is not bindings is moved aside before the next change is written, never written over
    /// (see [Change]).
    public static Bindings Load(string? path = null)
    {
        try
        {
            return Read(path ?? DefaultPath).Bindings;
        }
        catch (Exception)
        {
            return Empty;
        }
    }

    /// The bindings [path] holds, and why they are not bindings when the file is there but holds
    /// none. Throws when it cannot be read at all, which is not the same thing.
    private static (Bindings Bindings, string? Corrupt) Read(string path)
    {
        if (!File.Exists(path)) return (Empty, null);
        var json = Value.Parse(File.ReadAllText(path));
        if (!json.IsObject) return (Empty, "it is not a JSON object");
        var machines = new Dictionary<string, MachineBinding>(StringComparer.Ordinal);
        foreach (var entry in json["machines"].Properties())
        {
            machines[entry.Key] = new MachineBinding(
                entry.Value["identityFile"].AsText(),
                entry.Value["sshAlias"].AsText());
        }
        return (new Bindings
        {
            DeviceName = json["deviceName"].AsText(),
            Self = SelfBinding.From(json["self"]),
            LocalAgent = json["localAgent"]["argv"].AsStringList() is { Count: > 0 } argv
                ? argv
                : json["localAgent"].AsStringList(),
            IdentityFile = json["identityFile"].AsText(),
            KnownHostsFile = json["knownHostsFile"].AsText(),
            Machines = machines,
            CurrentSite = json["currentSite"].AsText(),
        }, null);
    }

    /// Applies [change] to the bindings as the file holds them now, under its lock, and writes the
    /// result. The window and the command line both change these, and a change made in one never
    /// puts back what the other changed since this process read them: an alias set on the command
    /// line survives the window saving this device's name. Returns the bindings now in force, and
    /// a sentence when they could not be saved.
    public static (Bindings Bindings, string? Problem) Change(Func<Bindings, Bindings> change, string? path = null)
    {
        path ??= DefaultPath;
        try
        {
            using var held = StateFile.Lock(path);
            var (current, corrupt) = Read(path);
            var next = change(current);
            if (corrupt is not null) StateFile.SetAside(path, corrupt);
            AtomicWrite.Text(path, next.Serialize());
            return (next, null);
        }
        catch (Exception error)
        {
            return (change(Load(path)), $"Could not save the private settings to {path}: {error.Message}");
        }
    }

    private string Serialize()
    {
        var payload = new JsonObject();
        if (DeviceName is not null) payload["deviceName"] = DeviceName;
        if (Self is not null)
        {
            payload["self"] = new JsonObject
            {
                ["machine"] = Self.Machine,
                ["system"] = Self.System,
            };
        }
        if (LocalAgent.Count > 0)
        {
            payload["localAgent"] = new JsonObject
            {
                ["argv"] = new JsonArray(LocalAgent.Select(part => (JsonNode)part!).ToArray()),
            };
        }
        if (IdentityFile is not null) payload["identityFile"] = IdentityFile;
        if (KnownHostsFile is not null) payload["knownHostsFile"] = KnownHostsFile;
        if (CurrentSite is not null) payload["currentSite"] = CurrentSite;
        if (Machines.Count > 0)
        {
            var machines = new JsonObject();
            foreach (var (id, binding) in Machines)
            {
                var entry = new JsonObject();
                if (binding.IdentityFile is not null) entry["identityFile"] = binding.IdentityFile;
                if (binding.SshAlias is not null) entry["sshAlias"] = binding.SshAlias;
                machines[id] = entry;
            }
            payload["machines"] = machines;
        }
        return payload.ToJsonString(new JsonSerializerOptions { WriteIndented = true });
    }

    /// These bindings with every field [after] changed from [before] taken from [after], and every
    /// other field left as it is here: what an editor that opened on [before] and saved [after]
    /// changed, applied to the bindings as the file holds them now. A field nobody touched keeps
    /// whatever another process set it to while the editor was open.
    public Bindings WithChanges(Bindings before, Bindings after) => this with
    {
        DeviceName = after.DeviceName == before.DeviceName ? DeviceName : after.DeviceName,
        Self = after.Self == before.Self ? Self : after.Self,
        LocalAgent = after.LocalAgent.SequenceEqual(before.LocalAgent) ? LocalAgent : after.LocalAgent,
        IdentityFile = after.IdentityFile == before.IdentityFile ? IdentityFile : after.IdentityFile,
        KnownHostsFile = after.KnownHostsFile == before.KnownHostsFile ? KnownHostsFile : after.KnownHostsFile,
        Machines = after.Machines.Count == before.Machines.Count && after.Machines.All(entry =>
            before.Machines.TryGetValue(entry.Key, out var was) && was == entry.Value) ? Machines : after.Machines,
        CurrentSite = after.CurrentSite == before.CurrentSite ? CurrentSite : after.CurrentSite,
    };

    /// The name to put on a setup revision this device writes.
    public string Device => DeviceName is { Length: > 0 } name ? name : Environment.MachineName;

    /// Where the pinned host keys are: this device's own setting, or wherever the run's state
    /// directory puts them.
    public string? KnownHosts => KnownHostsFile is { Length: > 0 } path
        ? AppPaths.ExpandHome(path)
        : AppPaths.KnownHostsOverride;

    /// What is wrong with these bindings, if anything.
    ///
    /// The one rule worth checking is the absence of a shell. A local agent is spawned as an argv
    /// and never handed to cmd or PowerShell: there is no far side to parse it, so a shell here
    /// would add a quoting layer with nothing on the other end of it.
    public static IReadOnlyList<string> Problems(Value json)
    {
        var problems = new List<string>();
        if (!json.IsObject) return new[] { "The private settings are not a JSON object." };
        var localAgent = json["localAgent"];
        if (localAgent.Exists)
        {
            if (!localAgent.IsObject || localAgent["argv"].AsStringList().Count == 0)
            {
                problems.Add("localAgent needs an argv: the binary to run and its arguments.");
            }
            foreach (var property in localAgent.Properties())
            {
                if (property.Key != "argv")
                {
                    problems.Add(
                        $"localAgent.{property.Key} is not a setting. A local agent is spawned as an argv; there is no shell on this path to configure.");
                }
            }
        }
        if (json["self"].Exists && json["self"]["machine"].AsText() is null)
        {
            problems.Add("self needs the id of the machine this device is.");
        }
        return problems;
    }

    /// Whether this device is the given machine.
    public bool IsSelf(string machineId) => Self?.Machine == machineId;

    /// Whether this device can drive itself without ssh.
    public bool CanRunLocally(string machineId) => IsSelf(machineId) && LocalAgent.Count > 0;

    /// The key to offer when dialling a machine, in the order the user's own settings win.
    public string? IdentityFor(string machineId, string? fromDocument)
    {
        if (Machines.TryGetValue(machineId, out var binding) && binding.IdentityFile is { Length: > 0 } own)
        {
            return own;
        }
        if (IdentityFile is { Length: > 0 } shared) return shared;
        if (!string.IsNullOrWhiteSpace(fromDocument)) return fromDocument;
        // The key this app generates for itself, when it has one. Never invented: a path that does
        // not exist would make ssh fail with a confusing message rather than fall back to the
        // user's own configuration.
        return File.Exists(AppPaths.DefaultIdentityFile) ? AppPaths.DefaultIdentityFile : null;
    }

    /// The alias to dial first, out of this device's own ssh config.
    public string? AliasFor(string machineId) =>
        Machines.TryGetValue(machineId, out var binding) ? binding.SshAlias : null;

    /// The routes to try for a machine, with this device's private settings applied.
    ///
    /// The alias goes first when there is one, because it is the entry the user maintains in their
    /// own ssh config and it usually knows about jump hosts and proxies this app does not. Every
    /// route inherits the key: dropping the identity on an explicit address is how a machine that
    /// works through its alias fails through its address.
    ///
    /// [machine] is the one [LaunchGate] built, so the document's key path here is one this device
    /// approved, never one a peer only wrote into the setup.
    public IReadOnlyList<MachineRoute> RoutesFor(MachineConfig machine)
    {
        var routes = new List<MachineRoute>();
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var identity = IdentityFor(machine.Id, machine.Ssh?.IdentityFile);

        void Add(MachineRoute route)
        {
            var target = route.Target with { IdentityFile = identity };
            if (!seen.Add($"{target.Destination}:{target.Port ?? 22}")) return;
            routes.Add(route with { Target = target });
        }

        if (AliasFor(machine.Id) is { Length: > 0 } alias)
        {
            Add(new MachineRoute("alias", alias, new SshTarget { Host = alias }, null, "alias"));
        }
        foreach (var route in machine.Routes) Add(route);
        return routes;
    }
}

public sealed record SelfBinding(string Machine, string? System)
{
    public static SelfBinding? From(Value json)
    {
        if (!json.IsObject) return null;
        var machine = json["machine"].AsText();
        return machine is null ? null : new SelfBinding(machine, json["system"].AsText());
    }
}

public sealed record MachineBinding(string? IdentityFile, string? SshAlias);
