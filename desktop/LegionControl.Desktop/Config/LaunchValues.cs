using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Config;

// Launch values and local approval (contract section 9.9).
//
// The shared setup carries the argv this device runs over ssh with its own key, and lineage only
// proves that a document claims an ancestor, not who wrote it. Before this, any key allowed to run
// `config set`, a restricted one included, could publish a descendant whose `agent` was `sh -c …`,
// and this app adopted it and ran it on every machine with a key that is not restricted. The
// document still replicates and applies unchanged; what changes is that this device runs only the
// launch values it approved itself, and everything else waits for a person here.
//
// This file is the function every implementation agrees on, ported from contract/tools/launch.mjs
// and held to it by contract/vectors/launch.json: which values are launch values and how they are
// read from a document, which offered values are accepted without asking, which wait and whether
// they could be approved at all, and what this device runs meanwhile. Values compare as JSON:
// arrays in order, strings by code point, with no case folding and no path or Unicode
// normalisation, because two spellings a person would call the same path are two different files
// to a file system that does not normalise.

/// Where a launch value sits.
public enum LaunchScope
{
    /// A system's `{agent, grammar}`: the argv run over ssh and how its line is quoted.
    System,
    /// A machine's deprecated `ssh.identityFile`: which of this device's keys ssh offers.
    IdentityFile,
    /// `appUpdates.githubRepo`: where this app's own updates come from.
    Repo,
    /// The deprecated top-level `local`, which only the Mac acts on.
    Local,
}

/// What this device does with one launch value (the `state` of the `launch` vectors).
public enum LaunchUse
{
    /// The value the document offers, accepted without asking.
    Offered,
    /// The value this device approved at the same place, because the offered one waits.
    Approved,
    /// A system with no runnable tuple: no command line is built for it.
    NotDriven,
    /// The repository this build names itself.
    BuiltIn,
    /// No identity file, or no local agent.
    None,
}

/// How a system is driven from this device, for the rows that say so.
public enum LaunchState
{
    /// It runs the command the document offers, which this device approved.
    Approved,
    /// It runs the command this device approved, and a changed one is waiting for approval.
    ApprovedWithChangeWaiting,
    /// No command it may run, so nothing is sent to it from here.
    NotDriven,
}

/// One system's launch value: the argv this device runs over ssh with its own key, and the grammar
/// of section 15.1 its line is written in. An inert tuple has neither, and runs nothing.
public sealed record LaunchTuple(IReadOnlyList<string>? Agent, string? Grammar)
{
    public static LaunchTuple Inert { get; } = new(null, null);

    public bool IsInert => Agent is null;

    /// Whether this tuple can be run as it stands: every argv item shown faithfully, and a grammar
    /// section 15.2 can write.
    public bool IsRunnable =>
        Agent is { Count: > 0 } agent && agent.All(LaunchValues.IsLaunchText) && RemoteShells.FromWire(Grammar) is not null;

    public bool Equals(LaunchTuple? other) =>
        other is not null && Grammar == other.Grammar
        && (Agent is null ? other.Agent is null : other.Agent is not null && Agent.SequenceEqual(other.Agent, StringComparer.Ordinal));

    public override int GetHashCode() => HashCode.Combine(Grammar, Agent?.Count ?? -1);

    public JsonObject ToJson() => new()
    {
        ["agent"] = Agent is null ? null : new JsonArray(Agent.Select(item => (JsonNode)item!).ToArray()),
        ["grammar"] = Grammar,
    };

    /// A tuple as a record or a vector holds one: a non-empty argv of strings and a grammar. Anything
    /// else is no tuple at all, and approves nothing.
    public static LaunchTuple? From(JsonNode? node)
    {
        if (node is not JsonObject tuple) return null;
        if (!tuple.TryGetPropertyValue("agent", out var agent) || agent is not JsonArray items || items.Count == 0) return null;
        var argv = new List<string>(items.Count);
        foreach (var item in items)
        {
            if (LaunchValues.Text(item) is not { } text) return null;
            argv.Add(text);
        }
        return LaunchValues.Text(tuple["grammar"]) is { } grammar ? new LaunchTuple(argv, grammar) : null;
    }
}

public sealed record LaunchSystem(string Id, LaunchTuple Tuple);

