using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Config;

// The controller document: which machines exist, where they are, how to reach them, how to wake
// them, and which systems each of them can boot into.
//
// It is the same document the Mac and the phone read, byte for byte, and every device is a peer
// that may edit and publish it. So nothing here may invent a key of its own, refuse a key it has
// not heard of, or drop one on the way through an editor: what one device does not understand,
// another one wrote on purpose.
//
// What is deliberately NOT in here: anything private to one device. Key paths, ssh aliases, which
// machine "I" am and how to run an agent locally live in [Bindings], because publishing them would
// hand every other device a path that does not exist there.

public sealed record ControllerConfig
{
    public int? Version { get; init; }
    public IReadOnlyList<MachineConfig> Machines { get; init; } = Array.Empty<MachineConfig>();
    /// Where the LANs are. Optional: a one-house setup never needs it.
    public IReadOnlyList<SiteConfig> Sites { get; init; } = Array.Empty<SiteConfig>();
    /// Deprecated in the shared document, still read: a device that runs an agent for itself now
    /// says so in its own bindings.
    public LocalConfig? Local { get; init; }
    public ControllerIdentity Identity { get; init; } = new();
    public string? UpdateRepo { get; init; }
    private IReadOnlyList<string> InputProblems { get; init; } = Array.Empty<string>();

    public const string DefaultUpdateRepo = "x1f4r/legion-control";

    public static ControllerConfig From(Value json) => new()
    {
        Version = json["version"].AsInt(),
        Machines = json["machines"].Map(MachineConfig.From),
        Sites = json["sites"].Map(SiteConfig.From),
        Local = LocalConfig.From(json["local"]),
        Identity = ControllerIdentity.From(json["controller"]),
        UpdateRepo = json["appUpdates"]["githubRepo"].AsText(),
        InputProblems = ValidateInput(json),
    };

    private static IReadOnlyList<string> ValidateInput(Value json)
    {
        var problems = new List<string>();
        if (!json["machines"].IsArray) problems.Add("machines must be an array.");
        if (json["sites"].Exists && !json["sites"].IsArray) problems.Add("sites must be an array.");
        if (json.Properties().Any(p => p.Key == "controller"))
        {
            var identity = json["controller"];
            if (!identity.IsObject || identity["id"].Raw.ValueKind != System.Text.Json.JsonValueKind.String
                || !CommandSurface.IsValidToken(identity["id"].AsText()))
                problems.Add("controller must contain a nonempty string id.");
            if (identity["revision"].Raw.ValueKind != System.Text.Json.JsonValueKind.Number
                || !identity["revision"].Raw.TryGetInt64(out var revision) || revision < 0)
                problems.Add("controller.revision must be a nonnegative integer.");
            if (identity.Properties().Any(p => p.Key == "lineage") && (!identity["lineage"].IsArray
                || identity["lineage"].AsArray().Any(item => item.Raw.ValueKind != System.Text.Json.JsonValueKind.String)))
                problems.Add("controller.lineage must be an array of hash strings.");
        }
        return problems;
    }

    public string Repo => string.IsNullOrWhiteSpace(UpdateRepo) ? DefaultUpdateRepo : UpdateRepo!;

    public MachineConfig? Machine(string id) => Machines.FirstOrDefault(machine => machine.Id == id);

    public SiteConfig? Site(string id) => Sites.FirstOrDefault(site => site.Id == id);

    public bool IsEmpty => Machines.Count == 0;

