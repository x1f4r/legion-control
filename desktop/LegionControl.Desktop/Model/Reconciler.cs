using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Model;

/// What reconciling one machine did, or refused to do.
public abstract record ReconcileOutcome
{
    /// The machine holds exactly what this device holds.
    public sealed record InSync : ReconcileOutcome;
    /// The machine was behind and has been handed the document.
    public sealed record Published(string Action) : ReconcileOutcome;
    /// This device was behind and has adopted the machine's document.
    public sealed record Adopted(string Hash) : ReconcileOutcome;
    /// Neither side descends from the other, or they are different setups. Nothing moved.
    public sealed record NeedsDecision(SetupSharing Sharing, ControllerMark? Theirs) : ReconcileOutcome;
    /// Nothing was attempted: too soon after a failure, or an operation is running there.
    public sealed record Held(string Reason) : ReconcileOutcome;
    /// The machine cannot carry a setup with an identity, so it takes no part in this.
    public sealed record Unsupported : ReconcileOutcome;
    public sealed record Failed(string Sentence) : ReconcileOutcome;

    public string Sentence_ => this switch
    {
        InSync => "in sync",
        Published published => $"published ({published.Action})",
        Adopted adopted => $"adopted {adopted.Hash[..12]}",
        NeedsDecision decision => decision.Sharing.Sentence_ ?? "needs a decision",
        Held held => held.Reason,
        Unsupported => "this machine cannot carry the setup",
        Failed failed => failed.Sentence,
        _ => "",
    };
}

/// Keeps every machine's copy of the setup in step with this device's, without ever losing an edit.
///
/// The whole rule is descent. This device publishes only when the machine holds a document its own
/// document descends from, and adopts only when its own document is one the machine's descends
/// from. Both directions are strictly downhill, so two devices can never take turns overwriting
/// each other; everything else stops and asks a person, because a revision number cannot tell "you
/// are behind" from "we both edited last night".
public sealed class Reconciler(ConfigStore config, Bindings bindings)
{
    private readonly ConfigStore _config = config;
    private readonly Dictionary<string, DateTimeOffset> _failedAt = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> _pushedHash = new(StringComparer.Ordinal);
    private readonly SemaphoreSlim _reconcileGate = new(1, 1);

    public Bindings Bindings { get; set; } = bindings;

    /// How long a machine that refused or could not be reached is left alone.
    public static readonly TimeSpan Backoff = TimeSpan.FromSeconds(60);

    /// What each machine's copy is doing, for the setup section.
    public IReadOnlyDictionary<string, ReconcileOutcome> Outcomes => _outcomes;
    private readonly Dictionary<string, ReconcileOutcome> _outcomes = new(StringComparer.Ordinal);

    /// Everything waiting for a person, keyed by machine id.
    public IReadOnlyDictionary<string, ReconcileOutcome.NeedsDecision> Decisions =>
        _outcomes
            .Where(entry => entry.Value is ReconcileOutcome.NeedsDecision)
            .ToDictionary(entry => entry.Key, entry => (ReconcileOutcome.NeedsDecision)entry.Value, StringComparer.Ordinal);

