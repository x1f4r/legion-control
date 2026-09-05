using System.Text.RegularExpressions;

namespace LegionControl.Desktop.Contract;

/// Everything a command needs beyond its verb.
public sealed record CommandOptions
{
    public string? Service { get; init; }
    public string? Target { get; init; }
    public string? ActionId { get; init; }
    /// Skips the busy gate and nothing else. Never a policy, never a lock, never a missing
    /// postcondition.
    public bool Force { get; init; }
    public string? OperationId { get; init; }
    public bool Detach { get; init; }
    public bool WhenIdle { get; init; }
    /// A duration in the agent's own grammar: 30m, 4h, 2d.
    public string? Expires { get; init; }
    public bool NoReboot { get; init; }
    public bool DryRun { get; init; }
}

/// Turns one intention into the exact argv that goes over ssh.
///
/// Kept apart from the transport so the bytes can be asserted in a test rather than inferred from a
/// machine that may or may not be awake, and kept apart from the models so that the whole command
/// surface is one file to read.
///
/// Every v3 flag here is gated on the agent having said `contract >= 3`. A flag an older agent has
/// never heard of is not ignored there: it fails the whole command, and a person with one
/// out-of-date machine would lose the ability to update it at all.
public static class CommandSurface
{
    /// The grammar every token the agent accepts has to match. Checked here as well as there,
    /// because a token this client cannot see the far side reject comes back as a puzzling failure
    /// rather than as a refusal to send something malformed.
    private static readonly Regex TokenGrammar = new(@"^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$", RegexOptions.Compiled);

    /// Operation ids are narrower still: lower case, and long enough not to collide.
    private static readonly Regex OperationIdGrammar = new("^[a-z0-9-]{8,64}$", RegexOptions.Compiled);

    private static readonly Regex DurationGrammar = new(@"^\d+(m|h|d)$", RegexOptions.Compiled);

    public static bool IsValidToken(string? value) => value is not null && TokenGrammar.IsMatch(value);

    public static bool IsValidOperationId(string? value) => value is not null && OperationIdGrammar.IsMatch(value);

    public static bool IsValidDuration(string? value) => value is not null && DurationGrammar.IsMatch(value);

    /// A fresh operation id in the shape the agent accepts.
    public static string NewOperationId() => Guid.NewGuid().ToString("d").ToLowerInvariant();

    /// Thrown before anything is sent when a value would not survive the agent's own validation.
    public sealed class InvalidArgument(string argument, string value)
        : Exception($"The value \"{value}\" is not a valid {argument} for the agent, so nothing was sent.")
    {
        public string Argument { get; } = argument;
        public string Value { get; } = value;
    }

    private static void Require(string argument, string? value)
    {
        if (!IsValidToken(value)) throw new InvalidArgument(argument, value ?? "");
    }

    /// Thrown when a v3 verb is asked of an agent that has not said it speaks contract 3.
    public sealed class NotAvailable(string verb, int contractVersion)
        : Exception($"The agent answers contract {contractVersion}, which has no \"{verb}\". Nothing was sent.")
    {
        public string Verb { get; } = verb;
    }

    private static void RequireV3(string verb, AgentCapabilities capabilities)
    {
        if (!capabilities.SpeaksV3) throw new NotAvailable(verb, capabilities.ContractVersion);
    }

    // MARK: the baseline, understood by every agent

    public static string[] Status(AgentCapabilities capabilities, int? budgetMs = null)
    {
        if (!capabilities.SpeaksV3 || budgetMs is null) return new[] { "status" };
        return new[] { "status", "--budget-ms", budgetMs.Value.ToString() };
    }

    public static string[] Busy() => new[] { "busy" };

    public static string[] Version() => new[] { "version" };

    public static string[] Help() => new[] { "help" };

    public static string[] ReadConfig() => new[] { "config" };

    public static string[] Update(CommandOptions options, AgentCapabilities capabilities)
    {
        var argv = new List<string> { "update" };
        AddService(argv, options.Service);
        if (options.Force) argv.Add("--force");
        AddOperation(argv, options, capabilities);
        return argv.ToArray();
    }