    /// What is wrong with the document, in the words of the thing that has to change.
    ///
    /// A document that fails this never replaces one that did: a machine with no systems, or an
    /// unparseable hardware address, has no sensible default to fall back to.
    public IReadOnlyList<string> Problems()
    {
        var problems = new List<string>(Identity.Problems());
        problems.AddRange(InputProblems);
        if (Version != 1) problems.Add("The shared setup must use document version 1.");
        var seenSites = new HashSet<string>(StringComparer.Ordinal);
        foreach (var site in Sites)
        {
            if (!CommandSurface.IsValidToken(site.Id)) problems.Add("A site has an invalid id.");
            else if (!seenSites.Add(site.Id)) problems.Add($"Two sites share the id \"{site.Id}\".");
        }

        var seenMachines = new HashSet<string>(StringComparer.Ordinal);
        foreach (var machine in Machines)
        {
            if (!CommandSurface.IsValidToken(machine.Id))
            {
                problems.Add("A machine has an empty id.");
                continue;
            }
            if (!seenMachines.Add(machine.Id))
            {
                problems.Add($"Two machines share the id \"{machine.Id}\". Ids have to be unique.");
            }
            if (machine.Site is { } site && Site(site) is null)
            {
                problems.Add($"Machine \"{machine.Id}\" names the site \"{site}\", which is not in sites.");
            }

            var seenSystems = new HashSet<string>(StringComparer.Ordinal);
            foreach (var system in machine.Systems)
            {
                if (!CommandSurface.IsValidToken(system.Id))
                {
                    problems.Add($"A system on machine \"{machine.Id}\" has an empty id.");
                    continue;
                }
                if (!seenSystems.Add(system.Id))
                {
                    problems.Add($"Machine \"{machine.Id}\" has two systems with the id \"{system.Id}\".");
                }
            }

            var seenEndpoints = new HashSet<string>(StringComparer.Ordinal);
            foreach (var endpoint in machine.Endpoints)
            {
                if (!seenEndpoints.Add(endpoint.Id)) problems.Add($"Machine {machine.Id} has duplicate endpoint ids.");
                if (endpoint.Host.Length == 0 || endpoint.Host.StartsWith('-') || endpoint.Host.Any(char.IsWhiteSpace))
                    problems.Add($"Endpoint {endpoint.Id} needs a valid host or SSH alias.");
                if (endpoint.Port is < 1 or > 65535) problems.Add($"Endpoint {endpoint.Id} has an invalid port.");
            }

            if (machine.Wake is { } wake)
            {
                if (wake.MacBytes is null)
                {
                    problems.Add($"The wake address \"{wake.Mac}\" on machine \"{machine.Id}\" is not a six byte hardware address.");
                }
                problems.AddRange(HelperProblems(machine));
            }
        }
        return problems;
    }

    /// What is worth saying about the document without refusing it.
    ///
    /// The line between this and [Problems] is what a person can still do. A machine with no
    /// systems yet is half-written, not broken, and refusing the whole document over it would take
    /// away the very screen where the systems are added. A helper cycle, on the other hand, cannot
    /// be acted on at all.
    public IReadOnlyList<string> Warnings()
    {
        var warnings = new List<string>();
        foreach (var machine in Machines)
        {
            if (machine.Systems.Count == 0)
            {
                warnings.Add($"Machine \"{machine.Id}\" lists no systems, so nothing can be run on it yet.");
            }
            if (machine.Routes.Count == 0)
            {
                warnings.Add($"Machine \"{machine.Id}\" has no address, so there is no way to reach it yet.");
            }
            foreach (var system in machine.Systems.Where(system => system.Agent.Count == 0))
            {
                warnings.Add($"System \"{system.Id}\" on machine \"{machine.Id}\" has no agent command, so it cannot be driven.");
            }
            if (machine.Wake is not { } wake) continue;
            foreach (var helper in wake.EffectiveHelpers)
            {
                var helperMachine = Machine(helper.Machine);
                if (helperMachine is null) continue;
                if (helperMachine.Site is not null && machine.Site is not null && helperMachine.Site != machine.Site)
                {
                    warnings.Add(
                        $"The helper \"{helper.Machine}\" for \"{machine.Id}\" is at another site. That only works if its action reaches the target's network another way, through a router or a VPN.");
                }
                if (helperMachine.AlwaysOn != true)
                {
                    warnings.Add(
                        $"The helper \"{helper.Machine}\" for \"{machine.Id}\" is not marked as always on, so it may be asleep when it is needed.");
                }
            }
            if (wake.EffectiveHelpers.Count == 0 && machine.Site is null && wake.LanPrefix is null)
            {
                warnings.Add(
                    $"Machine \"{machine.Id}\" can only be woken by a device that is already on its network: it names no site, no lanPrefix and no helper.");
            }
        }
        return warnings;
    }