/// One machine's launch values. [IdentityFile] is null when the document names none.
public sealed record LaunchMachine(string Id, IReadOnlyList<LaunchSystem> Systems, string? IdentityFile);

/// The deprecated `local` as it is offered: [Agent] null means the default path.
public sealed record LaunchLocal(string? Agent);

/// The launch values of a document (section 9.9, "Reading them"). [Repo] null means the built-in
/// repository and [Local] null means none.
public sealed record LaunchProjection(IReadOnlyList<LaunchMachine> Machines, string? Repo, LaunchLocal? Local)
{
    public static LaunchProjection Empty { get; } = new(Array.Empty<LaunchMachine>(), null, null);

    public static LaunchProjection Of(ControllerDocument? document) => document is null ? Empty : Of(document.Text);

    public static LaunchProjection Of(byte[] bytes) => Of(Encoding.UTF8.GetString(Canonical.Bytes(bytes)));

    /// Read the way [ConfigStore] reads a document, so the two never disagree about which
    /// machines there are.
    public static LaunchProjection Of(string text) => Of(Value.Parse(text));

    /// Always the parsed JSON, never [ControllerConfig]: a typed model fills in defaults and reads
    /// the number 5 as the identity file "5", and either would make this disagree with what the
    /// next device reads from the same bytes. Machines and systems are walked in order; a
    /// non-object, an item without a string id and a repeated id after its first occurrence are
    /// skipped.
    public static LaunchProjection Of(Value document)
    {
        var machines = new List<LaunchMachine>();
        var seenMachines = new HashSet<string>(StringComparer.Ordinal);
        foreach (var machine in Items(Own(document.Raw, "machines")))
        {
            if (LaunchValues.Text(Own(machine, "id")) is not { } machineId || !seenMachines.Add(machineId)) continue;
            var systems = new List<LaunchSystem>();
            var seenSystems = new HashSet<string>(StringComparer.Ordinal);
            foreach (var system in Items(Own(machine, "systems")))
            {
                if (LaunchValues.Text(Own(system, "id")) is not { } systemId || !seenSystems.Add(systemId)) continue;
                systems.Add(new LaunchSystem(systemId, TupleOf(system)));
            }
            var identityFile = LaunchValues.Text(Own(Own(machine, "ssh"), "identityFile"));
            machines.Add(new LaunchMachine(machineId, systems, identityFile is { Length: > 0 } ? identityFile : null));
        }

        var githubRepo = LaunchValues.Text(Own(Own(document.Raw, "appUpdates"), "githubRepo"));
        var repo = githubRepo is { Length: > 0 } && githubRepo != ControllerConfig.DefaultUpdateRepo ? githubRepo : null;
        LaunchLocal? local = null;
        if (Own(document.Raw, "local") is { ValueKind: JsonValueKind.Object } localBlock
            && Own(localBlock, "enabled")?.ValueKind != JsonValueKind.False)
        {
            local = new LaunchLocal(LaunchValues.Text(Own(localBlock, "agent")) is { Length: > 0 } agent ? agent : null);
        }
        return new LaunchProjection(machines, repo, local);
    }

    public LaunchMachine? Machine(string id) => Machines.FirstOrDefault(machine => machine.Id == id);

    public LaunchTuple? TupleAt(string machine, string system) =>
        Machine(machine)?.Systems.FirstOrDefault(candidate => candidate.Id == system)?.Tuple;

