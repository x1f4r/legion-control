using System.Text.Json;

namespace LegionControl.Desktop.Contract;

/// What a client decides on (contract section 5.6). Deliberately not a boolean: "we do not know" is
/// a state the whole design exists to be able to say, and it must never collapse into either of
/// the other two.
///
/// Five words and no more. The finer things a person reads, that there was nothing to do, that a
/// queued request was cancelled or expired, that something else held the machine, come from the
/// action and the reason code beside it. They never decide what happens next: that is these.
public enum OperationOutcome
{
    /// The agent still owns the work, so it is followed with `op ID --wait 20`. A failed poll, a
    /// dropped link or a timeout never turns it into anything else.
    Pending,
    /// Finished, and the requested state holds, including "it already did".
    Succeeded,
    /// Finished, not in effect, and the state is known. A new attempt is a new request.
    Failed,
    /// Finished without acting, by design: busy, a policy, a lock, a request cancelled or expired
    /// before it ran. A new attempt is a new request, with Force offered only for busy.
    Deferred,
    /// No evidence either way. Never sent again on its own: the same id is read, and any new
    /// attempt is left to a person.
    Unknown,
}

/// The outcome of a reply or an operation, and the one function that derives it when the agent
/// did not say (reference contract/tools/outcome.mjs).
///
/// Agent 3.1.0 states `outcome` on every mutating reply, every record and every summary. Older
/// agents do not, and before this the clients each guessed it from `ok`, `action` and `reasonCode`
/// and guessed differently: this one called a refusal with a code unknown where the others called
/// it failed, and called an action it had never heard of a success because `ok` was true. This is
/// the reducer every client ports line for line, and contract/vectors/outcome.json is what proves
/// the copies agree.
///
/// Every field is read the way the reference reads it, never the way the rest of this client reads
/// a reply. `ok` is true only when it is the JSON value true, and an empty string is a string rather
/// than an absent field: coercing "yes" into true, or "" into nothing, would reduce some reply to a
/// different outcome here than on every other platform.
public static class Outcomes
{
    private static readonly HashSet<string> PendingActions = new(StringComparer.Ordinal)
    {
        "accepted", "already-running", "queued", "rebooting", "sleeping",
    };

    private static readonly HashSet<string> SuccessActions = new(StringComparer.Ordinal)
    {
        "updated", "restarted", "ran", "rebooted", "slept", "armed", "installed",
        "cycled", "noop", "checked", "skipped", "stored", "replaced",
    };

    private static readonly HashSet<string> DeferredActions = new(StringComparer.Ordinal) { "deferred", "conflict", "expired" };

    /// The outcome of a reply, a summary or a cycle child: its own fields.
    public static OperationOutcome Of(Value json) =>
        Reduce(json["outcome"], json["state"], json["ok"], json["action"], json["reasonCode"]);

    /// The outcome of an operation record: its result, read in the light of its state and of the
    /// outcome stated on the record or, failing that, on the result. `result` is null until the
    /// record is finished.
    public static OperationOutcome OfRecord(Value op)
    {
        var result = op["result"];
        var stated = op["outcome"].Exists ? op["outcome"] : result["outcome"];
        return Reduce(stated, op["state"], result["ok"], result["action"], result["reasonCode"]);
    }

    /// Exactly the pseudo-code of contract section 5.6.
    public static OperationOutcome Reduce(Value outcome, Value state, Value ok, Value action, Value reasonCode)
    {
        // 1. A stated outcome wins, but only one this version knows. A value from a later contract
        //    is not a reason to believe anything.
        if (outcome.Raw.ValueKind == JsonValueKind.String) return FromWire(outcome.Raw.GetString()) ?? OperationOutcome.Unknown;

        // 2. Work the agent still owns is pending whatever else the object says; a running record
        //    may already carry the action it is aiming at.
        if (Word(state) is "queued" or "running") return OperationOutcome.Pending;

        // 3. Otherwise the action decides, with the reason code where one action covers two
        //    different endings.
        var code = Word(reasonCode);
        if (Word(action) is not { } word)
        {
            if (ok.Raw.ValueKind == JsonValueKind.True) return code is null ? OperationOutcome.Succeeded : OperationOutcome.Unknown;
            if (ok.Raw.ValueKind == JsonValueKind.False)
            {
                return code switch
                {
                    "operation-in-progress" or "lock-held" => OperationOutcome.Deferred,
                    "timed-out" or "interrupted" or "internal" => OperationOutcome.Unknown,
                    _ => OperationOutcome.Failed,
                };
            }
            return OperationOutcome.Unknown;
        }
        if (PendingActions.Contains(word)) return OperationOutcome.Pending;
        if (SuccessActions.Contains(word)) return OperationOutcome.Succeeded;
        if (DeferredActions.Contains(word)) return OperationOutcome.Deferred;
        return word switch
        {
            // A cancelled request was itself deferred; a cancel that worked succeeded.
            "cancelled" => code == "cancelled" ? OperationOutcome.Deferred : OperationOutcome.Succeeded,
            // The old version is back and running: success only when nothing went wrong.
            "rolled-back" => code is null ? OperationOutcome.Succeeded : OperationOutcome.Failed,
            "interrupted" => OperationOutcome.Unknown,
            "failed" => code is "timed-out" or "interrupted" ? OperationOutcome.Unknown : OperationOutcome.Failed,
            _ => OperationOutcome.Unknown,
        };
    }

    /// The outcome's word on the wire: succeeded, failed, deferred, pending, unknown.
    public static string Wire(this OperationOutcome outcome) => outcome.ToString().ToLowerInvariant();

    /// The outcome a wire word names, or null for a word this version does not know.
    public static OperationOutcome? FromWire(string? word) => word switch
    {
        "succeeded" => OperationOutcome.Succeeded,
        "failed" => OperationOutcome.Failed,
        "deferred" => OperationOutcome.Deferred,
        "pending" => OperationOutcome.Pending,
        "unknown" => OperationOutcome.Unknown,
        _ => null,
    };

    /// A field as the reference compares it: a string as itself, null or absent as null, and any
    /// other value as its JSON text, which equals no word the reducer looks for, just as a number
    /// equals none of them there.
    private static string? Word(Value value) => !value.Exists
        ? null
        : value.Raw.ValueKind == JsonValueKind.String ? value.Raw.GetString() : value.Raw.GetRawText();
}