    /// Helper rules that block the editor: the helper has to exist, must not be the machine itself,
    /// and must not take part in a cycle of helpers.
    private IEnumerable<string> HelperProblems(MachineConfig machine)
    {
        var wake = machine.Wake!;
        // A client from before the ordered list reads only the singular key. Writing a different
        // machine there than the list starts with would send those clients to a helper this
        // document does not actually prefer, which is worse than having no list at all.
        if (wake.Helpers.Count > 0 && wake.Helper is { } alias && alias != wake.Helpers[0])
        {
            yield return $"The wake helper alias on \"{machine.Id}\" names {alias.Machine}/{alias.Action}, but the ordered list starts with {wake.Helpers[0].Machine}/{wake.Helpers[0].Action}. They have to agree.";
        }
        foreach (var helper in machine.Wake!.EffectiveHelpers)
        {
            if (string.IsNullOrWhiteSpace(helper.Machine) || string.IsNullOrWhiteSpace(helper.Action))
            {
                yield return $"A wake helper on \"{machine.Id}\" is missing a machine or an action.";
                continue;
            }
            if (!CommandSurface.IsValidToken(helper.Action))
            {
                yield return $"The wake action \"{helper.Action}\" on \"{machine.Id}\" is not a plain token.";
            }
            if (helper.Machine == machine.Id)
            {
                yield return $"Machine \"{machine.Id}\" lists itself as its own wake helper.";
                continue;
            }
            if (Machine(helper.Machine) is null)
            {
                yield return $"The wake helper \"{helper.Machine}\" for \"{machine.Id}\" is not a machine in this document.";
                continue;
            }
            if (HelperCycle(machine.Id, helper.Machine, new HashSet<string>(StringComparer.Ordinal) { machine.Id }))
            {
                yield return $"The wake helpers for \"{machine.Id}\" form a cycle through \"{helper.Machine}\".";
            }
        }
    }

    private bool HelperCycle(string origin, string current, HashSet<string> seen)
    {
        if (current == origin) return true;
        if (!seen.Add(current)) return false;
        var machine = Machine(current);
        foreach (var helper in machine?.Wake?.EffectiveHelpers ?? Array.Empty<WakeHelper>())
        {
            if (helper.Machine == origin) return true;
            if (HelperCycle(origin, helper.Machine, seen)) return true;
        }
        return false;
    }
}

/// Where a group of machines physically is, and which addresses a device on that network has.
public sealed record SiteConfig(string Id, string Name, IReadOnlyList<string> LanPrefixes, IReadOnlyList<string> Broadcast)
{
    public static SiteConfig From(Value json) => new(
        json["id"].AsText() ?? "",
        json["name"].AsText() ?? json["id"].AsText() ?? "",
        json["lanPrefixes"].AsStringList(),
        json["broadcast"].AsStringList());
}

/// One physical box: one network card, one hardware address, one thing to wake.
public sealed record MachineConfig
{
    public string Id { get; init; } = "";
    public string Name { get; init; } = "";
    /// Deprecated in the shared document. An ssh alias and a key path belong to one device, so they
    /// belong in that device's bindings; this is still read for documents written before that.
    public SshTarget? Ssh { get; init; }
    public IReadOnlyList<EndpointConfig> Endpoints { get; init; } = Array.Empty<EndpointConfig>();
    public WakeConfig? Wake { get; init; }
    public IReadOnlyList<SystemConfig> Systems { get; init; } = Array.Empty<SystemConfig>();
    /// Which site this machine is at, when the document names sites.
    public string? Site { get; init; }
    /// A hint, not a promise: whether this machine can be relied on to answer at any hour. Only
    /// ever changes what the wake rows say.
    public bool? AlwaysOn { get; init; }

