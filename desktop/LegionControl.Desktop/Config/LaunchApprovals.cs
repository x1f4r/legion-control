using System.Text.Json;
using System.Text.Json.Nodes;

namespace LegionControl.Desktop.Config;

/// The launch values this device approved, kept in `approved-launch.json` beside the applied-setup
/// state.
///
/// Never published, never merged and never in the bindings: an older build rewrites the bindings
/// with only the keys it knows and reads a corrupt one as empty, and either would drop every
/// approval on a downgrade. The window and the command line both write here, so every change takes
/// `approved-launch.json.lock`, reads the file again, applies its one change and writes it
/// atomically, the way [Transport.HostIdentityStore] does.
///
/// A record that cannot be read approves nothing: everything waits and the problem is shown. It is
/// never seeded again on its own, because that would approve whatever the document holds now. The
/// next write that approves something moves the old file aside to
/// `approved-launch.json.corrupt-<unix time>` and starts a new one. A record that is not there is
/// seeded at a start and only then (contract section 11): gone while the app runs, or not written
/// at the start, it approves nothing until the next one. Seeded later in the same run, it would
/// approve whatever a peer's revision had brought in by then. The next start does record the
/// setup as it is then, a peer's revision included, which is the local reset deleting the file
/// asks for; every command-line run is a start of its own. An approval or an edit made before
/// that writes a new record holding only what it approved, and a start never seeds over it.
public sealed class LaunchApprovals
{
    private static readonly object Sync = new();
    private static readonly JsonSerializerOptions Writing = new() { WriteIndented = true };

    private string? _signature;
    /// Set once the start has had its one chance to seed.
    private bool _started;

    /// [readOnly] is for a run that changes nothing on disk, as a smoke run does. Every change is
    /// then made to the record in memory only: a first start's seed is evaluated as the start would
    /// write it, so the run shows what a start would drive, and nothing is written or locked.
    public LaunchApprovals(string? path = null, bool readOnly = false)
    {
        Path = path ?? System.IO.Path.Combine(AppPaths.Home, "approved-launch.json");
        ReadOnly = readOnly;
        lock (Sync) Reload();
    }

    public string Path { get; }

    public bool ReadOnly { get; }

    /// The record as last read, or null when there is none or it cannot be read.
    public LaunchRecord? Record { get; private set; }

    /// What the gate evaluates against: the record, or one that approves nothing.
    public LaunchRecord Current => Record ?? LaunchRecord.Empty();

    /// True when there is no file at all, which is the state a first start of this build is in.
    public bool IsMissing { get; private set; }

    /// Why the record could not be read or written, or null.
    public string? Problem { get; private set; }

    /// Reads the record again when its file changed underneath, as when the command line approved
    /// something while the window was open. Returns whether it did.
    public bool ReloadIfChanged()
    {
        lock (Sync)
        {
            if (Signature() == _signature) return false;
            Reload();
            return true;
        }
    }

    /// The first start of this build: every launch value of the document this device already
    /// applies is approved, with `seededFrom` its hash, so an upgrade drives exactly what it drove
    /// before and asks nothing. With no document the record says so (`"none"`), and no later
    /// adoption can count as a seed. A record that exists, readable or not, is left alone.
    ///
    /// Called once, at the start, before anything is evaluated against the record. Only that call
    /// seeds, whether or not it could write: from then on a missing record approves nothing until
    /// the next start.
    public string? SeedIfMissing(ControllerDocument? document)
    {
        lock (Sync)
        {
            if (_started) return null;
            _started = true;
            return Change(current =>
                current is { Missing: true }
                    ? LaunchRecord.Seeded(LaunchProjection.Of(document), document?.Hash ?? "none")
                    : null);
        }
    }

