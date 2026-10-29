using System.Collections.Immutable;
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
    /// The machine refused this revision in a way no retry fixes, so it is not sent again until the
    /// setup here changes or a person asks ([Reconciler.TryAgainAsync]).
    public sealed record Withheld(SetupSharing Sharing) : ReconcileOutcome;
    /// The machine cannot carry a setup with an identity, so it takes no part in this.
    public sealed record Unsupported : ReconcileOutcome;
    public sealed record Failed(string Sentence) : ReconcileOutcome;

    public string Sentence_ => this switch
    {
        InSync => "in sync",
        Published published => $"published ({published.Action})",
        Adopted adopted => $"adopted {Short(adopted.Hash)}",
        NeedsDecision decision => decision.Sharing.Sentence_ ?? "needs a decision",
        Held held => held.Reason,
        Withheld withheld => withheld.Sharing.Sentence_ ?? "held",
        Unsupported => "this machine cannot carry the setup",
        Failed failed => failed.Sentence,
        _ => "",
    };

    /// The first twelve characters of a hash, or all of it when it is shorter.
    public static string Short(string hash) => hash.Length > 12 ? hash[..12] : hash;
}

/// What a divergence screen was built from, and everything a decision on it acts on.
///
/// Merge, Keep mine and Take theirs act on exactly these documents and nothing fetched later: what a
/// person answered is what is written, and only while the setup here is still [Mine] (contract
/// section 9.5, adopting is a compare-and-swap).
///
/// [Theirs] is what the machine sent and [TheirsHash] what it says it holds. When [TakeProblem]
/// says those bytes are not the ones the hash covers, or that this device does not accept them,
/// only a different setup has a session at all, and the one answer to it is replacing it: that
/// needs nothing of its bytes but the preview of what it overwrites.
public sealed record MergeSession(
    ControllerDocument? Mine,
    ControllerDocument Theirs,
    string TheirsHash,
    ControllerDocument? Base,
    IReadOnlyList<SetupDifference> Differences,
    SetupConflictPreview Preview,
    bool DifferentSetup,
    string? MergeProblem,
    string? TakeProblem = null)
{
    /// Whether Merge and Keep mine can be offered.
    public bool CanMerge => !DifferentSetup && Mine is not null && MergeProblem is null && TakeProblem is null;

    /// Whether Take theirs can be offered: only for verified bytes this device accepts.
    public bool CanTakeTheirs => TakeProblem is null;

    /// Whether this device's setup can be sent over the machine's with `--replace`: only over a
    /// different setup, never over a copy of this one.
    public bool CanReplace => DifferentSetup && Mine is not null;
}

/// Keeps every machine's copy of the setup in step with this device's, without ever losing an edit.
///
/// The whole rule is descent (contract section 9.5, [SetupSync]). This device publishes only when
/// the machine holds a document its own descends from, and adopts only when its own is one the
/// machine's descends from. Both directions are strictly downhill, so two devices can never take
/// turns overwriting each other; everything else stops and asks a person, because a revision number
/// cannot tell "you are behind" from "we both edited last night".
///
/// Every setup request to a machine, automatic or asked for, runs under one gate, and so does every
/// change to what is remembered about each machine. What the setup section reads is a snapshot that
/// is replaced whole, never changed in place, so the window can draw it while a poll writes.
public sealed class Reconciler(ConfigStore config, Bindings bindings)
{
    private readonly ConfigStore _config = config;
    private readonly Dictionary<string, PeerState> _peers = new(StringComparer.Ordinal);
    /// Why each remembered admission refusal was refused, by its settled key, so the setup section
    /// keeps saying why without reading the document again.
    private readonly Dictionary<string, string> _settledBecause = new(StringComparer.Ordinal);
    private readonly SemaphoreSlim _reconcileGate = new(1, 1);
    private ImmutableDictionary<string, ReconcileOutcome> _outcomes = ImmutableDictionary.Create<string, ReconcileOutcome>(StringComparer.Ordinal);

    public Bindings Bindings { get; set; } = bindings;