    public static MachineConfig From(Value json)
    {
        var id = json["id"].AsText() ?? "";
        return new MachineConfig
        {
            Id = id,
            Name = json["name"].AsText() ?? id,
            Ssh = SshTarget.From(json["ssh"]),
            Endpoints = json["endpoints"].Map(EndpointConfig.From),
            Wake = WakeConfig.From(json["wake"]),
            Systems = json["systems"].Map(SystemConfig.From),
            Site = json["site"].AsText(),
            AlwaysOn = json["alwaysOn"].AsBool(),
        };
    }

    public SystemConfig? System(string id) => Systems.FirstOrDefault(system => system.Id == id);

    /// The first configured system running the given platform. The second chance a status gets to
    /// be matched: an agent whose system id we do not know still says which platform it is.
    public SystemConfig? SystemFor(Platform platform) => Systems.FirstOrDefault(system => system.Platform == platform);

    /// Every way this device could reach the machine, in document order.
    ///
    /// Explicit endpoints are the routes for a modern document. The deprecated shared `ssh` block
    /// is used only when there are no endpoints: an old alias may contain a ProxyCommand that wakes
    /// a sleeping machine, so trying it after modern endpoints fail would turn an ordinary status
    /// poll into a wake request. A private bindings alias is still an explicit device-level override
    /// and is added ahead of this list by [Bindings.RoutesFor].
    ///
    /// The key is carried down onto every endpoint: an address written for the phone is still
    /// dialled with this device's key, and dropping the identity on an explicit endpoint is how a
    /// machine that works through the alias fails through its address.
    public IReadOnlyList<MachineRoute> Routes
    {
        get
        {
            var routes = new List<MachineRoute>();
            var seen = new HashSet<string>(StringComparer.Ordinal);
            var identity = Ssh?.IdentityFile;

            void Add(MachineRoute route)
            {
                if (!seen.Add($"{route.Target.Destination}:{route.Target.Port ?? 22}")) return;
                routes.Add(route);
            }

            foreach (var endpoint in Endpoints)
            {
                Add(new MachineRoute(
                    endpoint.Id,
                    endpoint.Label ?? endpoint.Host,
                    new SshTarget
                    {
                        Host = endpoint.Host,
                        User = endpoint.User,
                        Port = endpoint.Port,
                        IdentityFile = identity,
                    },
                    endpoint.System,
                    endpoint.Kind ?? "remote"));
            }
            if (routes.Count == 0 && Ssh is { } ssh && !string.IsNullOrWhiteSpace(ssh.Host))
            {
                Add(new MachineRoute("ssh", ssh.Display, ssh, null, "alias"));
            }
            return routes;
        }
    }
}

/// One way to reach a machine: an address, a name for it, the system it is known to answer as when
/// the document pins one, and whether it is a LAN address or a remote one.
public sealed record MachineRoute(string Id, string Label, SshTarget Target, string? SystemId, string Kind = "remote")
{
    public bool IsLan => string.Equals(Kind, "lan", StringComparison.OrdinalIgnoreCase);
}

/// Everything ssh needs to dial one address.
public sealed record SshTarget
{
    public string Host { get; init; } = "";
    public string? User { get; init; }
    public int? Port { get; init; }
    public string? IdentityFile { get; init; }

    public static SshTarget? From(Value json)
    {
        if (!json.IsObject) return null;
        var host = json["host"].AsText();
        if (host is null) return null;
        return new SshTarget
        {
            Host = host,
            User = json["user"].AsText(),
            Port = json["port"].AsInt(),
            IdentityFile = json["identityFile"].AsText(),
        };
    }