    /// One system's tuple, read from the system object exactly as the projection reads it.
    internal static LaunchTuple TupleOf(JsonElement system)
    {
        // Not an array, empty, or holding anything but strings: nothing runs on this system, so
        // the tuple is inert and accepted wherever it appears.
        if (Own(system, "agent") is not { ValueKind: JsonValueKind.Array } agent || agent.GetArrayLength() == 0) return LaunchTuple.Inert;
        var argv = new List<string>();
        foreach (var item in agent.EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.String) return LaunchTuple.Inert;
            argv.Add(item.GetString()!);
        }
        // The grammar, not the raw platform, shell and restricted fields, is what decides how the
        // line is quoted. Recording the grammar means an editor that writes `shell: "cmd"` where it
        // was absent on Windows changes nothing that runs and asks nobody anything. The three
        // fields go in as written: only the string "windows" is Windows, only true is restricted,
        // and a shell that is there but not a string is no shell this app knows.
        var platform = Own(system, "platform") is { ValueKind: JsonValueKind.String } named ? named.GetString() : null;
        var shell = Own(system, "shell") switch
        {
            null or { ValueKind: JsonValueKind.Null } => null,
            { ValueKind: JsonValueKind.String } text => text.GetString(),
            // Never one of the three shell names, so section 15.1 refuses it as unknown-shell.
            { } other => other.GetRawText(),
        };
        var restricted = Own(system, "restricted")?.ValueKind == JsonValueKind.True;
        var grammar = RemoteShells.Resolve(platform, shell, restricted, restrictedSession: false, argv.Count);
        return new LaunchTuple(argv, grammar.Wire);
    }

    /// A property of an object exactly as written, null when either is absent. JSON null is a
    /// value here, unlike in [Value], because `shell: null` and a shell that is not a string are
    /// read differently.
    private static JsonElement? Own(JsonElement? holder, string name) =>
        holder is { ValueKind: JsonValueKind.Object } element && element.TryGetProperty(name, out var found) ? found : null;

    private static IEnumerable<JsonElement> Items(JsonElement? array) =>
        array is { ValueKind: JsonValueKind.Array } items
            ? items.EnumerateArray().Where(item => item.ValueKind == JsonValueKind.Object)
            : Enumerable.Empty<JsonElement>();
}

/// One launch value this device has not accepted, shown to a person beside the one it approved.
///
/// [Recorded] is true when the record holds anything at [Path] (`kind: changed`), and [Approved]
/// is what it holds there. [Offered] is the value in the document: a tuple's `{agent, grammar}`,
/// the identity file or repository as a string, or local's `{agent}`.
public sealed record LaunchReviewEntry(
    string Path,
    LaunchScope Scope,
    string? MachineId,
    string? SystemId,
    bool Recorded,
    bool Approvable,
    JsonNode? Approved,
    JsonNode Offered)
{
    public string Kind => Recorded ? "changed" : "new";

    /// The offered tuple, for a system.
    public LaunchTuple? OfferedTuple => Scope == LaunchScope.System ? LaunchTuple.From(Offered) : null;

    /// Whether Revert has something runnable to put back: an ordinary edit restoring the value
    /// this device approved, which replicates. A record holding something unusable at this place
    /// is nothing to go back to.
    public bool CanRevert => Recorded && Scope switch
    {
        LaunchScope.System => LaunchTuple.From(Approved)?.IsRunnable == true,
        LaunchScope.IdentityFile => LaunchValues.IsLaunchText(LaunchValues.Text(Approved)),
        LaunchScope.Repo => LaunchValues.IsRepo(LaunchValues.Text(Approved)),
        LaunchScope.Local => LaunchValues.RunnableLocal(Approved),
        _ => false,
    };
}

/// What this device uses for one launch value (the `effective` of the `launch` vectors): one entry
/// per offered system, then one identityFile entry per machine, then the repository, then local.
/// [Tuple] is set only for a system that runs; [Text] holds the identity file, the repository or
/// local.agent, null for the built-in repository and for local's default path.
public sealed record EffectiveLaunch(
    string Path, LaunchScope Scope, string? MachineId, string? SystemId, LaunchUse Use, LaunchTuple? Tuple, string? Text);

/// The approval record (v3.md section 11): every launch value this device approved, nested by id
/// because an id may contain a slash or a dot.
///
/// `{ "schema": 1, "seededFrom": <hash or "none">, "machines": { <m>: { "systems": { <s>: {agent,
/// grammar} }, "identityFile": ".." } }, "githubRepo": "..", "local": { "agent": .. } }`. It is
/// read as tolerantly as the reference reads it: a value that is not one of these shapes approves
/// nothing, and a key this build does not know is kept on every write.
public sealed class LaunchRecord
{
    private readonly JsonObject _json;

    public LaunchRecord(JsonObject json) => _json = (JsonObject)json.DeepClone();

    public static LaunchRecord Empty(string seededFrom = "none") => new(new JsonObject { ["schema"] = 1, ["seededFrom"] = seededFrom });

