namespace LegionControl.Desktop.Contract;

// What the agent says, as this client reads it.
//
// Every field is optional and nothing here throws. The far side is a script that is upgraded by
// hand on machines this app cannot see, so a key that has not arrived yet and a key that was
// renamed both have to cost one blank row and never a blank window.
//
// Where two agent vintages spell the same idea differently, both spellings are read. That is not
// tidiness lost: it is the difference between a client that keeps working through a staggered
// upgrade and one that stops the moment the machine in front of it is a release behind.

/// The envelope every reply carries. Read first, and on its own, so that even a reply this build
/// cannot otherwise make sense of still says which agent answered and whether it was happy.
public sealed record AgentEnvelope
{
    public bool? Ok { get; init; }
    public int? ContractVersion { get; init; }
    public string? AgentVersion { get; init; }
    public SystemInfo? System { get; init; }
    public string? ReasonCode { get; init; }
    public string? Message { get; init; }
    public string? Error { get; init; }
    public IReadOnlyList<string> Notes { get; init; } = Array.Empty<string>();
    public IReadOnlyList<string> Capabilities { get; init; } = Array.Empty<string>();

    public static AgentEnvelope From(Value json) => new()
    {
        Ok = json["ok"].AsBool(),
        ContractVersion = json["contract", "contractVersion"].AsInt(),
        AgentVersion = json["agentVersion", "version"].AsText(),
        System = SystemInfo.From(json["system"]),
        ReasonCode = json["reasonCode", "reason"].AsText(),
        Message = json["message"].AsText(),
        Error = json["error"].AsText(),
        Notes = json["notes"].AsStringList(),
        Capabilities = json["capabilities"].AsStringList(),
    };

    /// The one sentence worth putting on the screen, in the order of how specific it is.
    public string? Sentence(string? service = null) =>
        Message ?? Error ?? Contract.ReasonCode.Describe(ReasonCode, service);
}

public sealed record SystemInfo(string? Id, string? Name)
{
    public static SystemInfo? From(Value json)
    {
        if (!json.IsObject) return null;
        var id = json["id"].AsText();
        var name = json["name"].AsText();
        return id is null && name is null ? null : new SystemInfo(id, name);
    }
}

/// Whether a service is in the middle of something a restart would destroy.
///
/// [Monitored] is the field that matters most and the one that arrived last. A service with no
/// busy configuration at all is not idle; it is unwatched, and an unwatched service blocks
/// disruptive work until its configuration says, in as many words, that it is never busy.
public sealed record BusyState
{
    public bool Busy { get; init; }
    public bool Unknown { get; init; }
    /// Null on an agent that predates the idea. Treated as unmonitored only when the agent says so.
    public bool? Monitored { get; init; }
    public string? Reason { get; init; }
    /// t3-sqlite | command | http | none | unmonitored | probe-error | timed-out
    public string? Evidence { get; init; }
    public DateTimeOffset? CheckedAt { get; init; }
    public long? ElapsedMs { get; init; }
    public string? Error { get; init; }
    public int? RunningTurns { get; init; }
    public int? PendingTurns { get; init; }
    public int? PendingApprovals { get; init; }
    public int? StaleTurns { get; init; }
    public int? StaleApprovals { get; init; }
    public IReadOnlyList<BusyThread> Threads { get; init; } = Array.Empty<BusyThread>();
    public int? ThreadsTruncated { get; init; }
    /// Only on the machine-wide answer: how many services are watched and how many are not.
    public int? MonitoredServices { get; init; }
    public int? UnmonitoredServices { get; init; }

    public static readonly BusyState UnknownState = new() { Unknown = true, Reason = "busy state unknown" };

    public static BusyState? From(Value json)
    {
        if (!json.IsObject) return null;
        return new BusyState
        {
            Busy = json["busy"].AsBool() ?? false,
            Unknown = json["unknown"].AsBool() ?? false,
            Monitored = json["monitored"].AsBool(),
            Reason = json["reason"].AsText(),
            Evidence = json["evidence"].AsText(),
            CheckedAt = json["checkedAt"].AsInstant(),
            ElapsedMs = json["elapsedMs"].AsLong(),
            Error = json["error"].AsText(),
            RunningTurns = json["runningTurns"].AsInt(),
            PendingTurns = json["pendingTurns"].AsInt(),
            PendingApprovals = json["pendingApprovals"].AsInt(),
            StaleTurns = json["staleTurns"].AsInt(),
            StaleApprovals = json["staleApprovals"].AsInt(),
            Threads = json["threads"].Map(BusyThread.From),
            ThreadsTruncated = json["threadsTruncated"].AsInt(),
            MonitoredServices = json["monitoredServices"].AsInt(),
            UnmonitoredServices = json["unmonitoredServices"].AsInt(),
        };
    }

    /// Whether the busy state is unwatched: the agent said so, or it reported an evidence of
    /// "unmonitored". Missing configuration is unmonitored, and unmonitored is not idle.
    public bool IsUnmonitored => Monitored == false || Evidence == "unmonitored";

    /// Whether a disruptive change may go ahead without the user being asked to force it.
    ///
    /// Three ways to say no and one to say yes. Busy is no; unknown is no, because a probe that
    /// could not read the machine has not said the machine is free; unmonitored is no, because
    /// nothing is watching. Only an explicit, monitored, idle reading is a yes.
    public bool IsSafeToDisturb => !Busy && !Unknown && !IsUnmonitored;

    public string Summary
    {
        get
        {
            if (!string.IsNullOrWhiteSpace(Reason)) return Reason!;
            if (Unknown) return "busy state unknown";
            if (IsUnmonitored) return "not monitored";
            return Busy ? "working" : "idle";
        }
    }
}

public sealed record BusyThread(
    string? ThreadId,
    string? TurnId,
    string? Title,
    string? State,
    string? At,
    bool? Stale,
    /// Whether this thread is one of the reasons the gate is closed.
    bool? Blocking,
    /// running | pending | pending-approval | stale.
    string? Disposition)
{
    public static BusyThread From(Value json) => new(
        json["threadId"].AsText(),
        json["turnId"].AsText(),
        json["title"].AsText(),
        json["state"].AsText(),
        json["at"].AsText(),
        json["stale"].AsBool(),
        json["blocking"].AsBool(),
        json["disposition"].AsText());
}

/// Whether the process behind a service is up, told apart from whether the service answers.
public sealed record ProcessState(bool? Running, string? State, DateTimeOffset? StartedAt, string? Error)
{
    public static ProcessState? From(Value json)
    {
        if (!json.IsObject) return null;
        return new ProcessState(
            json["running"].AsBool(),
            json["state"].AsText(),
            json["startedAt"].AsInstant(),
            json["error"].AsText());
    }
}

