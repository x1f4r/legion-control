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

    public const string DefaultUpdateRepo = "x1f4r/legion-control";

    public static ControllerConfig From(Value json) => new()
    {
        Version = json["version"].AsInt(),
        Machines = json["machines"].Map(MachineConfig.From),
        Sites = json["sites"].Map(SiteConfig.From),
        Local = LocalConfig.From(json["local"]),
        Identity = ControllerIdentity.From(json["controller"]),
        UpdateRepo = json["appUpdates"]["githubRepo"].AsText(),
    };

    public string Repo => string.IsNullOrWhiteSpace(UpdateRepo) ? DefaultUpdateRepo : UpdateRepo!;

    public MachineConfig? Machine(string id) => Machines.FirstOrDefault(machine => machine.Id == id);

    public SiteConfig? Site(string id) => Sites.FirstOrDefault(site => site.Id == id);

    public bool IsEmpty => Machines.Count == 0;

    /// The configuration without the machines at [indexes] in the document's `machines`: the ones a
    /// section 9.7 error is inside, which an applied document keeps but this device neither dials
    /// nor runs (contract section 9.7, applied intake). What is wrong with a document is
    /// [DocumentAcceptance]'s to say, never this model's: it reads leniently and cannot see a
    /// member written twice or a number spelled as a string.
    public ControllerConfig WithoutMachinesAt(IReadOnlySet<int> indexes) =>
        indexes.Count == 0 ? this : this with { Machines = Machines.Where((_, index) => !indexes.Contains(index)).ToList() };
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
                        UndialableHost = endpoint.UndialableHost,
                        UndialableUser = endpoint.UndialableUser,
                        UndialablePort = endpoint.UndialablePort,
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
    /// The host, user or port exactly as the document wrote it, when it is not the JSON value
    /// section 15.6 reads as one at all: a host or a user that is not a string, a port that is not
    /// an integer from 1 to 65535.
    ///
    /// [Value] coerces on the way in, which is right for an agent's reply and wrong for an
    /// address. It reads the string "22" and 22.5 as the port 22, 1e10 as no port and so the usual
    /// one, and the number 42 as the user "42". The schema refuses all of these, but a document
    /// this device already applies is kept whatever it holds, so the dialler cannot rely on that:
    /// [Addresses.Refusal] refuses a target that carries one, and it is never dialled.
    public string? UndialableHost { get; init; }
    public string? UndialableUser { get; init; }
    public string? UndialablePort { get; init; }

    public static SshTarget? From(Value json)
    {
        if (!json.IsObject) return null;
        var host = json["host"].AsText();
        if (host is null) return null;
        return new SshTarget
        {
            Host = host,
            User = json["user"].AsString(),
            Port = json["port"].AsInt(),
            IdentityFile = json["identityFile"].AsText(),
            UndialableHost = Undialable(json, "host"),
            UndialableUser = Undialable(json, "user"),
            UndialablePort = Undialable(json, "port"),
        };
    }

    /// The JSON written for [field], when the document has the field and it is not a value
    /// section 15.6 reads as that field; null when it is absent or is one. A string host or user
    /// is passed on as it stands and judged by [Addresses.Refusal] like any other.
    public static string? Undialable(Value holder, string field)
    {
        if (!holder.IsObject || !holder.Raw.TryGetProperty(field, out var written)) return null;
        var readable = field == "port" ? Addresses.IsPort(written) : written.ValueKind == System.Text.Json.JsonValueKind.String;
        return readable ? null : written.GetRawText();
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
    /// What [SshTarget.UndialableHost] and its two siblings say, carried onto the route.
    public string? UndialableHost { get; init; }
    public string? UndialableUser { get; init; }
    public string? UndialablePort { get; init; }

    public static EndpointConfig From(Value json)
    {
        var host = json["host"].AsText() ?? "";
        return new EndpointConfig
        {
            Host = host,
            Id = json["id"].AsText() ?? host,
            Kind = json["kind"].AsText(),
            Port = json["port"].AsInt(),
            User = json["user"].AsString(),
            System = json["system"].AsText(),
            Label = json["label"].AsText(),
            UndialableHost = SshTarget.Undialable(json, "host"),
            UndialableUser = SshTarget.Undialable(json, "user"),
            UndialablePort = SshTarget.Undialable(json, "port"),
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
    /// The hardware address a magic packet carries, or null when the block names none. A block
    /// without one is still a wake block: nothing can wake the machine directly, and its helpers
    /// still can (contract section 9.7, `wake-no-mac`).
    public string? Mac { get; init; }
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

    /// The six bytes of the hardware address, or null when there is none or it is not one.
    public byte[]? MacBytes
    {
        get
        {
            if (Mac is null) return null;
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
    /// The platform, read leniently: a document that says `win` still finds the Windows system to
    /// boot into. Never what decides a grammar for a system read from a document; see
    /// [PlatformAsWritten].
    public Platform Platform { get; init; } = Platform.Linux;
    /// The `platform` the document names, exactly as it is written, or null when it names none or
    /// names it with anything but a string. A system built here, from a form or a template, has
    /// none either, and its grammar is worked out from [Platform].
    ///
    /// The grammar reads this rather than [Platform] because to every client only the string
    /// "windows" is Windows. Reading `win` or ` Windows ` as Windows here would quote for cmd a
    /// line that every other client, and this device's own launch record, writes for a POSIX
    /// shell.
    public string? PlatformAsWritten { get; init; }
    /// The exact argv run over ssh, quoted for that system's shell on the way out. In the model the
    /// machines are driven from, this is the argv [LaunchGate] approved, and empty when there is none.
    public IReadOnlyList<string> Agent { get; init; } = Array.Empty<string>();
    public string? Symbol { get; init; }
    /// The `shell` the document names, exactly as it is written, or null when it names none. A
    /// value that is not a string is kept as its JSON text, which can never spell one of the three
    /// shell names.
    ///
    /// Kept raw rather than parsed, because a value this app does not know is not a shell to
    /// guess at: the system's grammar is then refused (`unknown-shell`) and it is not driven. That
    /// goes for `shell: 5` as much as for `shell: "pwsh"`; reading the number as no shell at all
    /// would quote for the platform's default shell a system every other client refuses. The
    /// only system that needs one at all is a Windows one whose OpenSSH DefaultShell has been
    /// pointed at PowerShell.
    public string? Shell { get; init; }
    /// Whether this system's key is restricted to a forced command: the JSON value true, and
    /// nothing that merely reads as true.
    ///
    /// A restricted key never reaches a shell: sshd hands the whole SSH_ORIGINAL_COMMAND to the
    /// dispatcher, which parses an argv grammar of its own. So the command is serialised for the
    /// dispatcher whatever platform the far side runs.
    public bool Restricted { get; init; }
    /// How this device drives the system, as [LaunchGate] decided it. Never read from a document:
    /// a system built here, from a form or a template, runs what it was given.
    public LaunchState LaunchState { get; init; } = LaunchState.Approved;

    private CommandGrammar? _grammar;

    /// The grammar this system's command lines are written in (contract section 15.1), as the
    /// document gives it. Serialisation reads this and nothing else; the launch gate writes the
    /// grammar it approved here, and without one it is worked out from the fields above.
    public CommandGrammar Grammar
    {
        get => _grammar ?? RemoteShells.Resolve(PlatformAsWritten ?? Platform.Wire(), Shell, Restricted, restrictedSession: false, Agent.Count);
        init => _grammar = value;
    }

    /// The grammar on a route whose last status said whether this key runs in a restricted
    /// session. A status can only ever move a system to the dispatcher, never away from it.
    public CommandGrammar GrammarOn(bool restrictedSession) => restrictedSession
        ? RemoteShells.Resolve(null, null, restricted: true, restrictedSession: true, Agent.Count)
        : Grammar;

    /// A system as the document writes it. The three fields a grammar is worked out from go in the
    /// way [LaunchProjection] reads them, so a system that is not gated is quoted exactly as the
    /// launch record would have it: only the string "windows" is Windows, only true is
    /// restricted, and a shell that is there but not a string is no shell this app knows.
    public static SystemConfig From(Value json)
    {
        var written = json["platform"].Raw.ValueKind == System.Text.Json.JsonValueKind.String ? json["platform"].Raw.GetString() : null;
        // Anything but a string is no platform, and Linux, whose default grammar is the one the
        // contract gives a system that names none.
        var platform = Platforms.Parse(written) ?? Platform.Linux;
        var shell = json["shell"];
        return new SystemConfig
        {
            Id = json["id"].AsText() ?? "",
            Platform = platform,
            PlatformAsWritten = written,
            Name = json["name"].AsText() ?? platform.DefaultName(),
            Agent = AgentArgv(json["agent"]),
            Symbol = json["symbol"].AsText(),
            Shell = !shell.Exists ? null
                : shell.Raw.ValueKind == System.Text.Json.JsonValueKind.String ? shell.Raw.GetString() : shell.Raw.GetRawText(),
            Restricted = json["restricted"].Raw.ValueKind == System.Text.Json.JsonValueKind.True,
        };
    }

    /// The agent argv exactly as written: every element a string, kept even when it is empty, so
    /// the command line refuses it rather than this app quietly sending a different command. An
    /// argv with anything but strings in it is no command at all, and the system is not driven.
    private static IReadOnlyList<string> AgentArgv(Value json)
    {
        if (!json.IsArray) return Array.Empty<string>();
        var items = json.AsArray();
        if (items.Any(item => item.Raw.ValueKind != System.Text.Json.JsonValueKind.String)) return Array.Empty<string>();
        return items.Select(item => item.Raw.GetString()!).ToList();
    }
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
