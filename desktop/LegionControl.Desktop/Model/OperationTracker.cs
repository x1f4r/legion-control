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
    /// True once asking again can tell nothing new: a terminal outcome, or a record the agent
    /// finished without saying how it went. Pending work is followed, and an unknown outcome with
    /// no finished record behind it is read again by the same id, never sent again.
    public bool IsResolved => Outcome is OperationOutcome.Succeeded or OperationOutcome.Failed or OperationOutcome.Deferred
        || (Outcome == OperationOutcome.Unknown && State == OperationState.Finished);
    /// True when the link went away and nothing has resolved it since. The amber row.
    public bool OutcomeUnknown { get; init; }
    /// The machine this helper action was run to wake, while it is a wake hold for it. Never
    /// published: holds are this device's own.
    public string? WakeTarget { get; init; }
    /// A wake hold: an action run for [WakeTarget] whose terminal record has not been read. Only
    /// that record, the agent saying it has no such operation, the target answering, the helper
    /// leaving the setup or a person releases it; never time, a restart or the helper being away.
    public bool IsWakeHold => WakeTarget is not null && State != OperationState.Finished;

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
///
/// The window and the command line keep the same list, and a wake hold in it is what stops a
/// helper command from running on two helpers. So every change takes `operations.json.lock`, folds
/// in what the other process wrote since this one last read or wrote the file, applies its one
/// change and writes the whole, the way [Config.LaunchApprovals] does; a hold the command line
/// recorded while the window was open is never written away by the window. The holds for a machine
/// are read again whenever the file changed, so a wake planned here sees one recorded there.
///
/// A file that is there but is not a list is moved aside before the next change is written over it,
/// so the operations it held can still be read by a person, and the log says where it went.
public sealed class OperationTracker
{
    private readonly string _path;
    private readonly bool _readOnly;
    private readonly object _gate = new();
    private readonly Dictionary<string, TrackedOperation> _operations = new(StringComparer.Ordinal);
    /// The file's time and size when this tracker last read or wrote it, and the ids it held then.
    private string? _signature;
    private HashSet<string> _onDisk = new(StringComparer.Ordinal);
    /// Why the file could not be read when this tracker last tried, or null.
    private string? _unreadable;