    /// How long a machine that refused or could not be reached is left alone.
    public static readonly TimeSpan Backoff = TimeSpan.FromMilliseconds(SetupSync.BackoffMs);

    /// How many times one status may lead to deciding again: after a meta read, a push refusal
    /// that says what the machine holds, or a head that moved during a read.
    private const int Rounds = 4;

    /// What each machine's copy is doing, for the setup section. A snapshot: it never changes after
    /// it is read.
    public IReadOnlyDictionary<string, ReconcileOutcome> Outcomes => Volatile.Read(ref _outcomes);

    /// Everything waiting for a person, keyed by machine id.
    public IReadOnlyDictionary<string, ReconcileOutcome.NeedsDecision> Decisions =>
        Outcomes
            .Where(entry => entry.Value is ReconcileOutcome.NeedsDecision)
            .ToDictionary(entry => entry.Key, entry => (ReconcileOutcome.NeedsDecision)entry.Value, StringComparer.Ordinal);

    public async Task<ReconcileOutcome> ReconcileAsync(
        MachineModel machine, CancellationToken cancellationToken = default, DateTimeOffset? now = null)
    {
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            var outcome = await DecideAsync(machine, cancellationToken, now ?? DateTimeOffset.UtcNow);
            Publish(machine.Id, outcome);
            return outcome;
        }
        finally { _reconcileGate.Release(); }
    }

    /// "Try again" for a push the machine refused with `restricted` or `config-invalid`: the hold
    /// for the setup here is lifted, and the machine is reconciled at once.
    public async Task<ReconcileOutcome> TryAgainAsync(MachineModel machine, CancellationToken cancellationToken = default)
    {
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            if (_config.Document is { } document) StateOf(machine.Id).Release(document.Hash);
        }
        finally { _reconcileGate.Release(); }
        return await ReconcileAsync(machine, cancellationToken);
    }

    private PeerState StateOf(string machineId)
    {
        if (!_peers.TryGetValue(machineId, out var state)) _peers[machineId] = state = new PeerState();
        return state;
    }

    private void Publish(string machineId, ReconcileOutcome outcome) =>
        Volatile.Write(ref _outcomes, _outcomes.SetItem(machineId, outcome));

    /// One status, decided, gated and acted on (contract section 9.5): decide, gate, act, read the
    /// reply, and decide again where the section says to.
    private async Task<ReconcileOutcome> DecideAsync(MachineModel machine, CancellationToken cancellationToken, DateTimeOffset now)
    {
        if (machine.Status is not { } status) return new ReconcileOutcome.Held("not read yet");
        var state = StateOf(machine.Id);
        var at = now.ToUnixTimeMilliseconds();
        var statusAt = machine.LastReadAt?.ToUnixTimeMilliseconds();
        var mark = status.Controller;

        for (var round = 0; round < Rounds; round += 1)
        {
            if (_config.IsPendingEdit) return new ReconcileOutcome.Held("the setup file here holds an edit that is not applied yet");
            var document = _config.Document;
            var local = document is null ? null : LocalHead.Of(document);
            var decision = SetupSync.Decide(local, status.ContractVersion, mark, state.MetaByHash);
            switch (decision.Kind)
            {
                case SetupDecision.Unsupported:
                    machine.NoteSharing(new SetupSharing.Unsupported());
                    return new ReconcileOutcome.Unsupported();
                case SetupDecision.Unknown:
                    return new ReconcileOutcome.Held(decision.Reason == "no-local"
                        ? "this device has no setup to share"
                        : "this device's setup has no setup id yet");
                case SetupDecision.InSync:
                    machine.NoteSharing(new SetupSharing.UpToDate());
                    return new ReconcileOutcome.InSync();
                case SetupDecision.DifferentSetup:
                {
                    var theirs = state.MetaByHash.GetValueOrDefault(mark!.Hash!) ?? mark;
                    var different = new SetupSharing.DifferentSetup(
                        $"{machine.Name} holds a different setup, written by {theirs.Provenance}. Neither is an older copy of the other.");
                    machine.NoteSharing(different);
                    return new ReconcileOutcome.NeedsDecision(different, theirs);
                }
                case SetupDecision.Diverged:
                {
                    var theirs = state.MetaByHash[mark!.Hash!];
                    var diverged = new SetupSharing.Diverged(
                        $"{machine.Name} holds revision {theirs.RevisionNumber} of this setup, written by {theirs.Provenance}, and this device holds revision {document!.Identity.RevisionNumber}. Neither descends from the other.");
                    machine.NoteSharing(diverged);
                    return new ReconcileOutcome.NeedsDecision(diverged, theirs);
                }
            }

            var gate = SetupSync.Gate(decision.Kind, local!.Hash, mark!.Hash, status.AgentVersion, at, state.View(
                localInvalid: _config.Acceptance is { Valid: false },
                localMutation: machine.Operations.Any(operation => operation.Outcome == OperationOutcome.Pending),
                runningOperations: status.Operations.Running.Count,
                statusAt: statusAt));
            if (gate.Action == GateAction.Hold) return Held(machine, state, local.Hash, mark.Hash, status.AgentVersion, gate.Reason);

            switch (decision.Kind)
            {
                case SetupDecision.Push:
                {
                    var (reply, problem) = await InFlight(state, () => machine.PushSetupAsync(document!, replace: false, cancellationToken));
                    var reading = SetupSync.AfterPush(reply?.Json ?? Value.Missing, local.Hash);
                    state.Record(reading, reply, local.Hash, at);
                    switch (reading.Effect)
                    {
                        case PushEffect.Clear:
                            machine.NoteSharing(reply!.Action == "noop" ? new SetupSharing.UpToDate() : new SetupSharing.JustShared());
                            return new ReconcileOutcome.Published(reply.Action ?? "stored");
                        case PushEffect.FileMeta:
                            // The machine said what it holds instead: decide again at once from that.
                            mark = reply!.Current;
                            continue;
                        case PushEffect.Locked:
                            return new ReconcileOutcome.Held("an operation is running there; the setup goes once it finishes");
                        case PushEffect.Hold:
                            return Withheld(machine, state, local.Hash);
                        default:
                            var failed = problem ?? reply?.Envelope.Sentence() ?? $"{machine.Name} did not store the setup it was sent.";
                            machine.NoteSharing(new SetupSharing.Failed(failed));
                            return new ReconcileOutcome.Failed(failed);
                    }
                }
                case SetupDecision.ReadMeta:
                {
                    var (meta, problem) = await InFlight(state, () => machine.ReadSetupMetaAsync(cancellationToken));
                    if (problem is not null)
                    {
                        state.FailedAt[local.Hash] = at;
                        return new ReconcileOutcome.Failed(problem);
                    }
                    // An answer about another hash means the machine changed in between: it is
                    // filed, and the next status decides.
                    if (meta is not null) state.File(meta);
                    if (meta?.Hash != mark.Hash)
                        return new ReconcileOutcome.Held($"{machine.Name} changed while it was read; the next reading decides");
                    continue;
                }
                case SetupDecision.Fetch:
                {
                    var (outcome, again) = await FetchAsync(machine, state, local, decision.Expect!, status.AgentVersion, at, cancellationToken);
                    if (again) continue;
                    return outcome!;
                }
            }
        }
        return new ReconcileOutcome.Held("the setup kept moving while it was compared; the next reading decides");
    }

    /// Reads the machine's document and adopts it when section 9.5 admits it. [Again] is true when
    /// the setup here moved during the read, which decides again from the new head.
    private async Task<(ReconcileOutcome? Outcome, bool Again)> FetchAsync(
        MachineModel machine, PeerState state, LocalHead local, string expect, string? agentVersion, long at,
        CancellationToken cancellationToken)
    {
        var (reply, problem) = await InFlight(state, () => machine.ReadSetupAsync(cancellationToken));
        // A reply without a hash describes no document and is never adopted: it counts as a failed
        // request, and so does one with no text in it.
        if (reply?.Hash is not { } replyHash || reply.Document is not { } text)
        {
            state.FailedAt[local.Hash] = at;
            return (new ReconcileOutcome.Failed(problem ?? reply?.Envelope.Sentence() ?? $"{machine.Name} did not send the setup it holds."), false);
        }

        var fetched = ControllerDocument.FromText(text);
        var accepted = DocumentAcceptance.Check(fetched.Bytes, Intake.Peer);
        var admission = SetupSync.Admit(local.Hash, local.Id!, expect, _config.CurrentHead(),
            fetched.Hash, replyHash, fetched.Identity.Id, fetched.Identity.Lineage, accepted.Valid);
        switch (admission)
        {
            case Admission.HeadMoved:
                return (null, true);
            case Admission.Moved:
                return (new ReconcileOutcome.Held($"{machine.Name} changed since it was read; the next reading decides"), false);
            case Admission.Adopt:
                // The bytes go in exactly as fetched, and only over the head the decision was made
                // from: an edit saved since stays, and the next status decides from it.
                var applied = _config.ApplyIfCurrent(fetched, local.Hash, $"adopted from {machine.Id}", expectedHash: expect);
                if (applied is ApplyResult.HeadMoved) return (null, true);
                if (applied is ApplyResult.Refused refused)
                {
                    state.FailedAt[local.Hash] = at;
                    return (new ReconcileOutcome.Failed(refused.Sentence), false);
                }
                machine.NoteSharing(new SetupSharing.UpToDate());
                return (new ReconcileOutcome.Adopted(fetched.Hash), false);
        }

        // Invalid, unverifiable or inconsistent: remembered, so the same document is not read again
        // until the setup here, the machine's or its agent changes.
        var sentence = admission switch
        {
            Admission.Unverifiable => machine.Verified(reply).Problem ?? $"{machine.Name}'s setup could not be verified.",
            Admission.Invalid => $"{machine.Name} holds a setup this device does not accept: {accepted.Sentence}",
            _ => $"The setup {machine.Name} sent is not the revision it said it holds: its own identity does not descend from this device's.",
        };
        var key = SetupSync.SettledKey(local.Hash, expect, agentVersion);
        state.Settled.Add(key);
        _settledBecause[key] = sentence;
        return (new ReconcileOutcome.Failed(sentence), false);
    }

    /// One setup request to a machine, marked as in flight while it runs, however it ends.
    private static async Task<T> InFlight<T>(PeerState state, Func<Task<T>> request)
    {
        state.InFlight = true;
        try
        {
            return await request();
        }
        finally
        {
            state.InFlight = false;
        }
    }

    private ReconcileOutcome Held(MachineModel machine, PeerState state, string localHash, string? remoteHash, string? agentVersion, string? reason) =>
        reason switch
        {
            "in-flight" => new ReconcileOutcome.Held("a setup request to this machine is still running"),
            "backoff" => new ReconcileOutcome.Held("waiting before trying again"),
            "local-invalid" => new ReconcileOutcome.Held("the setup here has errors, so it is not published until they are fixed"),
            "held" => Withheld(machine, state, localHash),
            "operation-running" => new ReconcileOutcome.Held("an operation is running there"),
            "settled" => new ReconcileOutcome.Failed(
                _settledBecause.GetValueOrDefault(SetupSync.SettledKey(localHash, remoteHash, agentVersion))
                ?? $"{machine.Name}'s setup was refused here before, and nothing has changed since."),
            _ => new ReconcileOutcome.Held(reason ?? "held"),
        };

    /// A held push, said the way section 9.5 asks: that the machine takes this revision only from a
    /// device whose key is not restricted, or the first problem it found in it.
    private static ReconcileOutcome Withheld(MachineModel machine, PeerState state, string localHash)
    {
        var because = state.HeldBecause.GetValueOrDefault(localHash);
        SetupSharing sharing = state.Held.GetValueOrDefault(localHash) == ReasonCode.Restricted
            ? new SetupSharing.AwaitingFullAccess(
                $"{machine.Name} takes this revision only from a device whose key is not restricted there. A device with an administrator key publishes it on its next reading.")
            : new SetupSharing.Refused($"{machine.Name} refused this revision: {because ?? "it is not a document it will store"}. It is not sent again until the setup here changes.");
        machine.NoteSharing(sharing);
        return new ReconcileOutcome.Withheld(sharing);
    }

    // MARK: the decisions a person makes

    /// Builds the divergence screen from the machine's document: the differences of section 9.8
    /// against the base this device still has, the change adopting it would make, and which of the
    /// answers can be offered at all.
    ///
    /// A different setup gets a screen even when its bytes cannot be verified or are refused here,
    /// an agent before 3.1.0 holding a file no client rebuilds for one: replacing it never uses its
    /// bytes, and refusing the screen left nothing that could replace it. Everything else needs the
    /// verified document this device accepts.
    public async Task<(MergeSession? Session, string? Problem)> PrepareMergeAsync(
        MachineModel machine, CancellationToken cancellationToken = default)
    {
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            if (_config.IsPendingEdit) return (null, "The setup file here holds an edit that is not applied yet. Save or fix it first.");
            var mine = _config.Document;
            var (reply, readProblem) = await machine.ReadSetupAsync(cancellationToken);
            if (reply is null) return (null, readProblem);
            var (verified, unverified) = machine.Verified(reply);
            // Bytes that do not hash to what the machine reports still show what it holds, and
            // that is all they are used for.
            var theirs = verified ?? (reply is { Envelope.Ok: not false, Hash: not null, Document: { } sent } ? ControllerDocument.FromText(sent) : null);
            if (theirs is null) return (null, unverified);
            var theirsHash = reply.Hash!;
            if (mine is not null && mine.Hash == theirsHash) return (null, $"{machine.Name} holds exactly this device's setup now.");
            var takeProblem = unverified ?? SetupMerge.WhyTheirsCannotMerge(theirs.Text);

            var different = mine is null || mine.Identity.Id != theirs.Identity.Id;
            if (takeProblem is not null && (!different || mine is null)) return (null, takeProblem);
            var preview = SetupConflictPreview.Between(_config.Config, theirs.Decoded);
            if (different || mine is null)
                return (new MergeSession(mine, theirs, theirsHash, null, Array.Empty<SetupDifference>(), preview,
                    DifferentSetup: true, MergeProblem: null, takeProblem), null);

            var baseHash = SetupSync.CommonBase(mine.Hash, theirs.Hash, mine.Identity.Lineage, theirs.Identity.Lineage,
                hash => _config.Ledger.Read(hash)?.Text);
            var baseDocument = baseHash is null ? null : _config.Ledger.Read(baseHash);
            // A kept revision is held to peer intake like anything else a merge reads. One this
            // build refuses, kept by an older one, is no base: every difference is then asked
            // about, which fails toward a question rather than toward a merge that cannot start.
            // Section 9.8 has not settled this yet, and the reference merge refuses such a base
            // instead, so Merge and Keep mine wait there where they are offered here.
            if (baseDocument is not null && SetupMerge.WhyTheirsCannotMerge(baseDocument.Text) is not null) baseDocument = null;
            var mergeProblem = SetupMerge.WhyMineCannotMerge(mine.Text);
            var differences = mergeProblem is null
                ? SetupMerge.Differences(mine.Text, theirs.Text, baseDocument?.Text)
                : Array.Empty<SetupDifference>();
            return (new MergeSession(mine, theirs, theirsHash, baseDocument, differences, preview, DifferentSetup: false, mergeProblem), null);
        }
        finally { _reconcileGate.Release(); }
    }

    /// Merges the two documents the screen showed, entry by entry. [choices] answers the entries a
    /// person was asked about; everything else follows whichever side changed it. The result
    /// descends from both heads, so every machine holding either of them fast-forwards to it. No
    /// network request: what is written is built from exactly what was shown.
    public Task<string?> MergeAsync(
        MachineModel machine, MergeSession session, IReadOnlyDictionary<string, SetupChoice>? choices,
        CancellationToken cancellationToken = default) =>
        WriteMergeAsync(machine, session, mine => SetupMerge.Merge(session.Base?.Text, mine.Text, session.Theirs.Text,
            mine.Hash, session.Theirs.Hash, SetupMerge.Writer.Here(Bindings.Device), choices), "merged", cancellationToken);

    /// Keeps this device's setup, in a revision the machine will accept: a merge with every entry
    /// answered mine. Not a replacement: the machine's head goes into the lineage, so what lands
    /// there is a descendant of what it holds and the push is an ordinary fast-forward.
    public Task<string?> KeepMineAsync(MachineModel machine, MergeSession session, CancellationToken cancellationToken = default) =>
        WriteMergeAsync(machine, session, mine => SetupMerge.KeepMine(session.Base?.Text, mine.Text, session.Theirs.Text,
            mine.Hash, session.Theirs.Hash, SetupMerge.Writer.Here(Bindings.Device)), "kept this device's setup over", cancellationToken);

    private async Task<string?> WriteMergeAsync(
        MachineModel machine, MergeSession session, Func<ControllerDocument, SetupMerge.Result> build, string what,
        CancellationToken cancellationToken)
    {
        if (!session.CanMerge) return session.MergeProblem ?? "These two setups cannot be merged.";
        var result = build(session.Mine!);
        if (result.Text is null)
            return $"The merge would break the setup rule {result.Error}, so nothing was written. Answer that entry the other way, or take one side whole.";
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            return _config.ApplyIfCurrent(ControllerDocument.FromText(result.Text), session.Mine!.Hash, $"{what} {machine.Id}").Problem;
        }
        finally { _reconcileGate.Release(); }
    }

    /// Adopts the machine's document the screen showed, byte for byte, dropping this device's. The
    /// document replaced is kept in the ledger first: Take theirs is the one decision here that
    /// throws away something somebody typed.
    public async Task<string?> TakeTheirsAsync(MachineModel machine, MergeSession session, CancellationToken cancellationToken = default)
    {
        if (!session.CanTakeTheirs) return session.TakeProblem;
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            var result = _config.ApplyIfCurrent(session.Theirs, session.Mine?.Hash, $"took the setup from {machine.Id}",
                expectedHash: session.Theirs.Hash);
            if (result is not ApplyResult.Applied) return result.Problem;
            Publish(machine.Id, new ReconcileOutcome.Adopted(session.Theirs.Hash));
            machine.NoteSharing(new SetupSharing.UpToDate());
            return null;
        }
        finally { _reconcileGate.Release(); }
    }

    /// Sends this device's document with `--replace`, which is the one command that overwrites a
    /// different setup. Only ever called from a control a person pressed after reading the preview,
    /// only while the setup here is still the one they read it against, and only while the
    /// machine's latest reading still names the document the preview was built from: `--replace`
    /// itself takes whatever the machine holds by then.
    public async Task<string?> ReplaceTheirsAsync(MachineModel machine, MergeSession session, CancellationToken cancellationToken = default)
    {
        if (!session.CanReplace) return $"{machine.Name} holds a copy of this setup, which is merged, kept or taken, never replaced.";
        await _reconcileGate.WaitAsync(cancellationToken);
        try
        {
            if (_config.Document is not { } mine) return "this device has no setup to send";
            if (_config.IsPendingEdit || mine.Hash != session.Mine?.Hash) return ConfigStore.HeadMovedSentence;
            if (machine.Status?.Controller?.Hash is { } holds && holds != session.TheirsHash)
                return $"{machine.Name} holds another document now, not the one shown. Nothing was sent; read it again first.";
            var state = StateOf(machine.Id);
            var (reply, problem) = await InFlight(state, () => machine.PushSetupAsync(mine, replace: true, cancellationToken));
            var reading = SetupSync.AfterPush(reply?.Json ?? Value.Missing, mine.Hash);
            state.Record(reading, reply, mine.Hash, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
            if (reading.Effect == PushEffect.Clear)
            {
                Publish(machine.Id, new ReconcileOutcome.Published(reply!.Action ?? "replaced"));
                machine.NoteSharing(new SetupSharing.JustShared());
                return null;
            }
            if (reading.Effect == PushEffect.Hold) Publish(machine.Id, Withheld(machine, state, mine.Hash));
            return problem ?? reply?.Envelope.Sentence() ?? $"{machine.Name} did not take the setup.";
        }
        finally { _reconcileGate.Release(); }
    }
}
