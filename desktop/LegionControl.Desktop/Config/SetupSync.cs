using System.Text;
using System.Text.Json;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// What one status of a machine calls for (contract section 9.5).
public enum SetupDecision
{
    /// An agent before contract 3, or one that carries no setup: left out, never a conflict.
    Unsupported,
    /// This device has no setup to compare, or one without an identity yet.
    Unknown,
    /// The machine holds nothing, a copy nobody claims, or an ancestor of this device's document.
    Push,
    InSync,
    /// Another setup altogether: a person decides, with a preview.
    DifferentSetup,
    /// The machine's lineage is needed to tell behind from diverged: `config meta`, once.
    ReadMeta,
    /// The machine holds a descendant of this device's document.
    Fetch,
    /// Same setup, and neither descends from the other: a person decides.
    Diverged,
}

/// A decision, why, and the hash a fetch must produce.
public sealed record Decision(SetupDecision Kind, string? Reason = null, string? Expect = null);

/// This device's applied document as the decision reads it.
public sealed record LocalHead(string Hash, string? Id, IReadOnlyList<string> Lineage)
{
    public static LocalHead Of(ControllerDocument document) => new(document.Hash, document.Identity.Id, document.Identity.Lineage);
}

/// Whether the bytes a fetch read may replace the applied document.
public enum Admission
{
    /// The setup here changed while the machine was read: nothing is written.
    HeadMoved,
    /// The machine changed since the decision: the next status decides.
    Moved,
    /// The bytes are not the ones the machine's hash covers: an agent that sends no `raw` and a
    /// document this device cannot rebuild exactly.
    Unverifiable,
    /// The bytes fail section 9.7 under peer intake.
    Invalid,
    /// The bytes' own identity is not the one the decision was made from.
    Inconsistent,
    Adopt,
}

/// What [SetupSync.Gate] lets happen.
public enum GateAction
{
    Act,
    Hold,
    None,
}

public sealed record GateVerdict(GateAction Action, string? Reason = null);

/// What a push reply means for the next decision.
public enum PushEffect
{
    /// Stored, or already held: the failure, if any, is forgotten.
    Clear,
    /// A failure to back off from.
    Fail,
    /// The machine said what it holds instead: filed under its hash, and decided again at once.
    FileMeta,
    /// Refused while an operation holds the machine's lock: tried again after a later status.
    Locked,
    /// Refused in a way no retry fixes: held for this applied hash.
    Hold,
}

public sealed record PushReading(PushEffect Effect, string? MetaHash = null);

/// The guardrails' view of one machine, at the moment a decision is gated.
public sealed record GateState(
    bool InFlight,
    IReadOnlyDictionary<string, long> FailedAt,
    IReadOnlyDictionary<string, string> Held,
    bool LocalInvalid,
    bool LocalMutation,
    int RunningOperations,
    long? LockedAt,
    long? StatusAt,
    IReadOnlyCollection<string> Settled);

/// What this device remembers about one machine, in memory only (contract section 9.5).
///
/// Nothing in here says "handled". The decision is worked out again from the hashes on every
/// status; this holds only what the guardrails need, and the `config meta` answers filed under the
/// hash they describe, which never go stale because a hash fixes the document.
public sealed class PeerState
{
    /// How many `config meta` answers are kept. Each is one hash a machine was seen holding.
    public const int MetaKept = 8;

    private readonly List<string> _metaOrder = new();

    public Dictionary<string, ControllerMark> MetaByHash { get; } = new(StringComparer.Ordinal);
    public bool InFlight { get; set; }
    public Dictionary<string, long> FailedAt { get; } = new(StringComparer.Ordinal);
    /// The refusal code a push of an applied hash was held for: `restricted` or `config-invalid`.
    public Dictionary<string, string> Held { get; } = new(StringComparer.Ordinal);
    public long? LockedAt { get; set; }
    /// The admission refusals remembered, by [SetupSync.SettledKey].
    public HashSet<string> Settled { get; } = new(StringComparer.Ordinal);
    /// Why the last push of each applied hash was held, in the machine's own words.
    public Dictionary<string, string> HeldBecause { get; } = new(StringComparer.Ordinal);