    /// A record approving every launch value of a document: what a device writes when it seeds
    /// itself from the document it already applies, and what agent 3.1.0 compares a restricted push
    /// with. An inert tuple runs nothing and is accepted anyway, so it is not recorded.
    public static LaunchRecord Seeded(LaunchProjection projection, string seededFrom)
    {
        var record = Empty(seededFrom);
        foreach (var change in LaunchValues.Changes(null, projection)) record = record.With(change);
        return record;
    }

    public string? SeededFrom => LaunchValues.Text(_json["seededFrom"]);

    public JsonObject ToJson() => (JsonObject)_json.DeepClone();

    public bool SameAs(LaunchRecord other) => JsonNode.DeepEquals(_json, other._json);

    /// Every approved tuple, wherever it sits: a value approved anywhere is accepted anywhere.
    public IReadOnlyList<LaunchTuple> Tuples =>
        MachineEntries().SelectMany(machine => Entries(machine["systems"] as JsonObject))
            .Select(LaunchTuple.From).OfType<LaunchTuple>().ToList();

    public IReadOnlyList<string> IdentityFiles =>
        MachineEntries().Select(machine => LaunchValues.Text(machine["identityFile"])).OfType<string>().ToList();

    public string? Repo => LaunchValues.Text(_json["githubRepo"]);

    /// The approved local, `{agent}`, or null when none is.
    public JsonObject? Local => _json["local"] as JsonObject;

    /// What the record holds at one place: the fallback for a value that waits there. [Present]
    /// is true for anything at all, JSON null included, which is what makes an entry `changed`.
    public (bool Present, JsonNode? Value) At(LaunchScope scope, string? machine, string? system)
    {
        JsonNode? holder = scope switch
        {
            LaunchScope.System => (MachineEntry(machine)?["systems"]) as JsonObject,
            LaunchScope.IdentityFile => MachineEntry(machine),
            _ => _json,
        };
        var key = scope switch
        {
            LaunchScope.System => system!,
            LaunchScope.IdentityFile => "identityFile",
            LaunchScope.Repo => "githubRepo",
            _ => "local",
        };
        return holder is JsonObject owner && owner.TryGetPropertyValue(key, out var found) ? (true, found) : (false, null);
    }

    /// The same record with [change] approved at its place, everything else exactly as it was.
    public LaunchRecord With(LaunchChange change)
    {
        var json = (JsonObject)_json.DeepClone();
        var value = change.Value.DeepClone();
        switch (change.Scope)
        {
            case LaunchScope.System:
                Child(Child(Child(json, "machines"), change.MachineId!), "systems")[change.SystemId!] = value;
                break;
            case LaunchScope.IdentityFile:
                Child(Child(json, "machines"), change.MachineId!)["identityFile"] = value;
                break;
            case LaunchScope.Repo:
                json["githubRepo"] = value;
                break;
            case LaunchScope.Local:
                json["local"] = value;
                break;
        }
        return new LaunchRecord(json);
    }

    private JsonObject? MachineEntry(string? machine) =>
        machine is not null && _json["machines"] is JsonObject machines && machines.TryGetPropertyValue(machine, out var entry)
            ? entry as JsonObject
            : null;

    private IEnumerable<JsonObject> MachineEntries() => Entries(_json["machines"] as JsonObject).OfType<JsonObject>();

    private static IEnumerable<JsonNode?> Entries(JsonObject? holder) =>
        holder is null ? Enumerable.Empty<JsonNode?>() : holder.Select(property => property.Value);

    /// The object under [key], made one when it is missing or is something else.
    private static JsonObject Child(JsonObject parent, string key)
    {
        if (parent[key] is JsonObject existing) return existing;
        var created = new JsonObject();
        parent[key] = created;
        return created;
    }
}

/// One launch value to approve at one place.
public sealed record LaunchChange(LaunchScope Scope, string? MachineId, string? SystemId, JsonNode Value);

public static class LaunchValues
{
    // Control characters, C1 controls, zero-width and bidirectional formatting characters, word
    // joiners and the byte order mark. Each can make the text a person approves look different
    // from the text that runs; a value holding one can only be reverted or left, never approved.
    private static readonly Regex LaunchTextGrammar = new(
        "\\A[^\\u0000-\\u001F\\u007F-\\u009F\\u200B-\\u200F\\u202A-\\u202E\\u2060-\\u2064\\u2066-\\u2069\\uFEFF]+\\z",
        RegexOptions.CultureInvariant);