/// Whether the service answered its own health check. A separate thing from the process being up.
public sealed record HealthState(bool? Ok, int? Status, DateTimeOffset? CheckedAt, long? ElapsedMs, string? Error)
{
    public static HealthState? From(Value json)
    {
        if (!json.IsObject) return null;
        return new HealthState(
            json["ok"].AsBool(),
            json["status"].AsInt(),
            json["checkedAt"].AsInstant(),
            json["elapsedMs"].AsLong(),
            json["error"].AsText());
    }
}

/// Reachability from outside the machine. Only ever filled in by a deep doctor run; null here
/// means "not asked", which is deliberately not the same as "no".
public sealed record EndpointState(bool? Configured, bool? Reachable, string? Error)
{
    public static EndpointState? From(Value json)
    {
        if (!json.IsObject) return null;
        return new EndpointState(json["configured"].AsBool(), json["reachable"].AsBool(), json["error"].AsText());
    }
}

public sealed record RelayState(bool? Configured, bool? Running)
{
    public static RelayState? From(Value json)
    {
        if (!json.IsObject) return null;
        return new RelayState(json["configured"].AsBool(), json["running"].AsBool());
    }
}

public sealed record LastUpdate(string? At, string? From_, string? To, string? Result, string? Message)
{
    public static LastUpdate? From(Value json)
    {
        if (!json.IsObject) return null;
        return new LastUpdate(
            json["at"].AsText(),
            json["from"].AsText(),
            json["to"].AsText(),
            json["result", "action"].AsText(),
            json["message"].AsText());
    }
}

/// One maintenance window, in the machine's own local time.
public sealed record MaintenanceWindow(IReadOnlyList<string> Days, string? From_, string? To)
{
    public static MaintenanceWindow From(Value json) => new(
        json["days"].AsStringList(),
        json["from", "start", "windowStart"].AsText(),
        json["to", "end", "windowEnd"].AsText());

    /// True when the window runs past midnight, which is the case the editor has to allow and the
    /// validator has to stop refusing.
    public bool IsOvernight
    {
        get
        {
            var from = PolicyTime.Parse(From_);
            var to = PolicyTime.Parse(To);
            return from is not null && to is not null && to <= from;
        }
    }

    public string Describe()
    {
        var hours = From_ is null || To is null ? "any time" : $"{From_}–{To}";
        var days = Days.Count == 0 || Days.Count == 7 ? "every day" : string.Join(", ", Days);
        return $"{hours}, {days}";
    }
}

/// Minutes past midnight, or null when the text is not an HH:MM.
public static class PolicyTime
{
    public static int? Parse(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return null;
        var parts = text.Split(':');
        if (parts.Length != 2) return null;
        if (!int.TryParse(parts[0], out var hours) || !int.TryParse(parts[1], out var minutes)) return null;
        if (hours is < 0 or > 23 || minutes is < 0 or > 59) return null;
        return hours * 60 + minutes;
    }

    public static readonly string[] Days = { "mon", "tue", "wed", "thu", "fri", "sat", "sun" };

    public static bool IsDay(string value) => Array.IndexOf(Days, value.Trim().ToLowerInvariant()) >= 0;
}

/// The update policy as it applies to one thing: the system as a whole, or one service.
///
/// Three separate ideas that used to be one boolean: whether the scheduler may act on its own,
/// whether it has been told to leave this alone for a while, and the hours it may work in. A
/// person asking for an update is none of the three, which is the entire point of separating them.
public sealed record UpdatePolicy
{
    /// Null on a service means "inherit the system's answer".
    public bool? Automatic { get; init; }
    public DateTimeOffset? PauseUntil { get; init; }
    /// Null on a service means "inherit". An empty list means "any time".
    public IReadOnlyList<MaintenanceWindow>? MaintenanceWindows { get; init; }
    public bool? Inherited { get; init; }
    public bool? InWindowNow { get; init; }
    public DateTimeOffset? NextWindow { get; init; }
    public bool? EligibleNow { get; init; }
    public string? DeferredReason { get; init; }
    public OperationSummary? LastCycle { get; init; }

    public static UpdatePolicy? From(Value json)
    {
        if (!json.IsObject) return null;
        var windows = json["maintenanceWindows", "windows"];
        return new UpdatePolicy
        {
            Automatic = json["automatic", "autoUpdate", "auto"].AsBool(),
            PauseUntil = json["pauseUntil", "pausedUntil"].AsInstant(),
            MaintenanceWindows = windows.IsArray ? windows.Map(MaintenanceWindow.From) : LegacyWindow(json),
            Inherited = json["inherited"].AsBool(),
            InWindowNow = json["inWindowNow"].AsBool(),
            NextWindow = json["nextWindow"].AsInstant(),
            EligibleNow = json["eligibleNow"].AsBool(),
            DeferredReason = json["deferredReason"].AsText(),
            LastCycle = OperationSummary.From(json["lastCycle"]),
        };
    }

    /// The shape the first policy-aware agent used: one start and one end, no days.
    private static IReadOnlyList<MaintenanceWindow>? LegacyWindow(Value json)
    {
        var start = json["windowStart"].AsText();
        var end = json["windowEnd"].AsText();
        if (start is null || end is null) return null;
        return new[] { new MaintenanceWindow(PolicyTime.Days, start, end) };
    }

    public bool IsPaused(DateTimeOffset now) => PauseUntil is { } until && until > now;

    /// What the effective policy is for a service, given the system's. Only the keys the service
    /// actually sets are its own; the rest are the system's answer, which is what inheritance
    /// means and what the editor has to be able to show.
    public UpdatePolicy InheritedFrom(UpdatePolicy? system) => new()
    {
        Automatic = Automatic ?? system?.Automatic,
        PauseUntil = PauseUntil ?? system?.PauseUntil,
        MaintenanceWindows = MaintenanceWindows ?? system?.MaintenanceWindows,
        Inherited = Automatic is null,
        InWindowNow = InWindowNow ?? system?.InWindowNow,
        NextWindow = NextWindow ?? system?.NextWindow,
        EligibleNow = EligibleNow,
        DeferredReason = DeferredReason,
        LastCycle = LastCycle ?? system?.LastCycle,
    };
}

public sealed record ServiceStatus
{
    public string? Id { get; init; }
    public string? Name { get; init; }
    public string? Kind { get; init; }
    public string? Installed { get; init; }
    public string? Latest { get; init; }
    public string? Channel { get; init; }
    public bool? UpToDate { get; init; }
    public bool? Running { get; init; }
    public bool? Healthy { get; init; }
    public int? Port { get; init; }
    public string? Staged { get; init; }
    public string? PendingVersion { get; init; }
    public string? AppPath { get; init; }
    public BusyState? Busy { get; init; }
    public ProcessState? Process { get; init; }
    public HealthState? Health { get; init; }
    public EndpointState? Endpoint { get; init; }
    public RelayState? Relay { get; init; }
    public bool? PendingRestart { get; init; }
    public bool? Drain { get; init; }
    public LastUpdate? LastUpdate { get; init; }
    public UpdatePolicy? Updates { get; init; }
    public OperationSummary? LastOperation { get; init; }
    public bool? CanUpdate { get; init; }
    public bool? CanRestart { get; init; }
    public long? ProbeMillis { get; init; }
    public bool? ProbeTimedOut { get; init; }
    public IReadOnlyList<string> Notes { get; init; } = Array.Empty<string>();