    /// Files what a machine said it holds under the hash it describes. A mark without a hash says
    /// nothing that could be filed.
    public void File(ControllerMark mark)
    {
        if (mark.Hash is not { } hash) return;
        if (!MetaByHash.ContainsKey(hash)) _metaOrder.Add(hash);
        MetaByHash[hash] = mark;
        while (_metaOrder.Count > MetaKept)
        {
            MetaByHash.Remove(_metaOrder[0]);
            _metaOrder.RemoveAt(0);
        }
    }

    /// The state after a push reply (`recordPush` in the reference): `clear` forgets the failure,
    /// `fail` starts the back-off, `fileMeta` files `current`, `locked` waits for a later status and
    /// `hold` remembers the refusal for the applied hash. Only "Try again" or a new applied hash
    /// ends a hold.
    public void Record(PushReading reading, ConfigReply? reply, string localHash, long now)
    {
        InFlight = false;
        switch (reading.Effect)
        {
            case PushEffect.Clear:
                FailedAt.Remove(localHash);
                break;
            case PushEffect.Fail:
                FailedAt[localHash] = now;
                break;
            case PushEffect.FileMeta:
                File(reply!.Current!);
                break;
            case PushEffect.Locked:
                LockedAt = now;
                break;
            case PushEffect.Hold:
                Held[localHash] = reply!.Envelope.ReasonCode!;
                HeldBecause[localHash] = reply.Problems.FirstOrDefault()?.Sentence ?? reply.Envelope.Sentence() ?? reply.Envelope.ReasonCode!;
                break;
        }
    }

    /// "Try again": a held push of [localHash] goes out on the next status.
    public void Release(string localHash)
    {
        Held.Remove(localHash);
        HeldBecause.Remove(localHash);
    }

    public GateState View(bool localInvalid, bool localMutation, int runningOperations, long? statusAt) =>
        new(InFlight, FailedAt, Held, localInvalid, localMutation, runningOperations, LockedAt, statusAt, Settled);
}

/// Setup sync: the reference of contract section 9.5, contract/tools/setup-sync.mjs, ported step by
/// step and held to contract/vectors/setup-reconcile.json.
///
/// Before this the desktop decided from a status alone where it could, read the machine's lineage
/// where it could not, and remembered what it had pushed and when it had failed per machine, with
/// rules of its own that the other two clients did not share. Each step is one function now:
/// [Decide] the eleven ordered rules, [Admit] whether fetched bytes may be adopted, [Gate] the
/// guardrails, [AfterPush] what a push reply means. [Model.Reconciler] runs them; nothing here
/// touches a machine, a clock or a file.
public static class SetupSync
{
    /// How long nothing automatic goes to a machine for the same applied hash after a failure.
    public const long BackoffMs = 60_000;

    /// The first of the eleven rules that matches.
    ///
    /// [contract] and [controller] are what the machine's status said; [metaByHash] is what this
    /// device filed about hashes the machine reported. Rule 7 is checked before rule 8 on purpose:
    /// a machine that holds another setup whose hash happens to be in this device's lineage is
    /// still another setup, and pushing over it would replace somebody else's fleet.
    public static Decision Decide(
        LocalHead? local, int? contract, ControllerMark? controller, IReadOnlyDictionary<string, ControllerMark> metaByHash)
    {
        if (!(contract >= 3) || controller is null) return new Decision(SetupDecision.Unsupported);
        if (local is null) return new Decision(SetupDecision.Unknown, "no-local");
        if (string.IsNullOrEmpty(local.Id)) return new Decision(SetupDecision.Unknown, "local-unidentified");
        if (string.IsNullOrEmpty(controller.Hash)) return new Decision(SetupDecision.Push, "machine-empty");
        if (controller.Hash == local.Hash) return new Decision(SetupDecision.InSync);
        var meta = metaByHash.GetValueOrDefault(controller.Hash);
        var id = meta is not null ? meta.Id : controller.Id;
        if (string.IsNullOrEmpty(id)) return new Decision(SetupDecision.Push, "machine-unidentified");
        if (id != local.Id) return new Decision(SetupDecision.DifferentSetup);
        if (local.Lineage.Contains(controller.Hash, StringComparer.Ordinal)) return new Decision(SetupDecision.Push, "machine-behind");
        if (meta is null) return new Decision(SetupDecision.ReadMeta);
        if (meta.Lineage.Contains(local.Hash, StringComparer.Ordinal)) return new Decision(SetupDecision.Fetch, Expect: controller.Hash);
        return new Decision(SetupDecision.Diverged);
    }

