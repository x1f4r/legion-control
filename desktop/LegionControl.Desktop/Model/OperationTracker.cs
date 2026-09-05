using System.Text.Json;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Model;

/// What an operation was asked to do, as this client asked for it.
///
/// An operation id binds this and nothing else. The rule, and the reason this record exists: the
/// same id with a different intent is a conflict, not a replay. A client that reuses an id for a
/// different request would otherwise be told "that already ran" about something else entirely, and
/// would draw a restart it never got as a restart that happened.
public sealed record OperationIntent(
    string Kind,
    string? Service = null,
    string? Target = null,
    string? ActionId = null,
    bool Force = false,
    bool WhenIdle = false)
{
    public string Key => string.Join("|", Kind, Service ?? "", Target ?? "", ActionId ?? "", Force, WhenIdle);

    /// Whether a record the agent returned is a record of this request, rather than of some other
    /// request that happens to carry the same id.
    public bool Matches(OperationRecord record)
    {
        if (record.Kind is { } kind && !KindsAgree(kind, Kind)) return false;
        if (Service is not null && record.Service is not null && record.Service != Service) return false;
        if (Target is not null && record.Target is not null && record.Target != Target) return false;
        if (ActionId is not null && record.ActionId is not null && record.ActionId != ActionId) return false;
        return true;
    }

    /// `run` and `action` are the same kind under two names, and nothing else is.
    private static bool KindsAgree(string left, string right) =>
        Normalise(left) == Normalise(right);

    private static string Normalise(string kind) => kind switch
    {
        "action" => "run",
        "deploy" => "self-update",
        _ => kind,
    };

    public string Describe() => OperationSummary.Describe(Kind, Service, Target, ActionId);
}

/// One operation this client started, kept on disk so a restart, a crash or a lost link cannot
/// turn a change that happened into a change nobody can ask about.
public sealed record TrackedOperation
{
    public required string Id { get; init; }
    public required string MachineId { get; init; }
    public string? SystemId { get; init; }
    public required OperationIntent Intent { get; init; }
    public DateTimeOffset StartedAt { get; init; } = DateTimeOffset.UtcNow;
    public DateTimeOffset UpdatedAt { get; init; } = DateTimeOffset.UtcNow;
    public OperationState State { get; init; } = OperationState.Unknown;
    public string? Phase { get; init; }
    public OperationOutcome Outcome { get; init; } = OperationOutcome.Pending;
    public string? Message { get; init; }
    public string? Output { get; init; }
    public string? ReasonCode { get; init; }
    public DateTimeOffset? ExpiresAt { get; init; }
    /// True once the outcome is something other than "we are still waiting to find out".
    public bool IsResolved => Outcome is not (OperationOutcome.Pending or OperationOutcome.Unresolved);
    /// True when the link went away and nothing has resolved it since. The amber row.
    public bool OutcomeUnknown { get; init; }

    public TrackedOperation WithRecord(OperationRecord record) => this with
    {
        UpdatedAt = DateTimeOffset.UtcNow,
        State = record.State,
        Phase = record.Phase,
        Outcome = record.Outcome,
        Message = record.Result?.Message,
        Output = record.Result?.Output ?? Output,
        ReasonCode = record.Result?.ReasonCode,
        ExpiresAt = record.ExpiresAt,
        OutcomeUnknown = false,
    };
}

/// Every operation this client has started and not yet seen the end of.
///
/// Written through a temporary file and a rename, like everything else this app persists, so a
/// crash in the middle of an update leaves either the old list or the new one and never half of
/// either.
public sealed class OperationTracker
{
    private readonly string _path;
    private readonly object _gate = new();
    private readonly Dictionary<string, TrackedOperation> _operations = new(StringComparer.Ordinal);

    public OperationTracker(string? path = null)
    {
        _path = path ?? AppPaths.OperationsFile;
        Load();
    }

    public event Action? Changed;