    public string DisplayName => Name ?? Id ?? "Service";

    public static ServiceStatus From(Value json) => new()
    {
        Id = json["id"].AsText(),
        Name = json["name"].AsText(),
        Kind = json["kind"].AsText(),
        Installed = json["installed", "installedVersion"].AsText(),
        Latest = json["latest", "latestVersion"].AsText(),
        Channel = json["channel"].AsText(),
        UpToDate = json["upToDate"].AsBool(),
        Running = json["running"].AsBool(),
        Healthy = json["healthy"].AsBool(),
        Port = json["port"].AsInt(),
        Staged = json["staged"].AsText(),
        PendingVersion = json["pendingVersion"].AsText(),
        AppPath = json["appPath"].AsText(),
        Busy = BusyState.From(json["busy"]),
        Process = ProcessState.From(json["process"]),
        Health = HealthState.From(json["health"]),
        Endpoint = EndpointState.From(json["endpoint"]),
        Relay = RelayState.From(json["relay"]),
        PendingRestart = json["pendingRestart"].AsBool(),
        Drain = json["drain"].AsBool(),
        LastUpdate = Contract.LastUpdate.From(json["lastUpdate"]),
        Updates = UpdatePolicy.From(json["updates", "policy"]),
        LastOperation = OperationSummary.From(json["lastOperation"]),
        CanUpdate = json["canUpdate"].AsBool(),
        CanRestart = json["canRestart"].AsBool(),
        ProbeMillis = json["probeMillis", "elapsedMs"].AsLong(),
        ProbeTimedOut = json["probeTimedOut"].AsBool(),
        Notes = json["notes"].AsStringList(),
    };

    /// Whether the process is up, from whichever of the two shapes the agent used.
    public bool? IsRunning => Process?.Running ?? Running;

    /// Whether there is a newer version to install, or null when nobody could find out. Null is
    /// never rendered as "up to date": a lookup that failed has not said there is nothing newer.
    public bool? HasUpdate
    {
        get
        {
            if (Latest is null) return null;
            if (Installed is null) return null;
            if (UpToDate is { } known) return !known;
            return !string.Equals(Latest, Installed, StringComparison.Ordinal);
        }
    }
}

public sealed record BootTarget(string? Id, string? Name, bool? Current)
{
    public string DisplayName => Name ?? Id ?? "system";

    public static BootTarget From(Value json) => new(
        json["id"].AsText(),
        json["name"].AsText(),
        json["current", "isCurrent"].AsBool());
}

public sealed record AgentAction(string? Id, string? Name, string? Confirm, bool? BusyGated, string? Kind)
{
    public string DisplayName => Name ?? Id ?? "Run";

    /// A wake action sends a magic packet from the machine it runs on. It is what makes one machine
    /// able to wake another on a network this device cannot reach.
    public bool IsWake => string.Equals(Kind, "wol", StringComparison.OrdinalIgnoreCase);

    public static AgentAction From(Value json) => new(
        json["id"].AsText(),
        json["name"].AsText(),
        json["confirm"].AsText(),
        json["busyGated"].AsBool(),
        json["kind"].AsText());
}

/// What the machine says about the setup document it is holding.
///
/// The hash says which bytes. The id and the revision say which setup and how far along, and the
/// lineage - only ever present on the fuller `config meta` reply, never in status - says what this
/// document descends from, which is the only thing that can tell "behind" from "different".
public sealed record ControllerMark
{
    public string? Hash { get; init; }
    public string? Id { get; init; }
    public long? Revision { get; init; }
    public DateTimeOffset? UpdatedAt { get; init; }
    /// mac | desktop | phone | cli | legacy.
    public string? Source { get; init; }
    /// The human name of the device that wrote it. Display only.
    public string? Device { get; init; }
    public IReadOnlyList<string> Lineage { get; init; } = Array.Empty<string>();
    public long? Bytes { get; init; }

    public static ControllerMark? From(Value json)
    {
        if (!json.IsObject) return null;
        return new ControllerMark
        {
            Hash = json["hash"].AsText(),
            Id = json["id", "controllerId"].AsText(),
            Revision = json["revision"].AsLong(),
            UpdatedAt = json["updatedAt"].AsInstant(),
            Source = json["source"].AsText(),
            Device = json["device"].AsText(),
            Lineage = json["lineage"].AsStringList(),
            Bytes = json["bytes"].AsLong(),
        };
    }

    public long RevisionNumber => Revision ?? 0;

    /// Who wrote it, in one phrase, for the row that says where a document came from.
    public string Provenance => (Device, Source) switch
    {
        ({ Length: > 0 } device, { Length: > 0 } source) => $"{device} ({source})",
        ({ Length: > 0 } device, _) => device,
        (_, { Length: > 0 } source) => source,
        _ => "an unnamed device",
    };
}

/// One thing wrong with the agent's own configuration file.
public sealed record ConfigProblem(string? Level, string? Path, string? Message, string? Fix)
{
    public static ConfigProblem From(Value json) => json.IsObject
        ? new ConfigProblem(
            json["level"].AsText(),
            json["path"].AsText(),
            json["message"].AsText(),
            json["fix"].AsText())
        : new ConfigProblem(null, null, json.AsText(), null);

    /// One line, in the order somebody reading it needs: where, what, and what to do.
    public string Sentence => string.Join(" ", new[]
    {
        Path is null ? null : $"{Path}:",
        Message,
        Fix is null ? null : $"Fix: {Fix}",
    }.Where(part => part is not null));
}

/// How the agent's own configuration file read.
public sealed record ConfigHealth(bool? Ok, string? Source, IReadOnlyList<ConfigProblem> Problems)
{
    public static ConfigHealth? From(Value json)
    {
        if (!json.IsObject) return null;
        return new ConfigHealth(json["ok"].AsBool(), json["source"].AsText(), json["problems"].Map(ConfigProblem.From));
    }

    /// The agent refuses every mutation while this is false, so the UI has to say so before the
    /// user presses something that will be turned down.
    public bool BlocksMutations => Ok == false;
}

public sealed record StatusTiming(long? BudgetMs, long? ElapsedMs, bool Partial)
{
    public static StatusTiming? From(Value json)
    {
        if (!json.IsObject) return null;
        return new StatusTiming(json["budgetMs"].AsLong(), json["elapsedMs"].AsLong(), json["partial"].AsBool() ?? false);
    }
}