    /// An edit made on this device approves each launch value it changed, at its new value, and
    /// nothing else. Called before the setup file is written, so the window never flashes a review
    /// of what the person just typed.
    ///
    /// An edit that arrives before the seed (a hand edit found while the setup is loaded at the
    /// start) seeds from the document it was made to, then approves the edit on top. One that
    /// arrives later over a missing record approves only what it changed.
    ///
    /// What waited for review in the document the edit was made to stays waiting (see
    /// [LeavingWaiting]). An edit whose starting point cannot be read approves nothing, because
    /// nobody can say what it changed.
    public string? RecordLocalEdit(byte[]? before, ControllerDocument after)
    {
        LaunchProjection? previous;
        try
        {
            previous = before is null ? null : LaunchProjection.Of(before);
        }
        catch (Canonical.NotUtf8)
        {
            return null;
        }
        var changes = LaunchValues.Changes(previous, LaunchProjection.Of(after));
        return Change(current =>
        {
            if (current.Missing && !_started)
                return Apply(LaunchRecord.Seeded(previous ?? LaunchProjection.Empty, before is null ? "none" : Canonical.Hash(before)), changes);
            var record = current.Record ?? LaunchRecord.Empty();
            var approved = previous is null ? changes : LeavingWaiting(changes, previous, record);
            return approved.Count == 0 ? null : Apply(record, approved);
        }, setAside: true);
    }

    /// The changes of a local edit less those that would approve a value nobody here chose, given
    /// what waited for review in the document [before] it was made to: a waiting value carried to
    /// another place, whatever shell it was given there, and a new tuple at a system whose own
    /// tuple waits, which is what an edit to the shell of a peer's command makes of that command
    /// (see [LaunchGate.Carried]). The editors refuse those edits outright. A hand edit of the file
    /// is already written by the time it is read, so it applies, and the values it carried wait for
    /// review like any other.
    private static IReadOnlyList<LaunchChange> LeavingWaiting(
        IReadOnlyList<LaunchChange> changes, LaunchProjection before, LaunchRecord record)
    {
        var waiting = LaunchValues.Review(record, before);
        return changes.Where(change => LaunchGate.Carried(change, before, record, waiting) is null).ToList();
    }

    /// Records exactly the values the entries showed, at their places. An entry that cannot be
    /// approved is skipped: a value that cannot be shown faithfully can only be reverted or left.
    public string? Approve(IEnumerable<LaunchReviewEntry> entries)
    {
        var changes = entries.Where(entry => entry.Approvable).Select(entry => entry.Change()).ToList();
        if (changes.Count == 0) return null;
        return Change(current => Apply(current.Record ?? LaunchRecord.Empty(), changes), setAside: true);
    }

    /// Enrolment: every launch value of a document this device is about to apply as its first
    /// setup, after a person was shown all of them and confirmed.
    public string? ApproveAll(ControllerDocument document)
    {
        var changes = LaunchValues.Changes(null, LaunchProjection.Of(document));
        if (changes.Count == 0) return null;
        return Change(current => Apply(current.Record ?? LaunchRecord.Empty(), changes), setAside: true);
    }

    /// A value accepted by reuse or by migration at a place whose record is missing or different is
    /// recorded there. The value it replaces leaves the approved set: that is the only pruning.
    /// Returns whether the record changed.
    ///
    /// Every such value is written in one pass, computed from the record the gate evaluated, so a
    /// value moved from one place to another stays approved at the place it moved to. Nothing is
    /// written over a record that cannot be read: it approves nothing, so nothing was accepted.
    public bool RecordAccepted(IReadOnlyList<EffectiveLaunch> effective)
    {
        var record = Current;
        var changes = new List<LaunchChange>();
        foreach (var entry in effective.Where(entry => entry.Use == LaunchUse.Offered))
        {
            LaunchChange? change = entry.Scope switch
            {
                LaunchScope.System => new LaunchChange(entry.Scope, entry.MachineId, entry.SystemId, entry.Tuple!.ToJson()),
                LaunchScope.IdentityFile => new LaunchChange(entry.Scope, entry.MachineId, null, entry.Text!),
                _ => null,
            };
            if (change is null) continue;
            var (present, value) = record.At(change.Scope, change.MachineId, change.SystemId);
            if (present && JsonNode.DeepEquals(value, change.Value)) continue;
            changes.Add(change);
        }
        if (changes.Count == 0) return false;
        var before = Record;
        Change(current => current.Record is { } readable ? Apply(readable, changes) : null);
        return Record is { } after && (before is null || !after.SameAs(before));
    }

    private static LaunchRecord Apply(LaunchRecord record, IEnumerable<LaunchChange> changes)
    {
        foreach (var change in changes) record = record.With(change);
        return record;
    }

    /// What the file held when a change was about to be applied to it.
    private sealed record OnDisk(LaunchRecord? Record, bool Missing);

