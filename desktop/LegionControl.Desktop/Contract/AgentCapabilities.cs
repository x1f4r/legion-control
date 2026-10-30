using System.Text.RegularExpressions;

namespace LegionControl.Desktop.Contract;

/// What the agent on the far side can be asked to do.
///
/// One number settles it. Every reply from a 3.x agent carries `"contract": 3`, and a client gates
/// every v3 feature on `contract >= 3`; absence means a 2.x agent and only the baseline verbs
/// exist there. That is deliberately not a per-verb capability list: an agent either implements
/// contract 3 or it does not, and a partial one would be a bug rather than a configuration.
///
/// Nothing here guesses. A verb the far side has not shown it understands is never sent, and a
/// control that needs one says what it needs instead of failing when pressed.
public sealed record AgentCapabilities
{
    /// The `contract` number from the last reply. Zero until something has answered.
    public int ContractVersion { get; init; }
    public string? AgentVersion { get; init; }
    /// The version floor the status reported, when it reported one.
    public string? Floor { get; init; }
    /// Whether the status carried a floor at all, null included: agent 3.1.0 and later.
    public bool ReportsFloor { get; init; }
    /// Whether this machine's own session is restricted, as its last status said.
    public bool RestrictedSession { get; init; }

    public static readonly AgentCapabilities None = new();

    public static AgentCapabilities Of(AgentStatus status) => new()
    {
        ContractVersion = status.ContractVersion,
        AgentVersion = status.AgentVersion,
        Floor = status.Agent?.Floor,
        ReportsFloor = status.Agent?.ReportsFloor ?? false,
        RestrictedSession = status.Agent?.RestrictedSession == true,
    };

    public static AgentCapabilities Of(AgentEnvelope envelope) => new()
    {
        ContractVersion = envelope.ContractVersion ?? 0,
        AgentVersion = envelope.AgentVersion,
    };

    /// Whether every v3 command and flag this app knows is safe to send.
    public bool SpeaksV3 => ContractVersion >= AgentContract.RequiredContract;

    /// Whether the far side has moved past what this build understands. Worth saying rather than
    /// hiding: the app keeps working against the part of the contract it knows.
    public bool IsNewerThanThisApp => ContractVersion > AgentContract.RequiredContract;

    /// Whether anything has answered yet.
    public bool IsUnknown => ContractVersion <= 0;

    // Everything below is one idea spelled once. They all say "contract 3" and they exist so that
    // the call sites read as what they need rather than as a version test.
    public bool SupportsOperationIds => SpeaksV3;
    public bool SupportsOperationQuery => SpeaksV3;
    public bool SupportsCancel => SpeaksV3;
    public bool SupportsDetach => SpeaksV3;
    public bool SupportsQueueUntilIdle => SpeaksV3;
    public bool SupportsPolicy => SpeaksV3;
    public bool SupportsCycle => SpeaksV3;
    public bool SupportsDoctor => SpeaksV3;
    public bool SupportsLogs => SpeaksV3;
    public bool SupportsHistory => SpeaksV3;
    public bool SupportsBundle => SpeaksV3;
    public bool SupportsAgentDeploy => SpeaksV3;
    /// Whether the machine can carry the setup with an identity and a lineage. A 2.x agent stores
    /// a document without either and is excluded from reconciliation rather than fought with.
    public bool SupportsSetupLineage => SpeaksV3;
    public bool SupportsConfigMeta => SpeaksV3;

    /// Why a control that needs contract 3 is disabled, in a sentence the window shows beside it,
    /// or null when it is not. A 2.x agent sends no contract number at all, so its version is what
    /// says it answered.
    public string? UnsupportedReason => SpeaksV3 ? null
        : IsUnknown && AgentVersion is null ? "Waits for this machine's agent to answer."
        : $"Needs agent 3.0 or later; this machine runs {AgentVersion ?? "an older one"}.";

    /// Whether `self-update --allow-downgrade` may be sent: only to an agent that reports a version
    /// floor, which every agent that knows the flag does, and never over a restricted key, which the
    /// dispatcher refuses it from. An older agent has no floor to cross and has never heard of the
    /// flag, so nothing in this app takes it back to an older agent.
    public bool SupportsAllowDowngrade => SpeaksV3 && ReportsFloor && !RestrictedSession;

    /// One line for the machine section.
    public string Summary => ContractVersion switch
    {
        <= 0 => "not read yet",
        var version when version < AgentContract.RequiredContract => $"contract {version}, older than this app needs",
        var version when version > AgentContract.RequiredContract => $"contract {version}, newer than this app knows",
        var version => $"contract {version}",
    };
}

/// Agent versions, compared the way the version floor compares them (contract section 13): the
/// three leading numbers, numerically, and whatever follows them ignored. A version without three
/// leading numbers compares with nothing.
public static class AgentVersions
{
    private static readonly Regex Leading = new(@"\A([0-9]+)\.([0-9]+)\.([0-9]+)", RegexOptions.CultureInvariant);

    /// Negative when [left] is the older, zero when the same, positive when the newer; null when
    /// either is not a version.
    public static int? Compare(string? left, string? right)
    {
        if (Parse(left) is not { } a || Parse(right) is not { } b) return null;
        for (var index = 0; index < 3; index += 1)
        {
            // Digits without their leading zeros: the longer is the larger, and of two as long the
            // first that differs decides. No number is too long to compare that way.
            var (x, y) = (a[index], b[index]);
            if (x.Length != y.Length) return x.Length.CompareTo(y.Length);
            var order = string.CompareOrdinal(x, y);
            if (order != 0) return Math.Sign(order);
        }
        return 0;
    }

    /// Whether [version] is known to be older than [other]. Not knowing is not older.
    public static bool IsOlder(string? version, string? other) => Compare(version, other) < 0;

    private static string[]? Parse(string? version)
    {
        if (version is null || Leading.Match(version) is not { Success: true } match) return null;
        return new[] { match.Groups[1], match.Groups[2], match.Groups[3] }
            .Select(group => group.Value.TrimStart('0') is { Length: > 0 } digits ? digits : "0")
            .ToArray();
    }
}

/// Facts about the contract this build was written against.
public static class AgentContract
{
    /// What the v3 features need. An agent below this still works for everything it does have.
    public const int RequiredContract = 3;

    /// The agent version this build carries a bundle of, when it carries one.
    public const string BundledAgentVersion = "3.1.0";

    public static string ClientVersion { get; } =
        (typeof(AgentContract).Assembly.GetName().Version
            ?? throw new InvalidOperationException("The desktop assembly has no version.")).ToString(3);

    /// What this client calls itself on the wire. Fixed by the contract; never a platform name.
    public const string ClientKind = "desktop";
}