    // common.schema.json#/$defs/githubRepo: owner/name as GitHub spells them; . and .. are not names.
    private static readonly Regex RepoGrammar = new(
        @"\A[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?/(?!\.\.?\z)[A-Za-z0-9._-]{1,100}\z",
        RegexOptions.CultureInvariant);

    private const string Entry = "/agent/src/index.mjs";
    private const string Launcher = "/bin/launcher.mjs";

    public static string SystemPath(string machine, string system) => $"machines[{machine}].systems[{system}]";

    public static string IdentityFilePath(string machine) => $"machines[{machine}].ssh.identityFile";

    public const string RepoPath = "appUpdates.githubRepo";

    public const string LocalPath = "local";

    public static string Wire(this LaunchScope scope) => scope switch
    {
        LaunchScope.System => "system",
        LaunchScope.IdentityFile => "identityFile",
        LaunchScope.Repo => "repo",
        _ => "local",
    };

    public static string Wire(this LaunchUse use) => use switch
    {
        LaunchUse.Offered => "offered",
        LaunchUse.Approved => "approved",
        LaunchUse.NotDriven => "not-driven",
        LaunchUse.BuiltIn => "built-in",
        _ => "none",
    };

    /// Whether an argv item, an identityFile or local.agent can be shown to a person and approved.
    public static bool IsLaunchText(string? text)
    {
        if (text is null || text.Contains('\n') || text.Contains('\r')) return false;
        var size = Encoding.UTF8.GetByteCount(text);
        return size is >= 1 and <= 4096 && LaunchTextGrammar.IsMatch(text);
    }

    public static bool IsRepo(string? value) => CommandSurface.Matches(RepoGrammar, value);

    /// Whether a recorded local can run: an object whose agent is null (the default path) or text
    /// that passes [IsLaunchText].
    public static bool RunnableLocal(JsonNode? local) =>
        local is JsonObject block && block.TryGetPropertyValue("agent", out var agent)
        && (agent is null || IsLaunchText(Text(agent)));

    /// Whether [offered] is the launcher migration of [approved]: the same grammar, the same argv
    /// except one item, which moves from `<base>/agent/src/index.mjs` to `<base>/bin/launcher.mjs`
    /// under the same non-empty base with no `..` segment, compared after replacing `\` with `/`
    /// because this app itself writes the new path with forward slashes into a Windows argv. The 3.1
    /// upgrade migrates every system this way, and asking every peer to review a file the agent
    /// installs under an already approved base would add prompts and no protection. The reverse
    /// direction is never a migration.
    public static bool IsLauncherMigration(LaunchTuple? approved, LaunchTuple? offered)
    {
        if (approved?.Agent is not { Count: > 0 } before || approved.Grammar is null) return false;
        if (offered?.Agent is not { Count: > 0 } after || offered.Grammar is null) return false;
        if (approved.Grammar != offered.Grammar || before.Count != after.Count) return false;
        var differing = Enumerable.Range(0, before.Count).Where(index => before[index] != after[index]).ToList();
        if (differing.Count != 1) return false;
        var from = before[differing[0]].Replace('\\', '/');
        var to = after[differing[0]].Replace('\\', '/');
        if (!from.EndsWith(Entry, StringComparison.Ordinal) || !to.EndsWith(Launcher, StringComparison.Ordinal)) return false;
        var root = from[..^Entry.Length];
        return root.Length > 0 && root == to[..^Launcher.Length] && !root.Split('/').Contains("..");
    }