    /// Whether fetched bytes may be adopted, checked in this order.
    ///
    /// The head check is first because it is the one that protects a person: an edit saved while
    /// the read was in flight is never overwritten by bytes chosen against the document it
    /// replaced. A hash that is not the expected one but is the reply's own means the machine moved
    /// on; one that is neither means the bytes could not be reproduced, which is what an agent
    /// that sends no `raw` gives a device that rebuilt them in another spelling. A reply
    /// without a hash never gets here: it is a failed request.
    public static Admission Admit(
        string expectedLocalHash, string expectedLocalId, string expectedRemoteHash,
        string? currentLocalHash,
        string fetchedHash, string replyHash, string? fetchedId, IReadOnlyList<string> fetchedLineage,
        bool accepted)
    {
        if (currentLocalHash != expectedLocalHash) return Admission.HeadMoved;
        if (fetchedHash != expectedRemoteHash) return replyHash == fetchedHash ? Admission.Moved : Admission.Unverifiable;
        if (!accepted) return Admission.Invalid;
        if (fetchedId != expectedLocalId || !fetchedLineage.Contains(expectedLocalHash, StringComparer.Ordinal)) return Admission.Inconsistent;
        return Admission.Adopt;
    }

    /// Whether this admission is remembered, so the same refused document is not read again on
    /// every status until the applied hash, the machine's hash or its agent changes.
    public static bool IsRemembered(Admission admission) =>
        admission is Admission.Invalid or Admission.Unverifiable or Admission.Inconsistent;

    /// The key a remembered admission is filed under.
    public static string SettledKey(string localHash, string? remoteHash, string? agentVersion) =>
        $"{localHash} {remoteHash ?? "null"} {agentVersion ?? "null"}";

    /// Whether the request a decision calls for goes out now.
    ///
    /// Only push, readMeta and fetch send anything. One request is in flight per machine, and a
    /// failure holds everything automatic for the same applied hash for a minute. The rest is for
    /// pushes only, because reading a machine never changes it: a device whose document has a
    /// section 9.7 error publishes nothing, a push refused with `restricted` or `config-invalid`
    /// is not sent again for that hash, and nothing is pushed while this device has an action in
    /// flight on the machine, while its last status lists a running operation, or after a lock
    /// refusal until a status read after it.
    public static GateVerdict Gate(SetupDecision decision, string localHash, string? remoteHash, string? agentVersion, long now, GateState state)
    {
        if (decision is not (SetupDecision.Push or SetupDecision.ReadMeta or SetupDecision.Fetch)) return new GateVerdict(GateAction.None);
        if (state.InFlight) return new GateVerdict(GateAction.Hold, "in-flight");
        if (state.FailedAt.TryGetValue(localHash, out var failed) && now - failed < BackoffMs) return new GateVerdict(GateAction.Hold, "backoff");
        if (decision == SetupDecision.Push)
        {
            if (state.LocalInvalid) return new GateVerdict(GateAction.Hold, "local-invalid");
            if (state.Held.ContainsKey(localHash)) return new GateVerdict(GateAction.Hold, "held");
            // A status with no time behind it reads as older than any refusal, as in the reference.
            if (state.LocalMutation || state.RunningOperations > 0 || (state.LockedAt is { } locked && (state.StatusAt ?? 0) <= locked))
                return new GateVerdict(GateAction.Hold, "operation-running");
        }
        if (decision == SetupDecision.Fetch && state.Settled.Contains(SettledKey(localHash, remoteHash, agentVersion)))
            return new GateVerdict(GateAction.Hold, "settled");
        return new GateVerdict(GateAction.Act);
    }