    /// `user@host`, or just the host when the document leaves the user to ssh.
    public string Destination => string.IsNullOrWhiteSpace(User) ? Host : $"{User}@{Host}";

    /// What the network row shows: the destination, and the port when it is not the usual one.
    public string Display => Port is { } port && port != 22 ? $"{Destination}:{port}" : Destination;

    /// The flags that go in front of the destination.
    public IReadOnlyList<string> SshOptions
    {
        get
        {
            var options = new List<string>();
            if (Port is { } port) options.AddRange(new[] { "-p", port.ToString() });
            if (!string.IsNullOrWhiteSpace(IdentityFile))
            {
                options.AddRange(new[] { "-i", AppPaths.ExpandHome(IdentityFile!) });
            }
            return options;
        }
    }
}

/// An address to dial directly.
public sealed record EndpointConfig
{
    public string Id { get; init; } = "";
    /// lan | remote. Routing only: a LAN address is tried first from the machine's own site.
    public string? Kind { get; init; }
    public string Host { get; init; } = "";
    public int? Port { get; init; }
    public string? User { get; init; }
    /// Which system answers here, when only one does. A hint for ordering, never a claim: a status
    /// that says otherwise is still accepted.
    public string? System { get; init; }
    public string? Label { get; init; }

    public static EndpointConfig From(Value json)
    {
        var host = json["host"].AsText() ?? "";
        return new EndpointConfig
        {
            Host = host,
            Id = json["id"].AsText() ?? host,
            Kind = json["kind"].AsText(),
            Port = json["port"].AsInt(),
            User = json["user"].AsText(),
            System = json["system"].AsText(),
            Label = json["label"].AsText(),
        };
    }
}

/// A machine that can send a magic packet on the target's own network, and the action on it that
/// does so.
public sealed record WakeHelper(string Machine, string Action)
{
    public static WakeHelper? From(Value json)
    {
        if (!json.IsObject) return null;
        var machine = json["machine"].AsText();
        var action = json["action"].AsText();
        return machine is null || action is null ? null : new WakeHelper(machine, action);
    }
}

/// Wake on LAN. A machine without this block simply has no wake action.
public sealed record WakeConfig
{
    public string Mac { get; init; } = "";
    public IReadOnlyList<string> Broadcast { get; init; } = new[] { "255.255.255.255" };
    public IReadOnlyList<int> Ports { get; init; } = new[] { 9, 7 };
    public WakeProbe? Probe { get; init; }
    /// Used only when the machine names no site. A prefix a device's own address starts with when
    /// it is on this machine's network.
    public string? LanPrefix { get; init; }
    /// The compatibility alias for the first helper, kept so a 1.2 client that only knows the
    /// singular key still has one.
    public WakeHelper? Helper { get; init; }
    /// Helpers in the order to try them.
    public IReadOnlyList<WakeHelper> Helpers { get; init; } = Array.Empty<WakeHelper>();
    /// Which system to boot into once the machine is awake, when the document asks for one.
    public string? BootTarget { get; init; }

    public static WakeConfig? From(Value json)
    {
        if (!json.IsObject) return null;
        var mac = json["mac"].AsText();
        if (mac is null) return null;
        var broadcast = json["broadcast"].AsStringList();
        var ports = json["ports"].Map(port => port.AsInt()).Where(port => port is not null).Select(port => port!.Value).ToList();
        return new WakeConfig
        {
            Mac = mac,
            Broadcast = broadcast.Count > 0 ? broadcast : new[] { "255.255.255.255" },
            Ports = ports.Count > 0 ? ports : new[] { 9, 7 },
            Probe = WakeProbe.From(json["probe"]),
            LanPrefix = json["lanPrefix"].AsText(),
            Helper = WakeHelper.From(json["helper"]),
            Helpers = json["helpers"].Map(WakeHelper.From).Where(helper => helper is not null).Select(helper => helper!).ToList(),
            BootTarget = json["bootTarget", "boot"].AsText(),
        };
    }