public sealed record AgentInfo(string? Version, int? ContractVersion, string? Base, string? Node, bool? RestrictedSession)
{
    public static AgentInfo? From(Value json)
    {
        if (!json.IsObject) return null;
        return new AgentInfo(
            json["version"].AsText(),
            json["contract"].AsInt(),
            json["base"].AsText(),
            json["node"].AsText(),
            json["restrictedSession"].AsBool());
    }
}

/// Optional readings about the machine itself. Rendered when sent and never asked for on their
/// own: this is a control app explaining an operational problem, not a dashboard.
public sealed record MachineMetrics
{
    public IReadOnlyList<TelemetryReading> Readings { get; init; } = Array.Empty<TelemetryReading>();
    public double? CpuPercent { get; init; }
    public long? MemoryUsedBytes { get; init; }
    public long? MemoryTotalBytes { get; init; }
    public long? DiskUsedBytes { get; init; }
    public long? DiskTotalBytes { get; init; }
    public double? TemperatureCelsius { get; init; }
    public int? BatteryPercent { get; init; }
    public bool? OnBattery { get; init; }
    public long? UptimeSeconds { get; init; }

    public static MachineMetrics? From(Value json)
    {
        if (json.IsArray) return new MachineMetrics { Readings = json.Map(TelemetryReading.From) };
        if (!json.IsObject) return null;
        return new MachineMetrics
        {
            CpuPercent = json["cpuPercent"].AsDouble(),
            MemoryUsedBytes = json["memoryUsedBytes"].AsLong(),
            MemoryTotalBytes = json["memoryTotalBytes"].AsLong(),
            DiskUsedBytes = json["diskUsedBytes"].AsLong(),
            DiskTotalBytes = json["diskTotalBytes"].AsLong(),
            TemperatureCelsius = json["temperatureCelsius"].AsDouble(),
            BatteryPercent = json["batteryPercent"].AsInt(),
            OnBattery = json["onBattery"].AsBool(),
            UptimeSeconds = json["uptimeSeconds"].AsLong(),
        };
    }
}

public sealed record TelemetryReading(string? Id, string Name, double? Value, string Unit,
    DateTimeOffset? CheckedAt, string? Error)
{
    public static TelemetryReading From(Value json) => new(json["id"].AsText(),
        json["name"].AsText() ?? json["id"].AsText() ?? "Reading", json["value"].AsDouble(),
        json["unit"].AsText() ?? "", json["checkedAt"].AsInstant(), json["error"].AsText());
}

public sealed record AgentStatus
{
    public AgentEnvelope Envelope { get; init; } = new();
    public string? Hostname { get; init; }
    public string? OsName { get; init; }
    public AgentInfo? Agent { get; init; }
    public ConfigHealth? Config { get; init; }
    public StatusTiming? Timing { get; init; }
    public IReadOnlyList<ServiceStatus> Services { get; init; } = Array.Empty<ServiceStatus>();
    public IReadOnlyList<BootTarget> BootTargets { get; init; } = Array.Empty<BootTarget>();
    public IReadOnlyList<AgentAction> Actions { get; init; } = Array.Empty<AgentAction>();
    public BusyState? Busy { get; init; }
    public ControllerMark? Controller { get; init; }
    public UpdatePolicy? Updates { get; init; }
    public bool? AutoUpdate { get; init; }
    public MachineMetrics? Metrics { get; init; }
    public OperationSet Operations { get; init; } = OperationSet.Empty;
    public DateTimeOffset? TakenAt { get; init; }
    /// Whether the snapshot ran out of budget. Distinct from a snapshot that simply has nothing
    /// to say: a partial status must never be drawn as a complete one.
    public bool Partial { get; init; }
    public IReadOnlyList<string> Incomplete { get; init; } = Array.Empty<string>();
    /// Whether the reply named a services array at all. Decides whether `--service` may be sent.
    public bool ReportsServices { get; init; }
    public bool ReportsPolicies { get; init; }

    public string? SystemId => Envelope.System?.Id ?? OsName;
    public string? SystemName => Envelope.System?.Name;
    public int ContractVersion => Envelope.ContractVersion ?? Agent?.ContractVersion ?? 0;
    public string? AgentVersion => Envelope.AgentVersion ?? Agent?.Version;

    public static AgentStatus From(Value json)
    {
        var services = json["services"];
        var operations = json["operations"];
        var timing = StatusTiming.From(json["timing"]);
        var systemPolicy = UpdatePolicy.From(json["updates"]);
        var decoded = services.IsArray ? services.Map(ServiceStatus.From) : LegacyServices(json);

        // The policy list some agents send beside the services rather than inside them. Folding it
        // in here keeps every reader above this file working against one shape.
        var policies = json["policies"];
        if (policies.IsArray)
        {
            var byService = new Dictionary<string, UpdatePolicy>(StringComparer.Ordinal);
            foreach (var entry in policies.AsArray())
            {
                var id = entry["service", "id"].AsText();
                var policy = UpdatePolicy.From(entry);
                if (id is not null && policy is not null) byService[id] = policy;
            }
            decoded = decoded
                .Select(service => service.Id is { } id && byService.TryGetValue(id, out var policy) && service.Updates is null
                    ? service with { Updates = policy }
                    : service)
                .ToList();
        }

        return new AgentStatus
        {
            Envelope = AgentEnvelope.From(json),
            Hostname = json["hostname"].AsText(),
            OsName = json["os"].AsText(),
            Agent = AgentInfo.From(json["agent"]),
            Config = ConfigHealth.From(json["config"]),
            Timing = timing,
            Services = decoded,
            BootTargets = json["bootTargets"].Map(BootTarget.From),
            Actions = json["actions"].Map(AgentAction.From),
            Busy = BusyState.From(json["busy"]),
            Controller = ControllerMark.From(json["controller"]),
            Updates = systemPolicy,
            AutoUpdate = json["autoUpdate"].AsBool() ?? systemPolicy?.Automatic,
            Metrics = MachineMetrics.From(json["metrics"]),
            Operations = OperationSet.From(operations),
            TakenAt = json["takenAt"].AsInstant(),
            Partial = timing?.Partial ?? json["partial"].AsBool() ?? false,
            Incomplete = json["incomplete"].AsStringList(),
            ReportsServices = services.IsArray,
            ReportsPolicies = policies.IsArray || decoded.Any(service => service.Updates is not null),
        };
    }

