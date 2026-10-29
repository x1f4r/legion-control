using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Config;

/// The configuration this device runs, built from the document and its own approval record.
///
/// Adopting and running are separate. Every adoption applies the document exactly as it is, so the
/// hash and the lineage advance and replication carries on; this is the one place where what the
/// document offers meets what this device approved, and every transport, bootstrap and update path
/// reads its result rather than the document. It runs on every load, every change to the record,
/// every rebuild and after every adoption.
public static class LaunchGate
{
    /// The gated configuration, what waits for a person, and what each launch value came to.
    public sealed record Result(
        ControllerConfig Config,
        IReadOnlyList<LaunchReviewEntry> Review,
        IReadOnlyList<EffectiveLaunch> Launches);

    private static readonly JsonSerializerOptions Writing = new() { WriteIndented = true };

    /// Shown escaped, never as they are, when a value holds a character [LaunchValues.IsLaunchText]
    /// refuses: a person cannot see a bidirectional override, only what it does to the text after it.
    private static readonly JsonSerializerOptions Escaping = new() { Encoder = JavaScriptEncoder.Default };

    private static readonly JsonSerializerOptions Readable = new() { Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping };

    /// Serialisation reads only the agent and the grammar this writes into each system. A system
    /// whose tuple waits runs the one approved at that machine and system; with none it is not
    /// driven, and its agent is empty so that no command line can be built for it. The identity
    /// file, the repository and local fall back the same way.
    public static Result Effective(ControllerConfig config, ControllerDocument? document, LaunchRecord record)
    {
        var projection = LaunchProjection.Of(document);
        var review = LaunchValues.Review(record, projection);
        var launches = LaunchValues.Effective(record, projection);

        var systems = new Dictionary<(string, string), EffectiveLaunch>();
        var identityFiles = new Dictionary<string, EffectiveLaunch>(StringComparer.Ordinal);
        foreach (var launch in launches)
        {
            if (launch.Scope == LaunchScope.System) systems.TryAdd((launch.MachineId!, launch.SystemId!), launch);
            if (launch.Scope == LaunchScope.IdentityFile) identityFiles.TryAdd(launch.MachineId!, launch);
        }
        var waiting = review.Where(entry => entry.Scope == LaunchScope.System)
            .Select(entry => (entry.MachineId!, entry.SystemId!)).ToHashSet();

        SystemConfig Gate(MachineConfig machine, SystemConfig system)
        {
            if (!systems.TryGetValue((machine.Id, system.Id), out var launch) || launch.Tuple is not { Agent: { } agent } tuple)
            {
                return system with { Agent = Array.Empty<string>(), LaunchState = LaunchState.NotDriven };
            }
            var grammar = RemoteShells.FromWire(tuple.Grammar)!.Value;
            return system with
            {
                Agent = agent,
                Grammar = new CommandGrammar(grammar, null),
                // What the approved grammar says, not what the document says: a peer that clears
                // `restricted` changes nothing this device decides about an administrator path.
                Restricted = grammar == RemoteShell.Dispatch,
                LaunchState = launch.Use == LaunchUse.Approved && waiting.Contains((machine.Id, system.Id))
                    ? LaunchState.ApprovedWithChangeWaiting
                    : LaunchState.Approved,
            };
        }

        var machines = config.Machines.Select(machine =>
        {
            var identityFile = identityFiles.TryGetValue(machine.Id, out var launch) ? launch.Text : null;
            return machine with
            {
                Systems = machine.Systems.Select(system => Gate(machine, system)).ToList(),
                Ssh = machine.Ssh is { } ssh ? ssh with { IdentityFile = identityFile } : null,
            };
        }).ToList();

        var repo = launches.First(launch => launch.Scope == LaunchScope.Repo);
        var local = launches.First(launch => launch.Scope == LaunchScope.Local);
        var gated = config with
        {
            Machines = machines,
            UpdateRepo = repo.Text,
            Local = local.Use == LaunchUse.None || config.Local is null
                ? null
                : config.Local with { Agent = local.Text ?? LocalConfig.DefaultAgentPath },
        };
        return new Result(gated, review, launches);
    }

    // MARK: putting an approved value back