    /// One entry for each offered launch value that is not accepted without asking, in this order:
    /// for each machine its systems and then its identityFile; then the repository; then local.
    /// [LaunchReviewEntry.Approvable] is false for a value that can only be reverted or left: an
    /// argv item, identityFile or local.agent that fails [IsLaunchText], a grammar section 15.1
    /// refuses, a repository that is not owner/name.
    public static IReadOnlyList<LaunchReviewEntry> Review(LaunchRecord record, LaunchProjection projection)
    {
        var approved = new Approved(record);
        var entries = new List<LaunchReviewEntry>();
        void Add(string path, LaunchScope scope, string? machine, string? system, bool approvable, JsonNode offered)
        {
            var (present, value) = record.At(scope, machine, system);
            entries.Add(new LaunchReviewEntry(path, scope, machine, system, present, approvable, value?.DeepClone(), offered));
        }

        foreach (var machine in projection.Machines)
        {
            foreach (var system in machine.Systems)
            {
                if (approved.Accepts(system.Tuple)) continue;
                Add(SystemPath(machine.Id, system.Id), LaunchScope.System, machine.Id, system.Id,
                    system.Tuple.IsRunnable, system.Tuple.ToJson());
            }
            if (!approved.AcceptsIdentityFile(machine.IdentityFile))
            {
                Add(IdentityFilePath(machine.Id), LaunchScope.IdentityFile, machine.Id, null,
                    IsLaunchText(machine.IdentityFile), machine.IdentityFile!);
            }
        }
        if (!approved.AcceptsRepo(projection.Repo))
        {
            Add(RepoPath, LaunchScope.Repo, null, null, IsRepo(projection.Repo), projection.Repo!);
        }
        if (!approved.AcceptsLocal(projection.Local))
        {
            Add(LocalPath, LaunchScope.Local, null, null, ApprovableLocal(projection.Local!),
                new JsonObject { ["agent"] = projection.Local!.Agent });
        }
        return entries;
    }

    /// What this device runs (section 9.9, "Everything else waits").
    ///
    /// An accepted value that can run is used as offered. Anything else falls back to what the
    /// record holds at that same place, never at another one, and only when the recorded value can
    /// run itself; with nothing usable there a system is not driven, an identityFile or local is
    /// none and the repository is the built-in one. An inert tuple is not driven: nothing runs on it.
    public static IReadOnlyList<EffectiveLaunch> Effective(LaunchRecord record, LaunchProjection projection)
    {
        var approved = new Approved(record);
        var entries = new List<EffectiveLaunch>();
        foreach (var machine in projection.Machines)
        {
            foreach (var system in machine.Systems)
            {
                var path = SystemPath(machine.Id, system.Id);
                var tuple = system.Tuple;
                if (!tuple.IsInert && approved.Accepts(tuple) && tuple.IsRunnable)
                {
                    entries.Add(new EffectiveLaunch(path, LaunchScope.System, machine.Id, system.Id, LaunchUse.Offered, tuple, null));
                }
                else if (!tuple.IsInert && LaunchTuple.From(record.At(LaunchScope.System, machine.Id, system.Id).Value) is { IsRunnable: true } recorded)
                {
                    entries.Add(new EffectiveLaunch(path, LaunchScope.System, machine.Id, system.Id, LaunchUse.Approved, recorded, null));
                }
                else
                {
                    entries.Add(new EffectiveLaunch(path, LaunchScope.System, machine.Id, system.Id, LaunchUse.NotDriven, null, null));
                }
            }
        }
        foreach (var machine in projection.Machines)
        {
            var path = IdentityFilePath(machine.Id);
            var offered = machine.IdentityFile;
            var recorded = Text(record.At(LaunchScope.IdentityFile, machine.Id, null).Value);
            entries.Add(offered is not null && approved.AcceptsIdentityFile(offered) && IsLaunchText(offered)
                ? new EffectiveLaunch(path, LaunchScope.IdentityFile, machine.Id, null, LaunchUse.Offered, null, offered)
                : offered is not null && IsLaunchText(recorded)
                    ? new EffectiveLaunch(path, LaunchScope.IdentityFile, machine.Id, null, LaunchUse.Approved, null, recorded)
                    : new EffectiveLaunch(path, LaunchScope.IdentityFile, machine.Id, null, LaunchUse.None, null, null));
        }
        {
            var offered = projection.Repo;
            var recorded = record.Repo;
            entries.Add(offered is not null && approved.AcceptsRepo(offered) && IsRepo(offered)
                ? new EffectiveLaunch(RepoPath, LaunchScope.Repo, null, null, LaunchUse.Offered, null, offered)
                : offered is not null && IsRepo(recorded) && recorded != ControllerConfig.DefaultUpdateRepo
                    ? new EffectiveLaunch(RepoPath, LaunchScope.Repo, null, null, LaunchUse.Approved, null, recorded)
                    : new EffectiveLaunch(RepoPath, LaunchScope.Repo, null, null, LaunchUse.BuiltIn, null, null));
        }
        {
            var offered = projection.Local;
            var recorded = record.Local;
            entries.Add(offered is not null && approved.AcceptsLocal(offered) && ApprovableLocal(offered)
                ? new EffectiveLaunch(LocalPath, LaunchScope.Local, null, null, LaunchUse.Offered, null, offered.Agent)
                : offered is not null && RunnableLocal(recorded)
                    ? new EffectiveLaunch(LocalPath, LaunchScope.Local, null, null, LaunchUse.Approved, null, Text(recorded!["agent"]))
                    : new EffectiveLaunch(LocalPath, LaunchScope.Local, null, null, LaunchUse.None, null, null));
        }
        return entries;
    }