    /// The first agent described exactly one service and spread it across the top level of the
    /// reply. Folding that back into one service here is what lets everything above this file be
    /// written once, against the shape the contract now has.
    private static IReadOnlyList<ServiceStatus> LegacyServices(Value json)
    {
        var t3 = json["t3"];
        if (!t3.IsObject) return Array.Empty<ServiceStatus>();
        return new[]
        {
            new ServiceStatus
            {
                Id = "t3",
                Name = "T3 Code",
                Kind = "npm",
                Installed = t3["installed"].AsText(),
                Latest = t3["nightly"].AsText(),
                Channel = "nightly",
                UpToDate = t3["upToDate"].AsBool(),
                Running = t3["serverRunning"].AsBool(),
                Healthy = t3["healthy"].AsBool(),
                Port = t3["port"].AsInt(),
                Busy = BusyState.From(json["busy"]),
                Relay = RelayState.From(json["connect"]),
                PendingRestart = json["pendingRestart"].AsBool(),
                LastUpdate = Contract.LastUpdate.From(json["lastUpdate"]),
            },
        };
    }

    public ServiceStatus? Service(string id) => Services.FirstOrDefault(service => service.Id == id);
}

/// The operations a machine is running, holding and has just finished.
public sealed record OperationSet(
    IReadOnlyList<OperationSummary> Running,
    IReadOnlyList<OperationSummary> Queued,
    IReadOnlyList<OperationSummary> Recent)
{
    public static readonly OperationSet Empty = new(
        Array.Empty<OperationSummary>(), Array.Empty<OperationSummary>(), Array.Empty<OperationSummary>());

    public static OperationSet From(Value json)
    {
        if (!json.Exists) return Empty;

        // Some agents send one flat list rather than three, so sort it here instead of asking
        // every screen above to know about both shapes.
        if (json.IsArray)
        {
            var all = Read(json);
            return new OperationSet(
                all.Where(operation => operation.State == OperationState.Running).ToList(),
                all.Where(operation => operation.State == OperationState.Queued).ToList(),
                all.Where(operation => operation.State == OperationState.Finished).ToList());
        }

        return new OperationSet(
            Read(json["running"]),
            Read(json["queued"]),
            Read(json["recent", "finished", "history"]));
    }

    /// A record that carries nothing at all is dropped rather than drawn as a blank row.
    private static IReadOnlyList<OperationSummary> Read(Value json) => json
        .Map(OperationSummary.From)
        .Where(summary => summary is not null)
        .Select(summary => summary!)
        .ToList();

    public IEnumerable<OperationSummary> All => Running.Concat(Queued).Concat(Recent);
}

public enum OperationState
{
    /// The agent has not said, which is not the same as any of the others.
    Unknown,
    Queued,
    Running,
    Finished,
}

/// What became of an operation. Deliberately not a boolean: "we do not know" is a state the whole
/// design exists to be able to say, and it must never collapse into either of the other two.
public enum OperationOutcome
{
    /// Still going, or waiting.
    Pending,
    Succeeded,
    /// The agent said it failed.
    Failed,
    /// The agent deliberately did nothing and said why.
    Deferred,
    Queued,
    Cancelled,
    Expired,
    /// Cut off mid-flight. Whatever it left behind is on the machine.
    Interrupted,
    /// Nothing needed doing.
    Noop,
    /// Refused because something else holds the machine.
    Conflict,
    /// The record is finished and does not say how it went, or the reply never arrived. Never
    /// rendered as either of the two that matter.
    Unresolved,
}

public sealed record OperationProgress(int? Step, int? Of, string? Note)
{
    public static OperationProgress? From(Value json)
    {
        if (!json.IsObject) return null;
        return new OperationProgress(json["step"].AsInt(), json["of"].AsInt(), json["note"].AsText());
    }

    public string Describe()
    {
        var counted = Step is { } step && Of is { } of ? $"step {step} of {of}" : null;
        return string.Join(", ", new[] { counted, Note }.Where(part => !string.IsNullOrWhiteSpace(part))!);
    }
}

public sealed record OperationLogLine(string? At, string? Line)
{
    public static OperationLogLine From(Value json) =>
        json.IsObject
            ? new OperationLogLine(json["at"].AsText(), json["line", "message", "text"].AsText())
            : new OperationLogLine(null, json.AsText());
}

/// One phase, in the shape the agents that record a list of them use.
public sealed record OperationPhaseRecord(string? Name, string? At, bool? Ok, string? Message)
{
    public static OperationPhaseRecord From(Value json) => new(
        json["name", "phase"].AsText(),
        json["at"].AsText(),
        json["ok"].AsBool(),
        json["message", "note"].AsText());
}

/// What a finished operation actually did.
public sealed record OperationResult
{
    public bool? Ok { get; init; }
    public string? Action { get; init; }
    public string? ReasonCode { get; init; }
    public string? Message { get; init; }
    public string? From_ { get; init; }
    public string? To { get; init; }
    public int? ExitCode { get; init; }
    public string? Output { get; init; }
    /// Whether the agent checked that what it set out to do actually happened. False is not a
    /// failure; it is a success nobody confirmed, and it is shown as exactly that.
    public bool? Verified { get; init; }

    public static OperationResult? From(Value json)
    {
        if (!json.IsObject) return null;
        return new OperationResult
        {
            Ok = json["ok"].AsBool(),
            Action = json["action"].AsText(),
            ReasonCode = json["reasonCode", "reason"].AsText(),
            Message = json["message"].AsText(),
            From_ = json["from"].AsText(),
            To = json["to"].AsText(),
            ExitCode = json["exitCode"].AsInt(),
            Output = json["output"].AsText(),
            Verified = json["verified"].AsBool(),
        };
    }
}

/// One durable operation, as the agent recorded it.
///
/// This is the record every recovery story rests on. A client that lost its link in the middle of
/// an update comes back, asks for the same id, and is told what happened rather than guessing or,
/// far worse, sending the command again.
public sealed record OperationRecord
{
    public string? Id { get; init; }
    public string? Kind { get; init; }
    public string? Service { get; init; }
    public string? Target { get; init; }
    public string? ActionId { get; init; }
    /// scheduled | manual | force | queued
    public string? Mode { get; init; }
    public string? Initiator { get; init; }
    public OperationState State { get; init; } = OperationState.Unknown;
    /// The agent's own word for the state, kept for the rows that show it as it stands.
    public string? RawState { get; init; }
    public string? Phase { get; init; }
    public IReadOnlyList<OperationPhaseRecord> Phases { get; init; } = Array.Empty<OperationPhaseRecord>();
    public OperationProgress? Progress { get; init; }
    public DateTimeOffset? RequestedAt { get; init; }
    public DateTimeOffset? StartedAt { get; init; }
    public DateTimeOffset? UpdatedAt { get; init; }
    public DateTimeOffset? FinishedAt { get; init; }
    public DateTimeOffset? ExpiresAt { get; init; }
    public long? Pid { get; init; }
    public bool? Detached { get; init; }
    public string? From_ { get; init; }
    public string? To { get; init; }
    public OperationResult? Result { get; init; }
    public IReadOnlyList<OperationLogLine> Log { get; init; } = Array.Empty<OperationLogLine>();
    /// One entry per service a cycle walked, so a scheduled pass can be read service by service
    /// rather than as one word about the whole machine.
    public IReadOnlyList<OperationChild> Children { get; init; } = Array.Empty<OperationChild>();
    public string? AgentVersion { get; init; }
    public string? SystemId { get; init; }
    /// Set when this reply is the record of a request that had already been made with this id.
    public bool Replayed { get; init; }
    /// Set when a queued request of the same shape was displaced by this one.
    public string? Replaced { get; init; }
    public OperationConflict? Conflict { get; init; }