    /// What a push reply means. [reply] is the reply exactly as it came, or [Value.Missing] when
    /// none was read: no answer, a dropped link, a timeout, an outcome nobody can know. That and
    /// every refusal not named here is a failure to back off from.
    ///
    /// Only a reply with `ok: false` is a refusal: the agent sends that with every refusal code, so
    /// a code on anything else is a reply this device cannot account for. And meta is filed under
    /// the hash it describes, so a `current` without one says nothing about what the machine holds.
    /// A reply that stored something other than the applied hash is a failure too: the machine now
    /// holds a document this device did not send. The JSON is read as it is, never coerced: `"ok":
    /// "true"` is not true.
    public static PushReading AfterPush(Value reply, string localHash)
    {
        if (!reply.IsObject) return new PushReading(PushEffect.Fail);
        var raw = reply.Raw;
        var ok = raw.TryGetProperty("ok", out var written) ? written.ValueKind : JsonValueKind.Undefined;
        if (ok == JsonValueKind.True)
        {
            var stored = raw.TryGetProperty("hash", out var hash) && hash.ValueKind == JsonValueKind.String ? hash.GetString() : null;
            return new PushReading(stored == localHash ? PushEffect.Clear : PushEffect.Fail);
        }
        if (ok != JsonValueKind.False) return new PushReading(PushEffect.Fail);
        var code = raw.TryGetProperty("reasonCode", out var reason) && reason.ValueKind == JsonValueKind.String ? reason.GetString() : null;
        if (code is ReasonCode.StaleRevision or ReasonCode.ControllerConflict
            && raw.TryGetProperty("current", out var current) && current.ValueKind == JsonValueKind.Object
            && current.TryGetProperty("hash", out var currentHash) && currentHash.ValueKind == JsonValueKind.String)
        {
            return new PushReading(PushEffect.FileMeta, currentHash.GetString());
        }
        if (code is ReasonCode.OperationInProgress or ReasonCode.LockHeld) return new PushReading(PushEffect.Locked);
        if (code is ReasonCode.Restricted or ReasonCode.ConfigInvalid) return new PushReading(PushEffect.Hold);
        return new PushReading(PushEffect.Fail);
    }

    /// The base of a merge (contract section 9.8): the first hash of mine's lineage, newest first,
    /// that is also in theirs' lineage and whose kept bytes still hash to it; null when there is
    /// none. The heads themselves are never a base: a merge whose base was one side would only
    /// ever take the other side.
    public static string? CommonBase(
        string mineHash, string theirsHash, IReadOnlyList<string> mineLineage, IReadOnlyList<string> theirsLineage,
        Func<string, string?> kept)
    {
        foreach (var hash in mineLineage)
        {
            if (hash == mineHash || hash == theirsHash || !theirsLineage.Contains(hash, StringComparer.Ordinal)) continue;
            if (kept(hash) is { } text && Canonical.Hash(Encoding.UTF8.GetBytes(text)) == hash) return hash;
        }
        return null;
    }

    /// The lineage of a merge: mine's hash, theirs' hash, then the two lineages interleaved, mine
    /// first, each hash once, cut at 32. Both heads are in it, so both branches fast-forward onto
    /// the result. Interleaved rather than concatenated: with a cap of 32, the recent history of
    /// both sides is worth more than all of one side's and none of the other's.
    public static IReadOnlyList<string> MergedLineage(
        string mineHash, string theirsHash, IReadOnlyList<string> mineLineage, IReadOnlyList<string> theirsLineage)
    {
        var lineage = new List<string>();
        void Add(string hash)
        {
            if (!lineage.Contains(hash, StringComparer.Ordinal)) lineage.Add(hash);
        }
        Add(mineHash);
        Add(theirsHash);
        for (var index = 0; index < Math.Max(mineLineage.Count, theirsLineage.Count); index += 1)
        {
            if (index < mineLineage.Count) Add(mineLineage[index]);
            if (index < theirsLineage.Count) Add(theirsLineage[index]);
        }
        return lineage.Take(ControllerIdentity.LineageLimit).ToList();
    }

    // The contract spells every value of this section in camel case: inSync, readMeta, fileMeta.
    public static string Wire(this SetupDecision decision) => CamelCase(decision.ToString());

    public static string Wire(this Admission admission) => CamelCase(admission.ToString());

    public static string Wire(this GateAction action) => CamelCase(action.ToString());

    public static string Wire(this PushEffect effect) => CamelCase(effect.ToString());

    private static string CamelCase(string name) => char.ToLowerInvariant(name[0]) + name[1..];
}