    /// The helpers to walk, in order: the list when there is one, and the singular alias otherwise.
    public IReadOnlyList<WakeHelper> EffectiveHelpers =>
        Helpers.Count > 0 ? Helpers : (Helper is null ? Array.Empty<WakeHelper>() : new[] { Helper });

    /// The six bytes of the hardware address, or null when it is not one.
    public byte[]? MacBytes
    {
        get
        {
            var parts = Mac.Split(':', '-');
            if (parts.Length != 6) return null;
            var bytes = new byte[6];
            for (var index = 0; index < 6; index += 1)
            {
                if (!byte.TryParse(parts[index], System.Globalization.NumberStyles.HexNumber, null, out bytes[index]))
                {
                    return null;
                }
            }
            return bytes;
        }
    }
}

public sealed record WakeProbe(string Host, int Port)
{
    public static WakeProbe? From(Value json)
    {
        if (!json.IsObject) return null;
        var host = json["host"].AsText();
        return host is null ? null : new WakeProbe(host, json["port"].AsInt() ?? 22);
    }
}

/// One operating system installed on a machine. A dual boot machine has two of these and only one
/// of them is up at a time.
public sealed record SystemConfig
{
    public string Id { get; init; } = "";
    public string Name { get; init; } = "";
    public Platform Platform { get; init; } = Platform.Linux;
    /// The exact argv run over ssh, quoted for that system's shell on the way out.
    public IReadOnlyList<string> Agent { get; init; } = Array.Empty<string>();
    public string? Symbol { get; init; }
    /// Which shell the far side hands an ssh command to, when it is not the one the platform
    /// implies. The only system that needs this is a Windows one whose OpenSSH DefaultShell has
    /// been pointed at PowerShell.
    public RemoteShell? Shell { get; init; }
    /// Whether this system's key is restricted to a forced command.
    ///
    /// A restricted key never reaches a shell: sshd hands the whole SSH_ORIGINAL_COMMAND to the
    /// dispatcher, which parses a POSIX argv grammar itself. So the command has to be serialised
    /// as POSIX argv whatever platform the far side runs, and a PowerShell call operator would be
    /// rejected by that grammar rather than executed.
    public bool Restricted { get; init; }

    public static SystemConfig From(Value json)
    {
        var platform = Platforms.Parse(json["platform"].AsText()) ?? Platform.Linux;
        return new SystemConfig
        {
            Id = json["id"].AsText() ?? "",
            Platform = platform,
            Name = json["name"].AsText() ?? platform.DefaultName(),
            Agent = json["agent"].AsStringList(),
            Symbol = json["symbol"].AsText(),
            Shell = RemoteShells.Parse(json["shell"].AsText()),
            Restricted = json["restricted"].AsBool() ?? false,
        };
    }

    /// The shell to serialise for: POSIX argv when the key is restricted, whatever the document
    /// names otherwise, and the platform's usual shell when it names nothing.
    public RemoteShell RemoteShell => Restricted ? Transport.RemoteShell.Posix : Shell ?? RemoteShells.Default(Platform);
}

/// The device the app itself runs on, looked after by an agent running here rather than over ssh.
/// Deprecated in the shared document: this is a per-device fact and now lives in [Bindings].
public sealed record LocalConfig(bool Enabled, string Name, string Agent)
{
    public const string DefaultAgentPath = "~/.legion-control/agent/src/index.mjs";

    public static LocalConfig? From(Value json)
    {
        if (!json.IsObject) return null;
        return new LocalConfig(
            json["enabled"].AsBool() ?? true,
            json["name"].AsText() ?? "This desktop",
            json["agent"].AsText() ?? DefaultAgentPath);
    }

    public string AgentPath => AppPaths.ExpandHome(Agent);
}
