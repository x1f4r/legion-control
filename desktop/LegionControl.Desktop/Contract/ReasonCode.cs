namespace LegionControl.Desktop.Contract;

/// Why the agent did not do the happy path, as a code rather than as prose.
///
/// The set is closed on the agent's side and open here on purpose: a code this build has never
/// heard of is rendered from the accompanying message rather than dropped, because the alternative
/// is a client that goes quiet the moment the agent learns a new word.
public static class ReasonCode
{
    public const string Busy = "busy";
    public const string BusyUnknown = "busy-unknown";
    public const string PolicyOff = "policy-off";
    public const string PolicyPaused = "policy-paused";
    public const string OutsideWindow = "outside-window";
    public const string OperationInProgress = "operation-in-progress";
    public const string LockHeld = "lock-held";
    public const string NoUpdate = "no-update";
    public const string LatestUnknown = "latest-unknown";
    public const string NotInstalled = "not-installed";
    public const string NotRunning = "not-running";
    public const string AppClosed = "app-closed";
    public const string ApplyAttemptsExhausted = "apply-attempts-exhausted";
    public const string ApplyFailed = "apply-failed";
    public const string PostconditionFailed = "postcondition-failed";
    public const string RolledBack = "rolled-back";
    public const string NotConfigured = "not-configured";
    public const string UnsupportedPlatform = "unsupported-platform";
    public const string UnknownService = "unknown-service";
    public const string UnknownTarget = "unknown-target";
    public const string UnknownAction = "unknown-action";
    public const string BadArgument = "bad-argument";
    public const string ConfigInvalid = "config-invalid";
    public const string ConfigMissing = "config-missing";
    public const string Interrupted = "interrupted";
    public const string Expired = "expired";
    public const string Cancelled = "cancelled";
    public const string AlreadyRunning = "already-running";
    public const string AlreadyOnTarget = "already-on-target";
    public const string StaleRevision = "stale-revision";
    public const string ControllerConflict = "controller-conflict";
    public const string SignatureInvalid = "signature-invalid";
    public const string Restricted = "restricted";
    public const string TimedOut = "timed-out";
    public const string Internal = "internal";

    /// One sentence for a code, or null when this build does not know it and the agent's own
    /// message has to stand in.
    public static string? Describe(string? code, string? service = null)
    {
        var subject = service is null ? "The service" : service;
        return code switch
        {
            null => null,
            Busy => $"{subject} is working, so nothing was changed.",
            BusyUnknown => $"Whether {subject} is working could not be read, so nothing was changed.",
            PolicyOff => "The scheduled cycle is turned off here. Asking for it directly still works.",
            PolicyPaused => "The scheduled cycle is paused. Asking for it directly still works.",
            OutsideWindow => "This is outside the maintenance window. Asking for it directly still works.",
            OperationInProgress => "Another operation is already running on this machine.",
            LockHeld => "Another operation holds the machine lock.",
            NoUpdate => "There is nothing newer to install.",
            LatestUnknown => "The newest version could not be looked up, so nothing was installed.",
            NotInstalled => $"{subject} is not installed.",
            NotRunning => $"{subject} is not running.",
            AppClosed => "The app is not open, so there was nothing to act on.",
            ApplyAttemptsExhausted => "The install was tried and did not take.",
            ApplyFailed => "The install command failed.",
            PostconditionFailed => "The command finished but the version afterwards is not the one asked for.",
            RolledBack => "The install failed and the previous version was put back.",
            NotConfigured => "Nothing is configured for that here.",
            UnsupportedPlatform => "This system cannot do that.",
            UnknownService => "That service is not in the machine's configuration.",
            UnknownTarget => "That boot target is not in the machine's configuration.",
            UnknownAction => "That action is not in the machine's configuration.",
            BadArgument => "The agent refused an argument this app sent.",
            ConfigInvalid => "The agent's own configuration file is not valid, so it refuses to change anything.",
            ConfigMissing => "The agent has no configuration file.",
            Interrupted => "The operation was cut off before it finished. What it left behind is on the machine.",
            Expired => "The queued request expired before the machine was idle.",
            Cancelled => "The request was cancelled.",
            AlreadyRunning => "That operation is already running.",
            AlreadyOnTarget => "The machine is already on that system.",
            StaleRevision => "The machine holds a newer revision of the setup than this one.",
            ControllerConflict => "The machine holds a different setup, and neither is an older copy of the other.",
            SignatureInvalid => "The signature did not verify, so nothing was installed.",
            Restricted => "This key is not allowed to run that.",
            TimedOut => "The agent gave up waiting.",
            Internal => "The agent hit an error of its own.",
            _ => null,
        };
    }

    /// Whether a code says "the machine deliberately did nothing", which is the family the force
    /// sheet is offered for. Only busy is ever bypassed by force: a policy is bypassed by asking
    /// directly, and an unknown outcome is never bypassed at all.
    public static bool IsForceable(string? code) => code is Busy or BusyUnknown;

    /// Whether the code describes a refusal that another route or another system could answer
    /// differently. None of them do: they all come from an agent that answered.
    public static bool IsDeferral(string? code) =>
        code is Busy or BusyUnknown or PolicyOff or PolicyPaused or OutsideWindow;
}