    public static string[] Restart(CommandOptions options, AgentCapabilities capabilities)
    {
        var argv = new List<string> { "restart" };
        AddService(argv, options.Service);
        if (options.Force) argv.Add("--force");
        AddOperation(argv, options, capabilities);
        return argv.ToArray();
    }

    public static string[] Boot(CommandOptions options, AgentCapabilities capabilities)
    {
        Require("boot target", options.Target);
        var argv = new List<string> { "boot", options.Target! };
        if (options.Force) argv.Add("--force");
        if (options.NoReboot) argv.Add("--no-reboot");
        AddOperation(argv, options, capabilities);
        return argv.ToArray();
    }

    public static string[] Sleep(CommandOptions options, AgentCapabilities capabilities)
    {
        var argv = new List<string> { "sleep" };
        if (options.Force) argv.Add("--force");
        AddOperation(argv, options, capabilities);
        return argv.ToArray();
    }

    public static string[] Run(CommandOptions options, AgentCapabilities capabilities)
    {
        Require("action id", options.ActionId);
        var argv = new List<string> { "run", options.ActionId! };
        if (options.Force) argv.Add("--force");
        AddOperation(argv, options, capabilities);
        return argv.ToArray();
    }

    /// The machine-wide switch every agent has had since 2.x.
    public static string[] AutoUpdate(bool on, string? service)
    {
        var argv = new List<string> { "auto-update", on ? "on" : "off" };
        AddService(argv, service);
        return argv.ToArray();
    }

    // MARK: contract 3

    /// One pass over every eligible service, which is what the schedulers run.
    public static string[] Cycle(CommandOptions options, AgentCapabilities capabilities)
    {
        RequireV3("cycle", capabilities);
        var argv = new List<string> { "cycle" };
        if (options.DryRun) argv.Add("--dry-run");
        AddOperation(argv, options with { Detach = false, WhenIdle = false }, capabilities);
        return argv.ToArray();
    }

    /// Ask what became of an operation. Read-only, and the whole reason an operation has an id.
    public static string[] ReadOperation(string id, int? waitSeconds, AgentCapabilities capabilities)
    {
        RequireV3("op", capabilities);
        if (!IsValidOperationId(id)) throw new InvalidArgument("operation id", id);
        var argv = new List<string> { "op", id };
        if (waitSeconds is { } seconds and > 0) argv.AddRange(new[] { "--wait", seconds.ToString() });
        return argv.ToArray();
    }

    public static string[] Cancel(string id, AgentCapabilities capabilities)
    {
        RequireV3("cancel", capabilities);
        if (!IsValidOperationId(id)) throw new InvalidArgument("operation id", id);
        return new[] { "cancel", id };
    }

    public static string[] History(int limit, string? service, string? kind, AgentCapabilities capabilities)
    {
        RequireV3("history", capabilities);
        var argv = new List<string> { "history", "--limit", Math.Clamp(limit, 1, 200).ToString() };
        if (service is not null)
        {
            Require("service id", service);
            argv.AddRange(new[] { "--service", service });
        }
        if (kind is not null)
        {
            Require("operation kind", kind);
            argv.AddRange(new[] { "--kind", kind });
        }
        return argv.ToArray();
    }

    public static string[] Logs(int lines, string? operationId, AgentCapabilities capabilities)
    {
        RequireV3("logs", capabilities);
        var argv = new List<string> { "logs", "--lines", Math.Clamp(lines, 1, 500).ToString() };
        if (operationId is not null)
        {
            if (!IsValidOperationId(operationId)) throw new InvalidArgument("operation id", operationId);
            argv.AddRange(new[] { "--op", operationId });
        }
        return argv.ToArray();
    }

    public static string[] Doctor(string? service, bool deep, AgentCapabilities capabilities)
    {
        RequireV3("doctor", capabilities);
        var argv = new List<string> { "doctor" };
        if (service is not null)
        {
            Require("service id", service);
            argv.AddRange(new[] { "--service", service });
        }
        if (deep) argv.Add("--deep");
        return argv.ToArray();
    }