    public IReadOnlyList<TrackedOperation> All
    {
        get
        {
            lock (_gate) return _operations.Values.OrderByDescending(operation => operation.StartedAt).ToList();
        }
    }

    public IReadOnlyList<TrackedOperation> Unfinished =>
        All.Where(operation => !operation.IsResolved).ToList();

    public IReadOnlyList<TrackedOperation> For(string machineId) =>
        All.Where(operation => operation.MachineId == machineId).ToList();

    public TrackedOperation? Find(string id)
    {
        lock (_gate) return _operations.GetValueOrDefault(id);
    }

    /// Starts tracking a request, or hands back the one already tracked under this id.
    ///
    /// The conflict case is the point: an id whose stored intent is not the intent being asked for
    /// is refused here, before anything is sent, because the agent would answer about the other
    /// request and this client would draw the wrong thing.
    public (TrackedOperation Operation, bool IsReplay) Begin(string id, string machineId, string? systemId, OperationIntent intent)
    {
        lock (_gate)
        {
            if (_operations.TryGetValue(id, out var existing))
            {
                if (existing.Intent.Key != intent.Key)
                {
                    throw new InvalidOperationException(
                        $"Operation {id} was started as \"{existing.Intent.Describe()}\" and cannot be reused for \"{intent.Describe()}\".");
                }
                return (existing, true);
            }
            var operation = new TrackedOperation
            {
                Id = id,
                MachineId = machineId,
                SystemId = systemId,
                Intent = intent,
            };
            _operations[id] = operation;
            Save();
            Changed?.Invoke();
            return (operation, false);
        }
    }

    public void Update(TrackedOperation operation)
    {
        lock (_gate)
        {
            _operations[operation.Id] = operation;
            Prune();
            Save();
        }
        Changed?.Invoke();
    }

    /// Folds an agent record into what this client knows.
    ///
    /// A record whose intent does not match the one this id was started for is not applied at all:
    /// it is somebody else's operation under a colliding id, and the honest answer is to say so
    /// rather than to adopt its outcome.
    public TrackedOperation? Apply(string id, OperationRecord record)
    {
        lock (_gate)
        {
            if (!_operations.TryGetValue(id, out var existing)) return null;
            if (!existing.Intent.Matches(record))
            {
                var conflicted = existing with
                {
                    UpdatedAt = DateTimeOffset.UtcNow,
                    Outcome = OperationOutcome.Conflict,
                    Message = $"The machine has an operation {id} that is a {record.Describe()}, not the {existing.Intent.Describe()} this app asked for.",
                    OutcomeUnknown = false,
                };
                _operations[id] = conflicted;
                Save();
                Changed?.Invoke();
                return conflicted;
            }
            var updated = existing.WithRecord(record);
            _operations[id] = updated;
            Prune();
            Save();
            Changed?.Invoke();
            return updated;
        }
    }

    /// Marks an operation as one whose outcome is genuinely not known: the link went away after
    /// the command was dispatched. Never a failure and never a success.
    public void MarkUnknown(string id, string message)
    {
        lock (_gate)
        {
            if (!_operations.TryGetValue(id, out var existing)) return;
            _operations[id] = existing with
            {
                UpdatedAt = DateTimeOffset.UtcNow,
                OutcomeUnknown = true,
                Outcome = OperationOutcome.Pending,
                Message = message,
            };
            Save();
        }
        Changed?.Invoke();
    }

    public void Forget(string id)
    {
        lock (_gate)
        {
            _operations.Remove(id);
            Save();
        }
        Changed?.Invoke();
    }