    /// The document with the value this device approved at [entry]'s place written back in: Revert,
    /// which is an ordinary edit and replicates like one.
    ///
    /// A tuple records the grammar rather than the fields it came from, so the fields are changed
    /// only as far as it takes to give the approved grammar back: `restricted` for the dispatcher,
    /// and otherwise `shell` named outright.
    public static (string? Text, string? Problem) Reverted(ControllerDocument document, LaunchReviewEntry entry)
    {
        if (!entry.CanRevert) return (null, $"This device approved nothing at {entry.Path} that it could put back.");
        JsonObject root;
        try
        {
            root = JsonNode.Parse(document.Text) as JsonObject ?? throw new JsonException("not an object");
        }
        catch (JsonException error)
        {
            return (null, $"The setup could not be read to change it: {error.Message}");
        }

        switch (entry.Scope)
        {
            case LaunchScope.System:
                var approved = LaunchTuple.From(entry.Approved)!;
                if (Find(Find(root, "machines", entry.MachineId!), "systems", entry.SystemId!) is not { } system)
                    return (null, $"{entry.Path} is no longer in the setup.");
                system["agent"] = new JsonArray(approved.Agent!.Select(item => (JsonNode)item!).ToArray());
                if (GrammarOf(system) != approved.Grammar)
                {
                    if (approved.Grammar == RemoteShell.Dispatch.Wire()) system["restricted"] = true;
                    else
                    {
                        if (system["restricted"] is JsonValue restricted && restricted.GetValueKind() == JsonValueKind.True)
                            system["restricted"] = false;
                        if (GrammarOf(system) != approved.Grammar) system["shell"] = approved.Grammar;
                    }
                }
                if (GrammarOf(system) != approved.Grammar)
                    return (null, $"The command approved at {entry.Path} could not be put back with its grammar.");
                break;
            case LaunchScope.IdentityFile:
                if (Find(root, "machines", entry.MachineId!) is not { } machine)
                    return (null, $"{entry.Path} is no longer in the setup.");
                Child(machine, "ssh")["identityFile"] = LaunchValues.Text(entry.Approved);
                break;
            case LaunchScope.Repo:
                Child(root, "appUpdates")["githubRepo"] = LaunchValues.Text(entry.Approved);
                break;
            case LaunchScope.Local:
                var local = Child(root, "local");
                if (LaunchValues.Text(entry.Approved!["agent"]) is { } agent) local["agent"] = agent;
                else local.Remove("agent");
                break;
        }
        return (root.ToJsonString(Writing), null);
    }

    private static string? GrammarOf(JsonObject system)
    {
        using var parsed = JsonDocument.Parse(system.ToJsonString());
        return LaunchProjection.TupleOf(parsed.RootElement).Grammar;
    }

    /// The first item of [owner]'s [list] with [id], as the projection reads a repeated id.
    internal static JsonObject? Find(JsonObject? owner, string list, string id) =>
        (owner?[list] as JsonArray)?.OfType<JsonObject>().FirstOrDefault(item => LaunchValues.Text(item["id"]) == id);

    private static JsonObject Child(JsonObject parent, string key)
    {
        if (parent[key] is JsonObject existing) return existing;
        var created = new JsonObject();
        parent[key] = created;
        return created;
    }

    // MARK: editors

    /// One reason an editor will not save a document, and the waiting value it is about, if any.
    public sealed record EditProblem(string Sentence, LaunchReviewEntry? Waiting = null);