    public static OperationRecord? From(Value json)
    {
        if (!json.IsObject) return null;
        var phases = json["phases"].Map(OperationPhaseRecord.From);
        var rawState = json["state", "status"].AsText();
        // History/status summaries flatten result fields onto the operation itself.
        var result = OperationResult.From(json["result"])
                     ?? (json["action"].Exists || json["ok"].Exists ? OperationResult.From(json) : null);
        var action = result?.Action ?? json["action"].AsText();
        return new OperationRecord
        {
            Id = json["id", "operationId", "opId"].AsText(),
            Kind = json["kind"].AsText(),
            Service = json["service"].AsText(),
            Target = json["target"].AsText(),
            ActionId = json["actionId", "action"].AsText(),
            Mode = json["mode"].AsText(),
            Initiator = ReadInitiator(json["initiator"]),
            State = ReadState(rawState, action, json["finishedAt"].AsInstant()),
            RawState = rawState,
            Phase = json["phase"].AsText() ?? phases.LastOrDefault()?.Name,
            Phases = phases,
            Progress = OperationProgress.From(json["progress"]),
            RequestedAt = json["requestedAt", "createdAt"].AsInstant(),
            StartedAt = json["startedAt"].AsInstant(),
            UpdatedAt = json["updatedAt"].AsInstant(),
            FinishedAt = json["finishedAt"].AsInstant(),
            ExpiresAt = json["expiresAt"].AsInstant(),
            Pid = json["pid"].AsLong(),
            Detached = json["detached"].AsBool(),
            From_ = json["from"].AsText() ?? result?.From_,
            To = json["to"].AsText() ?? result?.To,
            Result = result,
            Log = json["log"].Map(OperationLogLine.From),
            Children = json["children"].Map(OperationChild.From),
            AgentVersion = json["agentVersion"].AsText(),
            SystemId = json["systemId"].AsText(),
            Replayed = json["replayed"].AsBool() ?? false,
            Replaced = json["replaced"].AsText(),
            Conflict = OperationConflict.From(json["conflict"]),
        };
    }

    private static string? ReadInitiator(Value json)
    {
        if (!json.Exists) return null;
        if (!json.IsObject) return json.AsText();
        var client = json["client"].AsText();
        var device = json["device"].AsText();
        return string.Join(" ", new[] { client, device }.Where(part => part is not null)) is { Length: > 0 } joined
            ? joined
            : null;
    }

    /// Reads the state out of either vocabulary.
    ///
    /// One agent says queued, running, finished and puts the outcome in `result.action`. Another
    /// says succeeded, failed, deferred, cancelled, expired, interrupted or noop directly. Both
    /// are read, and a word from neither leaves the state unknown rather than guessed at.
    internal static OperationState ReadState(string? raw, string? action, DateTimeOffset? finishedAt)
    {
        switch (raw?.Trim().ToLowerInvariant())
        {
            case "queued":
                return OperationState.Queued;
            case "running":
            case "accepted":
            case "started":
                return OperationState.Running;
            case "finished":
            case "done":
            case "succeeded":
            case "success":
            case "failed":
            case "deferred":
            case "cancelled":
            case "canceled":
            case "expired":
            case "interrupted":
            case "noop":
            case "conflict":
                return OperationState.Finished;
        }
        if (finishedAt is not null) return OperationState.Finished;
        if (action is not null && Terminal.Contains(action)) return OperationState.Finished;
        return OperationState.Unknown;
    }

    private static readonly HashSet<string> Terminal = new(StringComparer.OrdinalIgnoreCase)
    {
        "updated", "current", "noop", "deferred", "failed", "rolled-back", "restarted", "rebooted",
        "slept", "ran", "cancelled", "canceled", "expired", "interrupted", "conflict", "cycled",
        "skipped", "armed", "already-on-target",
    };

    public bool IsFinished => State == OperationState.Finished;

    /// What became of it. The rule that matters: a finished record whose outcome cannot be read is
    /// [OperationOutcome.Unresolved], never success. Nothing in this client turns an absent
    /// postcondition into a good result, whatever was forced.
    public OperationOutcome Outcome
    {
        get
        {
            var word = (Result?.Action ?? RawState)?.Trim().ToLowerInvariant();
            switch (word)
            {
                case "updated" or "restarted" or "ran" or "rebooted" or "slept" or "cycled" or "succeeded" or "success" or "armed":
                    return OperationOutcome.Succeeded;
                case "noop" or "current" or "skipped" or "already-on-target":
                    return OperationOutcome.Noop;
                case "failed" or "rolled-back":
                    return OperationOutcome.Failed;
                case "deferred":
                    return OperationOutcome.Deferred;
                case "queued":
                    return OperationOutcome.Queued;
                case "cancelled" or "canceled":
                    return OperationOutcome.Cancelled;
                case "expired":
                    return OperationOutcome.Expired;
                case "interrupted":
                    return OperationOutcome.Interrupted;
                case "conflict":
                    return OperationOutcome.Conflict;
                case "rebooting" or "sleeping" or "accepted" or "running" or "started" or "already-running":
                    return OperationOutcome.Pending;
                case "installed":
                    return OperationOutcome.Succeeded;
            }
            if (State is OperationState.Running or OperationState.Queued) return OperationOutcome.Pending;
            if (State == OperationState.Finished)
            {
                // A finished record with an explicit ok is readable; one with neither an action nor
                // an ok is not, and saying so is the whole point of this branch.
                if (Result?.Ok == true) return OperationOutcome.Succeeded;
                if (Result?.Ok == false) return OperationOutcome.Failed;
                return OperationOutcome.Unresolved;
            }
            return OperationOutcome.Unresolved;
        }
    }

    public OperationSummary ToSummary() => new()
    {
        Id = Id,
        Kind = Kind,
        Service = Service,
        Target = Target,
        ActionId = ActionId,
        Mode = Mode,
        State = State,
        RawState = RawState,
        Phase = Phase,
        Action = Result?.Action,
        ReasonCode = Result?.ReasonCode,
        Message = Result?.Message,
        RequestedAt = RequestedAt,
        UpdatedAt = UpdatedAt ?? FinishedAt ?? StartedAt,
        ExpiresAt = ExpiresAt,
        Outcome = Outcome,
    };