    /// Keeps the list from growing without bound. Resolved operations older than a fortnight go;
    /// unresolved ones never do, because an operation nobody ever found the end of is exactly the
    /// thing this list exists to keep.
    private void Prune()
    {
        var cutoff = DateTimeOffset.UtcNow - TimeSpan.FromDays(14);
        var stale = _operations.Values
            .Where(operation => operation.IsResolved && operation.UpdatedAt < cutoff)
            .Select(operation => operation.Id)
            .ToList();
        foreach (var id in stale) _operations.Remove(id);

        if (_operations.Count <= 400) return;
        foreach (var id in _operations.Values
                     .Where(operation => operation.IsResolved)
                     .OrderBy(operation => operation.UpdatedAt)
                     .Take(_operations.Count - 400)
                     .Select(operation => operation.Id)
                     .ToList())
        {
            _operations.Remove(id);
        }
    }

    private void Load()
    {
        try
        {
            if (!File.Exists(_path)) return;
            var json = Value.Parse(File.ReadAllText(_path));
            foreach (var entry in json["operations"].AsArray())
            {
                var id = entry["id"].AsText();
                var machineId = entry["machineId"].AsText();
                if (id is null || machineId is null) continue;
                _operations[id] = new TrackedOperation
                {
                    Id = id,
                    MachineId = machineId,
                    SystemId = entry["systemId"].AsText(),
                    Intent = new OperationIntent(
                        entry["intent"]["kind"].AsText() ?? "update",
                        entry["intent"]["service"].AsText(),
                        entry["intent"]["target"].AsText(),
                        entry["intent"]["actionId"].AsText(),
                        entry["intent"]["force"].AsBool() ?? false,
                        entry["intent"]["whenIdle"].AsBool() ?? false),
                    StartedAt = entry["startedAt"].AsInstant() ?? DateTimeOffset.UtcNow,
                    UpdatedAt = entry["updatedAt"].AsInstant() ?? DateTimeOffset.UtcNow,
                    State = OperationRecord.ReadState(entry["state"].AsText(), null, entry["finishedAt"].AsInstant()),
                    Phase = entry["phase"].AsText(),
                    Outcome = Enum.TryParse<OperationOutcome>(entry["outcome"].AsText(), out var outcome)
                        ? outcome
                        : OperationOutcome.Pending,
                    Message = entry["message"].AsText(),
                    Output = entry["output"].AsText(),
                    ReasonCode = entry["reasonCode"].AsText(),
                    ExpiresAt = entry["expiresAt"].AsInstant(),
                    OutcomeUnknown = entry["outcomeUnknown"].AsBool() ?? false,
                };
            }
        }
        catch (Exception)
        {
            // A list that cannot be read is a list this app has to start again. Nothing here is
            // worth refusing to start over, and the machines are asked about what they are running
            // on the next status anyway.
        }
    }

    private void Save()
    {
        try
        {
            var payload = new
            {
                version = 1,
                operations = _operations.Values.Select(operation => new
                {
                    id = operation.Id,
                    machineId = operation.MachineId,
                    systemId = operation.SystemId,
                    intent = new
                    {
                        kind = operation.Intent.Kind,
                        service = operation.Intent.Service,
                        target = operation.Intent.Target,
                        actionId = operation.Intent.ActionId,
                        force = operation.Intent.Force,
                        whenIdle = operation.Intent.WhenIdle,
                    },
                    startedAt = operation.StartedAt.ToString("o"),
                    updatedAt = operation.UpdatedAt.ToString("o"),
                    state = operation.State.ToString().ToLowerInvariant(),
                    phase = operation.Phase,
                    outcome = operation.Outcome.ToString(),
                    message = operation.Message,
                    output = operation.Output,
                    reasonCode = operation.ReasonCode,
                    expiresAt = operation.ExpiresAt?.ToString("o"),
                    outcomeUnknown = operation.OutcomeUnknown,
                }).ToList(),
            };
            AtomicWrite.Text(_path, JsonSerializer.Serialize(payload, new JsonSerializerOptions { WriteIndented = true }));
        }
        catch (Exception)
        {
            // Losing the list costs the ability to ask about an operation after a restart. It is
            // not worth failing the operation itself over, and the row on screen still says so.
        }
    }
}