    /// What an editor refuses about an edit of [current] into [editedText], given the record this
    /// device holds.
    ///
    /// An edit made here approves the launch values it changes, so it must not change one that is
    /// waiting: an edit to the shell of a system whose command waits would otherwise approve a
    /// command nobody here chose, and the form editors make those inputs read-only for exactly that
    /// reason. The text editor cannot, so the save is refused instead and the value is offered for
    /// Approve or Revert. Carrying a waiting value somewhere else, under a renamed machine or on a
    /// new system, with or without a shell of its own, is refused the same way (see [Carried]).
    /// And an editor never saves an empty agent or a value that could not be approved: a 1.3 Mac
    /// refuses a document with an empty agent, and a value that cannot be shown faithfully is not
    /// one to approve by typing it.
    ///
    /// What waits is worked out here, for exactly [current], never taken from a review an earlier
    /// document left behind.
    public static IReadOnlyList<EditProblem> EditProblems(
        ControllerDocument? current, LaunchRecord record, string editedText)
    {
        var before = LaunchProjection.Of(current);
        var after = LaunchProjection.Of(editedText);
        var review = LaunchValues.Review(record, before);
        var problems = new List<EditProblem>();

        foreach (var entry in review.Where(entry => entry.Scope == LaunchScope.System))
        {
            // Putting back what this device approved there is a revert, and that is allowed.
            var edited = after.TupleAt(entry.MachineId!, entry.SystemId!);
            if (edited is not null && edited != before.TupleAt(entry.MachineId!, entry.SystemId!)
                && edited != LaunchTuple.From(entry.Approved))
            {
                problems.Add(new EditProblem(
                    $"The command of {entry.SystemId} on {entry.MachineId} is waiting for approval on this device. Approve or revert it before changing it here.",
                    entry));
            }
        }

        foreach (var change in LaunchValues.Changes(before, after))
        {
            var path = change.Scope switch
            {
                LaunchScope.System => LaunchValues.SystemPath(change.MachineId!, change.SystemId!),
                LaunchScope.IdentityFile => LaunchValues.IdentityFilePath(change.MachineId!),
                LaunchScope.Repo => LaunchValues.RepoPath,
                _ => LaunchValues.LocalPath,
            };
            if (problems.Any(problem => problem.Waiting?.Path == path)) continue;
            if (Carried(change, before, record, review) is { } carried)
            {
                problems.Add(new EditProblem(
                    $"{path} would take the value waiting for approval at {carried.Path}. Approve or revert it there first.", carried));
                continue;
            }
            if (Unapprovable(change) is { } reason) problems.Add(new EditProblem($"{path} {reason}"));
        }

        // A system whose agent this edit emptied or removed: never saved, so no 1.3 client refuses
        // the document, and nobody stops driving a system by accident.
        foreach (var machine in after.Machines)
        {
            foreach (var system in machine.Systems.Where(system => system.Tuple.IsInert))
            {
                var previous = before.TupleAt(machine.Id, system.Id);
                if (previous is { IsInert: true }) continue;
                problems.Add(new EditProblem(
                    $"{LaunchValues.SystemPath(machine.Id, system.Id)} needs an agent command: the program to run and its arguments, each a non-empty string."));
            }
        }
        return problems;
    }

    /// The waiting value that [change], made by an edit of the document [before] projects, would
    /// approve though nobody here chose it, or null. [waiting] is what waited in that document.
    ///
    /// The editors refuse such an edit, and a hand edit of the file, which is already written by
    /// the time it is read, leaves the value waiting. A change carries one when it holds the very
    /// value at another place, under a renamed machine or on a new system, or changes the tuple of
    /// a system whose own tuple waits, which is what an edit of the shell makes of a peer's
    /// command. For a system it also carries one when it holds a waiting argv under any other
    /// grammar, wherever that is: a rename or a copy and a shell edit at once. The argv is what
    /// nobody here chose, and the grammar only says how its line is quoted.
    ///
    /// A tuple the record accepts carries nothing, and neither does a new grammar for the argv a
    /// system already had: that system's tuple did not wait, so its argv is one this device
    /// approved, and quoting it another way is the person's own edit of it.
    internal static LaunchReviewEntry? Carried(
        LaunchChange change, LaunchProjection before, LaunchRecord record, IReadOnlyList<LaunchReviewEntry> waiting)
    {
        var same = waiting.FirstOrDefault(entry => entry.Scope == change.Scope && JsonNode.DeepEquals(entry.Offered, change.Value));
        if (change.Scope != LaunchScope.System) return same;
        var tuple = LaunchTuple.From(change.Value)!;
        if (LaunchValues.Accepts(record, tuple)) return null;
        if (same is not null) return same;
        var systems = waiting.Where(entry => entry.Scope == LaunchScope.System).ToList();
        if (systems.FirstOrDefault(entry => entry.MachineId == change.MachineId && entry.SystemId == change.SystemId) is { } here)
            return here;
        if (before.TupleAt(change.MachineId!, change.SystemId!)?.Agent is { } previous && SameArgv(previous, tuple.Agent!)) return null;
        return systems.FirstOrDefault(entry => SameArgv(entry.OfferedTuple!.Agent!, tuple.Agent!));
    }

    private static bool SameArgv(IReadOnlyList<string> one, IReadOnlyList<string> other) =>
        one.SequenceEqual(other, StringComparer.Ordinal);