    public async Task<ReconcileOutcome> ReconcileAsync(
        MachineModel machine, CancellationToken cancellationToken = default, DateTimeOffset? now = null)
    {
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            var outcome = await DecideAsync(machine, cancellationToken, now ?? DateTimeOffset.UtcNow);
            _outcomes[machine.Id] = outcome;
            return outcome;
        }
        finally { _reconcileGate.Release(); }
    }

    private async Task<ReconcileOutcome> DecideAsync(
        MachineModel machine, CancellationToken cancellationToken, DateTimeOffset now)
    {
        if (_config.Document is not { } mine) return new ReconcileOutcome.Held("this device has no setup to share");
        if (machine.Status is not { } status) return new ReconcileOutcome.Held("not read yet");

        // A 2.x agent has nowhere to put an identity, so it is left out of this entirely rather
        // than counted as a conflict it can do nothing about.
        if (!machine.Capabilities.SupportsSetupLineage) return new ReconcileOutcome.Unsupported();

        // Never while something is changing the machine: a `config set` that lands during an update
        // is exactly the write the machine's operation lock exists to keep out.
        if (status.Operations.Running.Count > 0)
        {
            return new ReconcileOutcome.Held("an operation is running there");
        }

        if (_failedAt.TryGetValue(machine.Id, out var failed) && now - failed < Backoff)
        {
            return new ReconcileOutcome.Held("waiting before trying again");
        }

        var theirs = status.Controller?.Hash;
        switch (SetupLineage.DecideFromStatus(mine.Hash, mine.Identity, theirs))
        {
            case Descent.Same:
                machine.NoteSharing(new SetupSharing.UpToDate());
                _pushedHash.Remove(machine.Id);
                return new ReconcileOutcome.InSync();

            case Descent.TheyHaveNothing:
            case Descent.TheyAreBehind:
                // At most one automatic push per machine per document until a reply is read.
                if (_pushedHash.TryGetValue(machine.Id, out var already) && already == mine.Hash)
                {
                    return new ReconcileOutcome.Held("already published, waiting for the next reading");
                }
                _pushedHash[machine.Id] = mine.Hash;
                var sharing = await machine.PushSetupAsync(mine, replace: false, cancellationToken);
                return ReadPush(machine, sharing);
        }

        // The hashes differ and this document does not descend from theirs. One extra round trip
        // settles whether they descend from this one, and it is only ever spent here.
        var (meta, problem) = machine.SetupMeta is { } cached && cached.Hash == theirs
            ? (cached, (string?)null)
            : await machine.ReadSetupMetaAsync(cancellationToken);
        if (meta is null)
        {
            _failedAt[machine.Id] = now;
            return new ReconcileOutcome.Failed(problem ?? "the machine did not say what it holds");
        }

        var descent = SetupLineage.Decide(mine.Hash, mine.Identity, meta.Hash,
            new ControllerIdentity { Id = meta.Id, Revision = meta.Revision, Lineage = meta.Lineage },
            meta.Lineage);

        switch (descent)
        {
            case Descent.Same:
                machine.NoteSharing(new SetupSharing.UpToDate());
                return new ReconcileOutcome.InSync();

            case Descent.TheyAreBehind:
                _pushedHash[machine.Id] = mine.Hash;
                return ReadPush(machine, await machine.PushSetupAsync(mine, replace: false, cancellationToken));

            case Descent.IAmBehind:
                // Downhill the other way: the machine holds a descendant of this document, so
                // adopting it loses nothing that is not already in it.
                var (document, fetchProblem) = await machine.FetchSetupAsync(cancellationToken);
                if (document is null)
                {
                    _failedAt[machine.Id] = now;
                    return new ReconcileOutcome.Failed(fetchProblem ?? "the machine did not send its document");
                }
                if (document.Hash != meta.Hash)
                {
                    _failedAt[machine.Id] = now;
                    return new ReconcileOutcome.Failed(
                        $"the document {machine.Name} sent hashes to {document.Hash[..12]} and it says it holds {meta.Hash?[..12]}");
                }
                if (_config.ApplyIfCurrent(document, mine.Hash, $"adopted from {machine.Id}") is { } applyProblem)
                {
                    return new ReconcileOutcome.Failed(applyProblem);
                }
                machine.NoteSharing(new SetupSharing.UpToDate());
                _pushedHash.Clear();
                return new ReconcileOutcome.Adopted(document.Hash);

            case Descent.DifferentSetup:
                var different = new SetupSharing.DifferentSetup(
                    $"{machine.Name} holds a different setup, written by {meta.Provenance}. Neither is an older copy of the other.");
                machine.NoteSharing(different);
                return new ReconcileOutcome.NeedsDecision(different, meta);

            default:
                var diverged = new SetupSharing.Diverged(
                    $"{machine.Name} holds revision {meta.RevisionNumber} of this setup, written by {meta.Provenance}, and this device holds revision {mine.Identity.RevisionNumber}. Neither descends from the other.");
                machine.NoteSharing(diverged);
                return new ReconcileOutcome.NeedsDecision(diverged, meta);
        }
    }

    private ReconcileOutcome ReadPush(MachineModel machine, SetupSharing sharing) => sharing switch
    {
        SetupSharing.JustShared => new ReconcileOutcome.Published("stored"),
        SetupSharing.UpToDate => new ReconcileOutcome.Published("noop"),
        SetupSharing.Ahead => new ReconcileOutcome.Held("the machine is ahead; the next reading fetches it"),
        SetupSharing.Diverged or SetupSharing.DifferentSetup =>
            new ReconcileOutcome.NeedsDecision(sharing, machine.SetupMeta),
        SetupSharing.Failed failed => Fail(machine.Id, failed.Sentence),
        _ => new ReconcileOutcome.Held("nothing to say yet"),
    };

    private ReconcileOutcome Fail(string machineId, string sentence)
    {
        _pushedHash.Remove(machineId);
        _failedAt[machineId] = DateTimeOffset.UtcNow;
        return new ReconcileOutcome.Failed(sentence);
    }

    // MARK: the decisions a person makes

    /// Adopts the machine's document, dropping this device's.
    ///
    /// The document being replaced is kept in the ledger first: "take theirs" is the one decision
    /// here that throws away something somebody typed.
    public async Task<string?> TakeTheirsAsync(MachineModel machine, CancellationToken cancellationToken = default)
    {
        var expectedHash = _config.Document?.Hash;
        var (document, problem) = await machine.FetchSetupAsync(cancellationToken);
        if (document is null) return problem ?? "the machine did not send its document";
        var applyProblem = expectedHash is null
            ? _config.Apply(document, $"took the setup from {machine.Id}")
            : _config.ApplyIfCurrent(document, expectedHash, $"took the setup from {machine.Id}");
        if (applyProblem is not null) return applyProblem;
        _outcomes[machine.Id] = new ReconcileOutcome.Adopted(document.Hash);
        _pushedHash.Clear();
        machine.NoteSharing(new SetupSharing.UpToDate());
        return null;
    }

    /// Keeps this device's setup, in a revision the machine will accept.
    ///
    /// Not a replacement: the machine's current hash goes into the lineage, so what lands there is
    /// a descendant of what it holds and the push is an ordinary fast-forward. Nothing is forced,
    /// and the other side's edits are still in the ledger if somebody wants them back.
    public async Task<string?> KeepMineAsync(MachineModel machine, CancellationToken cancellationToken = default)
    {
        if (_config.Document is not { } mine) return "this device has no setup to keep";
        var theirHash = machine.SetupMeta?.Hash ?? machine.Status?.Controller?.Hash;
        if (theirHash is null) return "the machine has not said what it holds";

        var identity = ControllerIdentity.Merged(mine.Identity, mine.Hash,
            machine.SetupMeta is { } meta
                ? new ControllerIdentity { Id = meta.Id, Revision = meta.Revision, Lineage = meta.Lineage }
                : new ControllerIdentity(),
            theirHash,
            Bindings.Device);

        var stamped = Restamp(mine, identity);
        var problem = _config.ApplyIfCurrent(stamped, mine.Hash, $"kept this device's setup over {machine.Id}");
        if (problem is not null) return problem;
        _pushedHash.Remove(machine.Id);
        return null;
    }

    /// Merges the two branches, entry by entry.
    ///
    /// [choices] answers only the entries both sides moved; everything else follows whichever side
    /// changed it. The result descends from both heads, so every machine holding either of them
    /// fast-forwards to it.
    public async Task<(IReadOnlyList<SetupDifference> Differences, string? Problem)> PrepareMergeAsync(
        MachineModel machine, CancellationToken cancellationToken = default)
    {
        if (_config.Document is not { } mine) return (Array.Empty<SetupDifference>(), "this device has no setup");
        var (theirs, problem) = await machine.FetchSetupAsync(cancellationToken);
        if (theirs is null) return (Array.Empty<SetupDifference>(), problem);
        var baseHash = _config.Ledger.CommonBase(mine.Identity.Lineage, theirs.Identity.Lineage);
        var baseDocument = baseHash is null ? null : _config.Ledger.Read(baseHash);
        return (SetupMerge.Compare(mine, theirs, baseDocument), null);
    }

    public async Task<string?> MergeAsync(
        MachineModel machine, IReadOnlyDictionary<string, SetupChoice>? choices,
        CancellationToken cancellationToken = default)
    {
        if (_config.Document is not { } mine) return "this device has no setup";
        var (theirs, problem) = await machine.FetchSetupAsync(cancellationToken);
        if (theirs is null) return problem;

        var baseHash = _config.Ledger.CommonBase(mine.Identity.Lineage, theirs.Identity.Lineage);
        var baseDocument = baseHash is null ? null : _config.Ledger.Read(baseHash);
        var merged = SetupMerge.Merge(mine, mine.Hash, theirs, theirs.Hash, baseDocument, choices, Bindings.Device);

        var applyProblem = _config.ApplyIfCurrent(merged, mine.Hash, $"merged with {machine.Id}");
        if (applyProblem is not null) return applyProblem;
        _pushedHash.Clear();
        return null;
    }

    /// Sends the document with `--replace`, which is the one command that overwrites a different
    /// setup. Only ever called from a control a person pressed after reading the preview.
    public async Task<string?> ReplaceTheirsAsync(MachineModel machine, CancellationToken cancellationToken = default)
    {
        if (_config.Document is not { } mine) return "this device has no setup to send";
        var sharing = await machine.PushSetupAsync(mine, replace: true, cancellationToken);
        _outcomes[machine.Id] = ReadPush(machine, sharing);
        return sharing is SetupSharing.Failed failed ? failed.Sentence : null;
    }

    /// The change a document would make here, for the sheet that asks about it.
    public async Task<(SetupConflictPreview? Preview, string? Problem)> PreviewTheirsAsync(
        MachineModel machine, CancellationToken cancellationToken = default)
    {
        var (theirs, problem) = await machine.FetchSetupAsync(cancellationToken);
        if (theirs is null) return (null, problem);
        return (SetupConflictPreview.Between(_config.Config, theirs.Decoded), null);
    }

    private static ControllerDocument Restamp(ControllerDocument document, ControllerIdentity identity)
    {
        var root = System.Text.Json.Nodes.JsonNode.Parse(document.Text) as System.Text.Json.Nodes.JsonObject
                   ?? new System.Text.Json.Nodes.JsonObject();
        root["controller"] = ConfigStore.IdentityJson(identity, root["controller"] as System.Text.Json.Nodes.JsonObject);
        return ControllerDocument.FromText(
            root.ToJsonString(new System.Text.Json.JsonSerializerOptions { WriteIndented = true }));
    }
}