    public static string[] Bundle(AgentCapabilities capabilities)
    {
        RequireV3("bundle", capabilities);
        return new[] { "bundle" };
    }

    /// What the machine holds: id, revision, lineage and who wrote it. One round trip, and only
    /// spent when a hash differs and descent cannot be decided from status alone.
    public static string[] ReadMeta(AgentCapabilities capabilities)
    {
        RequireV3("config meta", capabilities);
        return new[] { "config", "meta" };
    }

    /// Hands the machine the setup document. The bytes go in on stdin; only the identity travels as
    /// arguments, which is why those are the only two things validated here.
    ///
    /// `--replace` is never added on this app's own initiative. It is the explicit human decision
    /// that one setup replaces another, made in front of a preview of what changes.
    public static string[] WriteConfig(string? controllerId, long? revision, bool replace, AgentCapabilities capabilities)
    {
        var argv = new List<string> { "config", "set" };
        if (capabilities.SupportsSetupLineage && controllerId is not null && revision is not null)
        {
            Require("controller id", controllerId);
            argv.AddRange(new[] { "--controller-id", controllerId, "--revision", revision.Value.ToString() });
        }
        if (replace)
        {
            RequireV3("config set --replace", capabilities);
            argv.Add("--replace");
        }
        return argv.ToArray();
    }

    public static string[] ReadPolicy(string? service, AgentCapabilities capabilities)
    {
        RequireV3("policy", capabilities);
        var argv = new List<string> { "policy" };
        if (service is not null)
        {
            Require("service id", service);
            argv.AddRange(new[] { "--service", service });
        }
        return argv.ToArray();
    }

    /// The patch itself goes in on stdin as one JSON object. Nothing about a window, a pause or a
    /// switch is ever concatenated into a command line.
    public static string[] WritePolicy(string? service, AgentCapabilities capabilities)
    {
        RequireV3("policy set", capabilities);
        var argv = new List<string> { "policy", "set" };
        if (service is not null)
        {
            Require("service id", service);
            argv.AddRange(new[] { "--service", service });
        }
        return argv.ToArray();
    }

    /// Installs a signed agent archive that arrives on standard input. The archive is verified on
    /// this side before it is sent and again by the agent before it is used.
    public static string[] SelfUpdateFromStdin(string? operationId, AgentCapabilities capabilities)
    {
        RequireV3("self-update", capabilities);
        var argv = new List<string> { "self-update", "--stdin" };
        if (operationId is not null)
        {
            if (!IsValidOperationId(operationId)) throw new InvalidArgument("operation id", operationId);
            argv.AddRange(new[] { "--op", operationId });
        }
        return argv.ToArray();
    }

    /// The bootstrap an agent too old to read its own standard input needs: the staged tree is
    /// asked to install itself.
    public static string[] SelfUpdateInstall() => new[] { "self-update", "--install" };

    public static string[] SelfUpdateRollback(AgentCapabilities capabilities)
    {
        RequireV3("self-update", capabilities);
        return new[] { "self-update", "--rollback" };
    }

    private static void AddService(List<string> argv, string? service)
    {
        if (service is null) return;
        Require("service id", service);
        argv.AddRange(new[] { "--service", service });
    }

    /// The operation flags, and only ever against an agent that has them.
    private static void AddOperation(List<string> argv, CommandOptions options, AgentCapabilities capabilities)
    {
        if (!capabilities.SpeaksV3) return;

        if (options.OperationId is { } id)
        {
            if (!IsValidOperationId(id)) throw new InvalidArgument("operation id", id);
            argv.AddRange(new[] { "--op", id });
        }

        if (options.WhenIdle)
        {
            argv.Add("--when-idle");
            if (options.Expires is { } expires)
            {
                if (!IsValidDuration(expires)) throw new InvalidArgument("expiry", expires);
                argv.AddRange(new[] { "--expires", expires });
            }
            // A queued request is not also a detached one: the agent answers immediately either way.
            return;
        }

        if (options.Detach) argv.Add("--detach");
    }
}