    private static string? Unapprovable(LaunchChange change)
    {
        switch (change.Scope)
        {
            case LaunchScope.System:
                var tuple = LaunchTuple.From(change.Value)!;
                if (tuple.Grammar is { } grammar && grammar.StartsWith("refused:", StringComparison.Ordinal))
                    return $"cannot be quoted for this system ({grammar["refused:".Length..]}).";
                return tuple.IsRunnable ? null : TextReason;
            case LaunchScope.IdentityFile:
                return LaunchValues.IsLaunchText(LaunchValues.Text(change.Value)) ? null : TextReason;
            case LaunchScope.Repo:
                return LaunchValues.IsRepo(LaunchValues.Text(change.Value)) ? null : "is not a GitHub owner/name.";
            default:
                return LaunchValues.RunnableLocal(change.Value) ? null : TextReason;
        }
    }

    private const string TextReason =
        "holds a control, invisible or bidirectional formatting character, or an item over 4096 bytes, which no device would run.";

    // MARK: showing values

    /// A launch value in one line, the way the command line prints it: an argv as a JSON array and
    /// its grammar, a path or a repository as a JSON string. Anything a person could not see is
    /// escaped.
    public static string Describe(LaunchScope scope, JsonNode? value) => scope switch
    {
        LaunchScope.System when LaunchTuple.From(value) is { } tuple =>
            $"[{string.Join(", ", tuple.Agent!.Select(Quote))}] as {tuple.Grammar}",
        LaunchScope.Local when value is JsonObject local =>
            LaunchValues.Text(local["agent"]) is { } agent ? Quote(agent) : "the default path",
        _ when LaunchValues.Text(value) is { } text => Quote(text),
        _ => value?.ToJsonString() ?? "nothing",
    };

    /// One argv item, a path or a repository as a JSON string.
    public static string Quote(string text) =>
        JsonSerializer.Serialize(text, LaunchValues.IsLaunchText(text) ? Readable : Escaping);

    /// One argv item as a person reads it on its own line: as it is, or escaped when it holds
    /// something they could not see.
    public static string Show(string text) => LaunchValues.IsLaunchText(text) ? text : Quote(text);

    /// The line every review shows about who wrote the revision. `controller.device` and `source`
    /// are whatever the revision says about itself, so they are marked as unverified, and escaped
    /// like a launch value when they hold something a person could not see: a bidirectional
    /// override in a device name would otherwise rewrite the rest of the line.
    public static string Provenance(ControllerIdentity? identity) =>
        $"Revision {identity?.RevisionNumber ?? 0}, says it was written by "
        + $"{(identity?.Device is { } device ? Show(device) : "an unnamed device")} "
        + $"({(identity?.Source is { } source ? Show(source) : "no source")}). Not verified.";

    /// Every launch value of a document, for the summary a person confirms before a device that
    /// applies no setup yet adopts one and approves all of them: each machine and system with its
    /// argv and grammar, and any identity file, update repository and local agent. Names and ids
    /// are the peer's too, and are escaped like the values beside them.
    public static IReadOnlyList<string> Summary(ControllerDocument document)
    {
        var projection = LaunchProjection.Of(document);
        var decoded = document.Decoded;
        var lines = new List<string>();
        foreach (var machine in projection.Machines)
        {
            var name = decoded.Machine(machine.Id)?.Name ?? machine.Id;
            lines.Add($"{Show(name)} ({Show(machine.Id)})");
            foreach (var system in machine.Systems)
            {
                var systemName = Show(decoded.Machine(machine.Id)?.System(system.Id)?.Name ?? system.Id);
                if (system.Tuple.IsInert)
                {
                    lines.Add($"  {systemName}: no agent command, so it is not driven");
                    continue;
                }
                lines.Add($"  {systemName}, written as {system.Tuple.Grammar}:");
                lines.AddRange(system.Tuple.Agent!.Select(item => $"    {Show(item)}"));
            }
            if (machine.IdentityFile is { } identityFile) lines.Add($"  identity file {Show(identityFile)}");
        }
        if (projection.Repo is { } repo) lines.Add($"Updates for this app from {Show(repo)}");
        if (projection.Local is { } local) lines.Add($"Local agent {(local.Agent is { } agent ? Show(agent) : "at the default path")}");
        return lines;
    }
}