    /// [readOnly] is for a run that changes nothing on disk, as a smoke run does: changes are made
    /// to the list in memory, and nothing is written or locked.
    public OperationTracker(string? path = null, bool readOnly = false)
    {
        _path = path ?? AppPaths.OperationsFile;
        _readOnly = readOnly;
        ReloadIfChanged();
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
    public (TrackedOperation Operation, bool IsReplay) Begin(
        string id, string machineId, string? systemId, OperationIntent intent, string? wakeTarget = null)
    {
        lock (_gate)
        {
            using var held = Fresh();
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
                WakeTarget = wakeTarget,
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
            using var held = Fresh();
            // Stamped here, so a change made in this process wins over the older copy in another.
            _operations[operation.Id] = operation with { UpdatedAt = DateTimeOffset.UtcNow };
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
            using var held = Fresh();
            if (!_operations.TryGetValue(id, out var existing)) return null;
            if (!existing.Intent.Matches(record))
            {
                // This request is not that record, and an agent refuses an id that already names
                // another request: whatever this app asked for under it is not in effect there.
                var conflicted = existing with
                {
                    UpdatedAt = DateTimeOffset.UtcNow,
                    State = OperationState.Finished,
                    Outcome = OperationOutcome.Failed,
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
            using var held = Fresh();
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

    /// The wake holds recorded for [target], oldest first, with any the other process recorded or
    /// released since this one last looked.
    public IReadOnlyList<TrackedOperation> WakeHoldsFor(string target)
    {
        ReloadIfChanged();
        return All.Where(operation => operation.IsWakeHold && operation.WakeTarget == target).Reverse().ToList();
    }

    /// Folds in what the other process wrote, when the file's time or size changed since this one
    /// last read or wrote it, as when the command line ran something while the window was open.
    /// Returns whether it read the file.
    public bool ReloadIfChanged()
    {
        lock (_gate)
        {
            if (Signature() == _signature) return false;
            // Read under the lock as well: a reader holding the file open can make another
            // process's rename over it fail on Windows, and that write would be lost.
            using var held = Lock();
            return Merge();
        }
    }

    /// Releases every hold for [target] recorded before [before]: the target answered an
    /// authenticated status asked for after them, or a person chose to try the other helpers. The
    /// operations stay, still asked about; they only stop holding anything back.
    public void ReleaseWakeHolds(string target, DateTimeOffset before)
    {
        var released = false;
        lock (_gate)
        {
            using var held = Fresh();
            foreach (var operation in _operations.Values.Where(operation =>
                         operation.IsWakeHold && operation.WakeTarget == target && operation.StartedAt < before).ToList())
            {
                _operations[operation.Id] = operation with { WakeTarget = null, UpdatedAt = DateTimeOffset.UtcNow };
                released = true;
            }
            if (released) Save();
        }
        if (released) Changed?.Invoke();
    }

    /// Releases one hold: its helper left the setup.
    public void ReleaseWakeHold(string id)
    {
        lock (_gate)
        {
            using var held = Fresh();
            if (!_operations.TryGetValue(id, out var operation) || operation.WakeTarget is null) return;
            _operations[id] = operation with { WakeTarget = null, UpdatedAt = DateTimeOffset.UtcNow };
            Save();
        }
        Changed?.Invoke();
    }

    public void Forget(string id)
    {
        lock (_gate)
        {
            using var held = Fresh();
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
        // A hold is never pruned: only what releases it may end it.
        var stale = _operations.Values
            .Where(operation => operation.IsResolved && !operation.IsWakeHold && operation.UpdatedAt < cutoff)
            .Select(operation => operation.Id)
            .ToList();
        foreach (var id in stale) _operations.Remove(id);

        if (_operations.Count <= 400) return;
        foreach (var id in _operations.Values
                     .Where(operation => operation.IsResolved && !operation.IsWakeHold)
                     .OrderBy(operation => operation.UpdatedAt)
                     .Take(_operations.Count - 400)
                     .Select(operation => operation.Id)
                     .ToList())
        {
            _operations.Remove(id);
        }
    }

    /// Takes the lock every process that writes the list takes, and folds in the file as it is now,
    /// for one change made while the lock is held.
    private FileStream? Fresh()
    {
        var held = Lock();
        Merge();
        return held;
    }

    /// Folds in what the other process wrote since this one last read or wrote the list. The side
    /// that changed an entry later wins; an entry only the file has is taken; one the file held when
    /// this side last read or wrote it and holds no more was forgotten or pruned there. A file that
    /// is not there or cannot be read changes nothing here, and the next write puts this list back.
    private bool Merge()
    {
        // Taken before the read: a write that lands in between is then read again next time.
        _signature = Signature();
        if (Read() is not { } stored) return false;
        _unreadable = null;
        foreach (var (id, theirs) in stored)
        {
            if (!_operations.TryGetValue(id, out var ours) || theirs.UpdatedAt > ours.UpdatedAt) _operations[id] = theirs;
        }
        foreach (var id in _onDisk.Where(id => !stored.ContainsKey(id))) _operations.Remove(id);
        _onDisk = stored.Keys.ToHashSet(StringComparer.Ordinal);
        return true;
    }

    /// The exclusive handle that serialises writers across processes. The file is never unlinked,
    /// so two processes can never each hold a lock on a different file of the same name. Null when
    /// it cannot be had within ten seconds: the change is then made without it rather than not at
    /// all, as it was before there was one.
    private FileStream? Lock()
    {
        if (_readOnly) return null;
        var until = DateTimeOffset.UtcNow.AddSeconds(10);
        while (true)
        {
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(_path) ?? ".");
                return new FileStream(_path + ".lock", FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
            }
            catch (IOException) when (DateTimeOffset.UtcNow < until)
            {
                Thread.Sleep(50);
            }
            catch (Exception)
            {
                return null;
            }
        }
    }

    /// Modification time and size together, as [Config.LaunchApprovals] watches its file.
    private string? Signature()
    {
        try
        {
            var info = new FileInfo(_path);
            return info.Exists ? $"{info.LastWriteTimeUtc.Ticks}:{info.Length}" : null;
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// The list as the file holds it, or null when there is no file or it is not a list. A file
    /// that is there and holds no list is noted in [_unreadable], so the next write sets it aside.
    private Dictionary<string, TrackedOperation>? Read()
    {
        try
        {
            if (!File.Exists(_path)) return null;
            var json = Value.Parse(File.ReadAllText(_path));
            if (!json["operations"].IsArray)
            {
                _unreadable = json.Exists ? "it holds no operations list" : "it is not JSON";
                return null;
            }
            var stored = new Dictionary<string, TrackedOperation>(StringComparer.Ordinal);
            foreach (var entry in json["operations"].AsArray())
            {
                var id = entry["id"].AsText();
                var machineId = entry["machineId"].AsText();
                if (id is null || machineId is null) continue;
                stored[id] = new TrackedOperation
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
                    Outcome = ReadOutcome(entry["outcome"].AsText()),
                    Message = entry["message"].AsText(),
                    Output = entry["output"].AsText(),
                    // An earlier build kept a cancelled request as a word of its own and no code.
                    ReasonCode = entry["reasonCode"].AsText() ?? (entry["outcome"].AsText() == "Cancelled" ? ReasonCode.Cancelled : null),
                    ExpiresAt = entry["expiresAt"].AsInstant(),
                    OutcomeUnknown = entry["outcomeUnknown"].AsBool() ?? false,
                    WakeTarget = entry["wakeTarget"].AsText(),
                };
            }
            return stored;
        }
        catch (Exception)
        {
            // A list that cannot be read this time is read again on the next check, not taken for
            // read until the file changes. Nothing here is worth refusing to start over, and the
            // machines are asked about what they are running on the next status anyway.
            _signature = null;
            return null;
        }
    }

    /// An outcome as this list stores it. Builds before contract 3.1 wrote eleven words where the
    /// contract has five, and each of the others is read as the one section 5.6 gives that ending:
    /// nothing to do succeeded, a conflict, a cancelled or an expired request was deferred, an
    /// interruption is unknown, and a queued request is still pending. A word nobody wrote is
    /// pending, which is asked about again rather than believed.
    internal static OperationOutcome ReadOutcome(string? stored) => stored switch
    {
        "Succeeded" or "Noop" => OperationOutcome.Succeeded,
        "Failed" => OperationOutcome.Failed,
        "Deferred" or "Conflict" or "Cancelled" or "Expired" => OperationOutcome.Deferred,
        "Unknown" or "Unresolved" or "Interrupted" => OperationOutcome.Unknown,
        _ => OperationOutcome.Pending,
    };

    private void Save()
    {
        if (_readOnly) return;
        try
        {
            if (_unreadable is { } why && File.Exists(_path))
            {
                StateFile.SetAside(_path, why);
                _unreadable = null;
            }
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
                    wakeTarget = operation.WakeTarget,
                }).ToList(),
            };
            AtomicWrite.Text(_path, JsonSerializer.Serialize(payload, new JsonSerializerOptions { WriteIndented = true }));
            _signature = Signature();
            _onDisk = _operations.Keys.ToHashSet(StringComparer.Ordinal);
        }
        catch (Exception)
        {
            // Losing the list costs the ability to ask about an operation after a restart. It is
            // not worth failing the operation itself over, and the row on screen still says so.
        }
    }
}