    /// A one line description of what was asked for, for the rows that list operations.
    public string Describe() => OperationSummary.Describe(Kind, Service, Target, ActionId);
}

/// One service's outcome inside a cycle.
public sealed record OperationChild(string? OpId, string? Service, string? Action, string? ReasonCode)
{
    public static OperationChild From(Value json) => new(
        json["opId", "id"].AsText(),
        json["service"].AsText(),
        json["action"].AsText(),
        json["reasonCode", "reason"].AsText());
}

/// What was already holding the machine when a request was turned down.
public sealed record OperationConflict(string? OpId, string? Kind, string? Service, string? Phase, DateTimeOffset? StartedAt)
{
    public static OperationConflict? From(Value json)
    {
        if (!json.IsObject) return null;
        return new OperationConflict(
            json["opId", "id", "operationId"].AsText(),
            json["kind"].AsText(),
            json["service"].AsText(),
            json["phase"].AsText(),
            json["startedAt"].AsInstant());
    }
}

public sealed record OperationSummary
{
    public string? Id { get; init; }
    public string? Kind { get; init; }
    public string? Service { get; init; }
    public string? Target { get; init; }
    public string? ActionId { get; init; }
    public string? Mode { get; init; }
    public OperationState State { get; init; } = OperationState.Unknown;
    public string? RawState { get; init; }
    public string? Phase { get; init; }
    public string? Action { get; init; }
    public string? ReasonCode { get; init; }
    public string? Message { get; init; }
    public DateTimeOffset? RequestedAt { get; init; }
    public DateTimeOffset? UpdatedAt { get; init; }
    public DateTimeOffset? ExpiresAt { get; init; }
    public OperationOutcome Outcome { get; init; } = OperationOutcome.Unresolved;

    public static OperationSummary? From(Value json)
    {
        if (!json.IsObject) return null;
        var record = OperationRecord.From(json);
        return record?.ToSummary();
    }

    public string Describe() => Describe(Kind, Service, Target, ActionId);

    public static string Describe(string? kind, string? service, string? target, string? actionId) => kind switch
    {
        "update" => service is null ? "Update" : $"Update {service}",
        "restart" => service is null ? "Restart" : $"Restart {service}",
        "boot" => target is null ? "Boot" : $"Boot into {target}",
        "sleep" => "Sleep",
        "run" or "action" => actionId is null ? "Run an action" : $"Run {actionId}",
        "cycle" => "Scheduled cycle",
        "self-update" or "deploy" => "Agent update",
        "config" => "Setup",
        null => "Operation",
        _ => kind,
    };

    public string OutcomeText => Outcome switch
    {
        OperationOutcome.Succeeded => Action ?? "done",
        OperationOutcome.Noop => "nothing to do",
        OperationOutcome.Failed => "failed",
        OperationOutcome.Deferred => "deferred",
        OperationOutcome.Queued => "queued",
        OperationOutcome.Cancelled => "cancelled",
        OperationOutcome.Expired => "expired",
        OperationOutcome.Interrupted => "interrupted",
        OperationOutcome.Conflict => "refused, something else was running",
        OperationOutcome.Pending => Phase ?? "running",
        _ => "outcome not known",
    };
}

/// The immediate reply to a mutating command.
///
/// It carries the operation record when the agent keeps one, and stands alone when it does not, so
/// that a 2.x agent and a 3.x agent produce the same object above this line.
public sealed record ActionResult
{
    public AgentEnvelope Envelope { get; init; } = new();
    public string? Action { get; init; }
    public string? Service { get; init; }
    public string? Target { get; init; }
    public string? From_ { get; init; }
    public string? To { get; init; }
    public bool? AutoUpdate { get; init; }
    public int? ExitCode { get; init; }
    public string? Output { get; init; }
    public bool? Verified { get; init; }
    public bool? Detached { get; init; }
    public string? OperationId { get; init; }
    public DateTimeOffset? ExpiresAt { get; init; }
    public string? Replaced { get; init; }
    public bool Replayed { get; init; }
    public OperationRecord? Operation { get; init; }
    public OperationConflict? Conflict { get; init; }

    public static ActionResult From(Value json)
    {
        var operation = OperationRecord.From(json["op", "operation"]);
        return new ActionResult
        {
            Envelope = AgentEnvelope.From(json),
            Action = json["action"].AsText() ?? operation?.Result?.Action,
            Service = json["service"].AsText() ?? operation?.Service,
            Target = json["target"].AsText() ?? operation?.Target,
            From_ = json["from"].AsText() ?? operation?.From_,
            To = json["to"].AsText() ?? operation?.To,
            AutoUpdate = json["autoUpdate"].AsBool(),
            ExitCode = json["exitCode"].AsInt(),
            Output = json["output"].AsText() ?? operation?.Result?.Output,
            Verified = json["verified"].AsBool() ?? operation?.Result?.Verified,
            Detached = json["detached"].AsBool() ?? operation?.Detached,
            OperationId = json["operationId", "opId"].AsText() ?? operation?.Id,
            ExpiresAt = json["expiresAt"].AsInstant() ?? operation?.ExpiresAt,
            Replaced = json["replaced"].AsText() ?? operation?.Replaced,
            Replayed = (json["replayed"].AsBool() ?? false) || (operation?.Replayed ?? false),
            Operation = operation,
            Conflict = OperationConflict.From(json["conflict"]) ?? operation?.Conflict,
        };
    }

    /// Whether the agent took the work away and left this client to poll for it.
    public bool IsAccepted => string.Equals(Action, "accepted", StringComparison.OrdinalIgnoreCase);

    public bool IsCommandRefusal => Envelope.Ok == false && Operation is null
        && Envelope.ReasonCode is ReasonCode.Restricted or ReasonCode.BadArgument;

    public bool IsQueued => string.Equals(Action, "queued", StringComparison.OrdinalIgnoreCase);

    public bool IsConflict => string.Equals(Action, "conflict", StringComparison.OrdinalIgnoreCase)
        || Envelope.ReasonCode == Contract.ReasonCode.OperationInProgress;

