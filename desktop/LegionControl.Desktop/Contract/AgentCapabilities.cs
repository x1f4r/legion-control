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

    public static readonly AgentCapabilities None = new();

    public static AgentCapabilities Of(AgentStatus status) => new()
    {
        ContractVersion = status.ContractVersion,
        AgentVersion = status.AgentVersion,
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

    /// One line for the machine section.
    public string Summary => ContractVersion switch
    {
        <= 0 => "not read yet",
        var version when version < AgentContract.RequiredContract => $"contract {version}, older than this app needs",
        var version when version > AgentContract.RequiredContract => $"contract {version}, newer than this app knows",
        var version => $"contract {version}",
    };
}

/// Facts about the contract this build was written against.
public static class AgentContract
{
    /// What the v3 features need. An agent below this still works for everything it does have.
    public const int RequiredContract = 3;

    /// The agent version this build carries a bundle of, when it carries one.
    public const string BundledAgentVersion = "3.0.0";

    public const string ClientVersion = "1.3.0";

    /// What this client calls itself on the wire. Fixed by the contract; never a platform name.
    public const string ClientKind = "desktop";
}