    /// Takes the lock, reads the file again, applies one change and writes it. [change] returns null
    /// to leave the file as it is.
    ///
    /// A file that cannot be read is shown no record. With [setAside] the change starts from an
    /// empty one instead, and when it has something to write the old file is moved out of the way
    /// first; without it, nothing is ever written over the old file.
    private string? Change(Func<OnDisk, LaunchRecord?> change, bool setAside = false)
    {
        lock (Sync)
        {
            if (ReadOnly)
            {
                if (change(new OnDisk(Record, IsMissing)) is { } next)
                {
                    Record = next;
                    IsMissing = false;
                }
                return null;
            }
            try
            {
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(Path) ?? ".");
                using var held = Lock();
                var (record, problem, unread) = Read();
                // A file that could not be opened this once is not one that cannot be read: it is
                // neither set aside nor written over, and the change waits for the next try.
                if (unread) throw new IOException(problem);
                var unreadable = problem is not null;
                var next = change(new OnDisk(unreadable && setAside ? LaunchRecord.Empty() : record, record is null && !unreadable));
                if (next is not null && unreadable && setAside)
                {
                    SetAside();
                    AtomicWrite.Text(Path, next.ToJson().ToJsonString(Writing));
                }
                else if (next is not null && !unreadable && (record is null || !next.SameAs(record)))
                {
                    AtomicWrite.Text(Path, next.ToJson().ToJsonString(Writing));
                }
                Reload();
                return null;
            }
            catch (Exception error)
            {
                Problem = $"Could not save the approved commands to {Path}: {error.Message}";
                return Problem;
            }
        }
    }

    /// The exclusive handle that serialises writers across processes. The file is never unlinked,
    /// so two processes can never each hold a lock on a different file of the same name.
    private FileStream Lock()
    {
        var until = DateTimeOffset.UtcNow.AddSeconds(10);
        while (true)
        {
            try
            {
                return new FileStream(Path + ".lock", FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
            }
            catch (IOException) when (DateTimeOffset.UtcNow < until)
            {
                Thread.Sleep(50);
            }
        }
    }

    private void SetAside()
    {
        var stamp = DateTimeOffset.UtcNow.ToUnixTimeSeconds();
        var target = $"{Path}.corrupt-{stamp}";
        for (var attempt = 2; File.Exists(target); attempt += 1) target = $"{Path}.corrupt-{stamp}-{attempt}";
        File.Move(Path, target);
    }

    private void Reload()
    {
        // Taken before the read, and kept only when the file was read: one that a virus scanner
        // or a writer held open for that one read is read again on the next check, rather than
        // taken for a record that cannot be read until it changes.
        var signature = Signature();
        var (record, problem, unread) = Read();
        _signature = unread ? null : signature;
        Record = record;
        IsMissing = record is null && problem is null;
        Problem = problem ?? (IsMissing && _started
            ? $"There is no record of approved commands at {Path}, so nothing counts as approved on this device. "
              + "The next start of this app, and every command-line run is one, records the setup it applies then, as a first start does. "
              + "An approval or an edit made before then starts a new record holding only what it approved: no start records the setup after that, and everything else waits for review."
            : null);
    }

    /// The record, or why there is none. [Unread] is a file that could not be opened or read this
    /// time, which says nothing about what it holds.
    private (LaunchRecord? Record, string? Problem, bool Unread) Read()
    {
        if (!File.Exists(Path)) return (null, null, false);
        try
        {
            if (JsonNode.Parse(File.ReadAllText(Path)) is not JsonObject json)
                return (null, $"The approved commands in {Path} are not a JSON object, so nothing counts as approved.", false);
            if (json["schema"] is not JsonValue schema || schema.GetValueKind() != JsonValueKind.Number
                || !schema.TryGetValue<int>(out var version) || version != 1)
                return (null, $"The approved commands in {Path} use a format this build does not know, so nothing counts as approved.", false);
            return (new LaunchRecord(json), null, false);
        }
        catch (Exception error)
        {
            return (null, $"The approved commands in {Path} could not be read ({error.Message}), so nothing counts as approved.",
                error is IOException or UnauthorizedAccessException);
        }
    }

    /// Modification time and size together, as [ConfigStore] watches its file.
    private string? Signature()
    {
        try
        {
            var info = new FileInfo(Path);
            return info.Exists ? $"{info.LastWriteTimeUtc.Ticks}:{info.Length}" : null;
        }
        catch (Exception)
        {
            return null;
        }
    }
}