    /// What this reply says happened, read through the same rules as an operation record so that a
    /// synchronous agent and a detaching one produce the same outcome above this line.
    public OperationOutcome Outcome
    {
        get
        {
            if (IsCommandRefusal) return OperationOutcome.Failed;
            if (IsAccepted) return OperationOutcome.Pending;
            if (IsQueued) return OperationOutcome.Queued;
            if (IsConflict) return OperationOutcome.Conflict;
            if (Operation is { } operation && operation.IsFinished) return operation.Outcome;
            var word = Action?.Trim().ToLowerInvariant();
            switch (word)
            {
                case "updated" or "restarted" or "ran" or "cycled" or "armed":
                    return OperationOutcome.Succeeded;
                case "noop" or "current" or "skipped" or "already-on-target":
                    return OperationOutcome.Noop;
                case "failed" or "rolled-back":
                    return OperationOutcome.Failed;
                case "deferred":
                    return OperationOutcome.Deferred;
                case "cancelled" or "canceled":
                    return OperationOutcome.Cancelled;
                case "expired":
                    return OperationOutcome.Expired;
                case "interrupted":
                    return OperationOutcome.Interrupted;
                // A machine that is going down has acknowledged and not yet arrived. It is pending
                // until something observes the other side of the transition, and never before.
                case "rebooting" or "sleeping":
                    return OperationOutcome.Pending;
                case "rebooted" or "slept" or "installed" or "rolled-back-agent":
                    return OperationOutcome.Succeeded;
                // The agent found the id already running and started nothing twice. What happened
                // is whatever that operation does, which is not known yet.
                case "already-running":
                    return OperationOutcome.Pending;
            }
            // A reply that carries a reason code is by definition not the happy path, so an action
            // word this build has never heard of must not be read as one. Saying "we do not know"
            // about a word nobody wrote a case for is the only honest answer.
            if (Envelope.ReasonCode is not null) return OperationOutcome.Unresolved;
            if (Envelope.Ok == true && word is not null) return OperationOutcome.Succeeded;
            if (Envelope.Ok == false) return OperationOutcome.Failed;
            return OperationOutcome.Unresolved;
        }
    }

    /// Whether this reply, on its own, says the machine is on its way down. Only an acknowledged
    /// reply ever does; a dropped link never does, wherever it dropped.
    public bool AcknowledgedTransition =>
        string.Equals(Action, "rebooting", StringComparison.OrdinalIgnoreCase)
        || string.Equals(Action, "sleeping", StringComparison.OrdinalIgnoreCase);
}

public sealed record DoctorCheck(string? Id, string? Name, string? Level, string? Summary, string? Detail, string? Fix)
{
    public static DoctorCheck From(Value json) => new(
        json["id"].AsText(),
        json["name"].AsText(),
        json["level", "status", "state"].AsText(),
        json["summary", "message"].AsText(),
        json["detail"].AsText(),
        json["fix", "remedy"].AsText());

    public string DisplayName => Name ?? Id ?? "check";

    public DoctorVerdict Verdict => Level?.Trim().ToLowerInvariant() switch
    {
        "ok" or "pass" or "good" => DoctorVerdict.Ok,
        "warn" or "warning" => DoctorVerdict.Warning,
        "fail" or "failed" or "error" => DoctorVerdict.Failed,
        "skip" or "skipped" => DoctorVerdict.Skipped,
        _ => DoctorVerdict.Unknown,
    };
}

public enum DoctorVerdict { Ok, Warning, Failed, Skipped, Unknown }

public sealed record DoctorReport(AgentEnvelope Envelope, IReadOnlyList<DoctorCheck> Checks)
{
    public static DoctorReport From(Value json) =>
        new(AgentEnvelope.From(json), json["checks"].Map(DoctorCheck.From));

    public int Failures => Checks.Count(check => check.Verdict == DoctorVerdict.Failed);
    public int Warnings => Checks.Count(check => check.Verdict == DoctorVerdict.Warning);
}

public sealed record HistoryReply(AgentEnvelope Envelope, IReadOnlyList<OperationSummary> Operations)
{
    public static HistoryReply From(Value json)
    {
        var list = json["operations", "history", "records", "items"];
        var operations = (list.IsArray ? list : json.IsArray ? json : Value.Missing)
            .Map(OperationSummary.From)
            .Where(summary => summary is not null)
            .Select(summary => summary!)
            .ToList();
        return new HistoryReply(AgentEnvelope.From(json), operations);
    }
}

public sealed record LogsReply(AgentEnvelope Envelope, IReadOnlyList<string> Lines, string? Path)
{
    public static LogsReply From(Value json)
    {
        var lines = json["lines", "log"];
        var text = lines.IsArray
            ? lines.Map(line => line.IsObject ? OperationLogLine.From(line).Line ?? "" : line.AsString() ?? "")
            : (json["text"].AsText()?.Split('\n') ?? Array.Empty<string>());
        return new LogsReply(AgentEnvelope.From(json), text.ToList(), json["path"].AsText());
    }
}

/// The reply to `config`, `config meta` and `config set`.
public sealed record ConfigReply
{
    public AgentEnvelope Envelope { get; init; } = new();
    public string? Hash { get; init; }
    public long? Bytes { get; init; }
    /// What the machine holds now, when it says: id, revision, lineage, who wrote it.
    public ControllerMark? Meta { get; init; }
    /// The document itself, on a `config` read. The agent sends it as the parsed object, so it is
    /// re-serialised here only to be canonicalised and hashed again on this side; the hash is
    /// compared against the reported one before anything is adopted.
    public string? Document { get; init; }
    /// stored | noop | replaced, on a `config set`.
    public string? Action { get; init; }
    /// Set on a refusal that is a real divergence rather than merely being behind.
    public bool Divergent { get; init; }
    /// On a refusal: what the machine holds instead.
    public ControllerMark? Current { get; init; }

    public static ConfigReply From(Value json)
    {
        var controller = json["controller"];
        var meta = ControllerMark.From(json["meta"]);
        // A `config set` reply carries the identity at the top level rather than under meta.
        if (meta is null && (json["id"].Exists || json["revision"].Exists))
        {
            meta = new ControllerMark
            {
                Hash = json["hash"].AsText(),
                Id = json["id"].AsText(),
                Revision = json["revision"].AsLong(),
                Source = json["source"].AsText(),
                Bytes = json["bytes"].AsLong(),
                Lineage = json["lineage"].AsStringList(),
            };
        }
        return new ConfigReply
        {
            Envelope = AgentEnvelope.From(json),
            Hash = json["hash"].AsText(),
            Bytes = json["bytes"].AsLong(),
            Meta = meta,
            Document = controller.IsObject ? controller.RawText() : controller.AsText(),
            Action = json["action"].AsText(),
            Divergent = json["divergent"].AsBool() ?? false,
            Current = ControllerMark.From(json["current"]),
        };
    }

    /// Whether the machine stored the document, took it as already held, or replaced what it had.
    public bool Stored => Action is "stored" or "noop" or "replaced" || Envelope.Ok == true;
}

/// The reply to `policy` and `policy set`.
public sealed record PolicyReply(AgentEnvelope Envelope, UpdatePolicy? Effective, string? Service)
{
    public static PolicyReply From(Value json) => new(
        AgentEnvelope.From(json),
        UpdatePolicy.From(json["updates", "policy", "effective"]),
        json["service"].AsText());
}

/// The reply to `version`.
public sealed record VersionReply(AgentEnvelope Envelope)
{
    public static VersionReply From(Value json) => new(AgentEnvelope.From(json));
}