    /// Every launch value of [after] that differs from the value at the same place in [before], at
    /// its new value: what an edit made on this device approves. With no [before], every launch
    /// value of [after] counts. Inert tuples, absent identity files, the built-in repository and an
    /// absent local are accepted anyway and approve nothing.
    public static IReadOnlyList<LaunchChange> Changes(LaunchProjection? before, LaunchProjection after)
    {
        var changes = new List<LaunchChange>();
        foreach (var machine in after.Machines)
        {
            var previous = before?.Machine(machine.Id);
            foreach (var system in machine.Systems)
            {
                if (system.Tuple.IsInert) continue;
                if (previous?.Systems.FirstOrDefault(candidate => candidate.Id == system.Id)?.Tuple == system.Tuple) continue;
                changes.Add(new LaunchChange(LaunchScope.System, machine.Id, system.Id, system.Tuple.ToJson()));
            }
            if (machine.IdentityFile is { } identityFile && (previous is null || previous.IdentityFile != identityFile))
            {
                changes.Add(new LaunchChange(LaunchScope.IdentityFile, machine.Id, null, identityFile));
            }
        }
        if (after.Repo is { } repo && before?.Repo != repo)
        {
            changes.Add(new LaunchChange(LaunchScope.Repo, null, null, repo));
        }
        if (after.Local is { } local && (before is null || before.Local != local))
        {
            changes.Add(new LaunchChange(LaunchScope.Local, null, null, new JsonObject { ["agent"] = local.Agent }));
        }
        return changes;
    }

    /// Approving a review entry: exactly the value it showed, at its place.
    public static LaunchChange Change(this LaunchReviewEntry entry) =>
        new(entry.Scope, entry.MachineId, entry.SystemId, entry.Offered.DeepClone());

    /// A JSON string as itself, and anything else as nothing.
    public static string? Text(JsonNode? node) =>
        node is JsonValue value && value.GetValueKind() == JsonValueKind.String ? value.GetValue<string>() : null;

    internal static string? Text(JsonElement? element) =>
        element is { ValueKind: JsonValueKind.String } text ? text.GetString() : null;

    private static bool ApprovableLocal(LaunchLocal local) => local.Agent is null || IsLaunchText(local.Agent);

    /// What a record approves, gathered once for one evaluation.
    private sealed class Approved(LaunchRecord record)
    {
        private readonly IReadOnlyList<LaunchTuple> _tuples = record.Tuples;
        private readonly IReadOnlyList<string> _identityFiles = record.IdentityFiles;
        private readonly string? _repo = record.Repo;
        private readonly JsonObject? _local = record.Local;

        public bool Accepts(LaunchTuple tuple) =>
            tuple.IsInert || _tuples.Any(candidate => candidate == tuple || IsLauncherMigration(candidate, tuple));

        public bool AcceptsIdentityFile(string? path) => path is null || _identityFiles.Contains(path, StringComparer.Ordinal);

        public bool AcceptsRepo(string? repo) => repo is null || _repo == repo;

        /// Only a recorded agent that is the same JSON value: a string equal by code point, or null
        /// for null. A record whose local has no agent at all approves no path.
        public bool AcceptsLocal(LaunchLocal? local) =>
            local?.Agent is null
            || (_local is not null && _local.TryGetPropertyValue("agent", out var agent) && Text(agent) == local.Agent);
    }
}
