using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// Holds whatever the setup document last said, and notices when it changes.
///
/// Two rules everything above depends on. A document that does not parse never replaces one that
/// did: a half-saved file is the normal state of a file being edited, and throwing the machines
/// away every time an editor writes a partial document would make the app flicker through its own
/// setup page. And every document this device applies is remembered by hash, because a merge needs
/// the common ancestor and a person who adopted the wrong thing needs the previous one back.
///
/// There is no authority here. Every device is a peer: this one edits, publishes, and fast-forwards
/// to what another peer wrote, all through the same lineage rule.
public sealed class ConfigStore : IDisposable
{
    public string Path { get; }
    /// Where the applied hash is remembered, so a hand edit can be told from this app's own write.
    public string StatePath { get; }
    public SetupLedger Ledger { get; }
    /// The name this device puts on the revisions it writes.
    public string DeviceName { get; set; }

    private FileSystemWatcher? _watcher;
    private string? _lastSignature;
    private AppliedSetup _applied = new();
    private string? _appliedSignature;
    private readonly object _gate = new();

    /// [localEdit] is subscribed to [LocalEdit] before the file is first read, because that first
    /// read can already be an edit made on this device: a hand edit made while the app was closed.
    public ConfigStore(
        string? path = null,
        string? statePath = null,
        SetupLedger? ledger = null,
        string? deviceName = null,
        bool watch = true,
        Action<byte[]?, ControllerDocument>? localEdit = null)
    {
        Path = path ?? AppPaths.ConfigFile;
        StatePath = statePath ?? System.IO.Path.Combine(AppPaths.Home, "setup-state.json");
        Ledger = ledger ?? new SetupLedger();
        DeviceName = deviceName ?? Environment.MachineName;
        if (localEdit is not null) LocalEdit += localEdit;
        Load();
        if (watch) StartWatching();
    }

    /// The last document that parsed and validated, or null when none ever has.
    public ControllerConfig? Config { get; private set; }
    /// Why the file on disk was refused, or null when it read cleanly.
    public string? Problem { get; private set; }
    /// Worth saying without refusing the document: helpers that may be asleep, machines nothing can
    /// wake, a helper at another site.
    public IReadOnlyList<string> Warnings { get; private set; } = Array.Empty<string>();
    /// True when there is simply nothing at the path.
    public bool IsMissing { get; private set; }
    /// The canonical bytes and their hash: what the machines are given and what they compare.
    public ControllerDocument? Document { get; private set; }
    /// The raw file, for the editor, which has to start from what is actually on disk.
    public byte[]? RawBytes { get; private set; }
    /// Set once when this device gave a document with no identity a setup id of its own, so the UI
    /// can say so rather than leaving a new id to be discovered in a diff.
    public string? AssignedSetupId { get; private set; }

    public event Action? Changed;

    /// An edit made on this device is about to be written: the document it was made to (null when
    /// there was none) and the document it makes. An edit approves the launch values it changes
    /// (contract section 9.9), and this is raised before the write so the approval is in place
    /// before the document is applied and nobody is asked about what they just typed.
    ///
    /// Raised by [ApplyEdit] and [ApplyEditIfCurrent], by a hand edit of the file that keeps its
    /// `controller` block, by a file with no identity that is given one here, and by
    /// [WriteExample]. Never by [Apply] or [ApplyIfCurrent], and never for a document either of
    /// those wrote, here or in another process on this device: what another peer wrote, a merge
    /// and a kept revision approve nothing.
    public event Action<byte[]?, ControllerDocument>? LocalEdit;

    public IReadOnlyList<MachineConfig> Machines => Config?.Machines ?? Array.Empty<MachineConfig>();

    public ControllerIdentity Identity => Document?.Identity ?? new ControllerIdentity();

    public bool HasAnything => Machines.Count > 0;

    /// A cheap "did it change" check, called from the poll and whenever a window comes back.
    public void ReloadIfChanged()
    {
        var signature = Signature(Path);
        if (signature != _lastSignature) Load();
        if (_watcher is null) StartWatching();
    }

    public void Load()
    {
        lock (_gate) LoadCore();
    }

    private void LoadCore()
    {
        _lastSignature = Signature(Path);

        byte[] data;
        try
        {
            data = File.ReadAllBytes(Path);
        }
        catch (Exception)
        {
            IsMissing = true;
            // Not an error worth showing: no file is the state a fresh install is in, and the setup
            // page says what to do about it. An earlier document is kept, so a file being rewritten
            // in place does not blank the window on its way past zero bytes.
            Problem = null;
            if (Config is not null && File.Exists(Path)) return;
            if (Config is not null)
            {
                Config = null;
                Document = null;
                RawBytes = null;
                Changed?.Invoke();
            }
            return;
        }

        IsMissing = false;
        ControllerDocument document;
        try
        {
            document = ControllerDocument.FromRaw(data);
        }
        catch (Canonical.NotUtf8)
        {
            Problem = "The setup file is not valid UTF-8, so it has no canonical form and no hash.";
            return;
        }

        var json = Value.Parse(document.Text);
        if (!json.IsObject)
        {
            Problem = "The setup file is not a JSON object.";
            return;
        }

        var decoded = ControllerConfig.From(json);
        var problems = decoded.Problems();
        if (problems.Count > 0)
        {
            Problem = string.Join(" ", problems);
            return;
        }

        // The window and the command line both adopt documents into this file. What the other one
        // applied counts as applied here too, or its Take theirs would read as an edit of the
        // revision it replaced.
        ReloadApplied();

        // A document that carries no identity of its own becomes a setup here and now, before it is
        // applied or published. Without one, two devices holding the same hand-written file would
        // look like two different setups to every machine they reach.
        if (!document.Identity.HasIdentity)
        {
            var assigned = ControllerIdentity.NewSetup(decoded.Identity.Name, DeviceName);
            // A file with no identity is the user's own: one written by hand, or the first one on
            // this device. Whatever it changed from what was applied here is theirs. One that a
            // process here adopted is stamped onto exactly itself, so nothing it carries counts as
            // changed.
            var editedFrom = WasAdopted(document) ? new EditedFrom(document.Bytes) : AppliedEditedFrom();
            if (StampIdentity(assigned, data, "assigned-setup-id", editedFrom) is { } stampProblem)
            {
                Problem = stampProblem;
                return;
            }
            AssignedSetupId = assigned.Id;
            Load();
            return;
        }

        // A file somebody edited by hand carries the identity of the revision it was edited from.
        // That is an edit like any other: it gets the next revision, and the document it came from
        // goes into the lineage so every machine holding that document fast-forwards to this one.
        //
        // Two branches edited from the same parent carry the same identity, so a sibling that a
        // process here adopted looks exactly like a hand edit of the one it replaced, and would
        // approve every launch value it changed. It is told apart by being kept, which it already
        // is when the file changes.
        if (_applied.Hash is { } applied
            && applied != document.Hash
            && _applied.MatchesIdentityOf(document.Identity)
            && !WasAdopted(document))
        {
            var next = document.Identity.Next(applied, DeviceName);
            if (StampIdentity(next, data, "hand-edit", AppliedEditedFrom()) is { } stampProblem)
            {
                Problem = stampProblem;
                return;
            }
            Load();
            return;
        }

        Problem = null;
        Warnings = decoded.Warnings();
        RawBytes = data;
        var unchanged = Config == decoded && Document?.Hash == document.Hash;
        Document = document;
        Ledger.Remember(document);
        RecordApplied(document);
        if (unchanged) return;
        Config = decoded;
        Changed?.Invoke();
    }

    /// The document a local edit was made to: [Bytes] null when nothing was applied before it.
    private sealed record EditedFrom(byte[]? Bytes);

    /// What this device applied last, as the document a hand edit started from: no document when
    /// it never applied one, and null when it did but those bytes are no longer anywhere. Without
    /// them nobody can say which launch values the edit changed, so it approves none of them and
    /// they wait for review like any other.
    private EditedFrom? AppliedEditedFrom()
    {
        if (_applied.Hash is not { } hash) return new EditedFrom(null);
        if (Document?.Hash == hash) return new EditedFrom(Document.Bytes);
        return Ledger.Read(hash) is { } kept ? new EditedFrom(kept.Bytes) : null;
    }

    /// Whether the file holds a document that a process on this device adopted rather than one a
    /// person wrote. [ApplyCore] keeps what it adopts before it writes the file, so this holds even
    /// while the applied state still names the document before it: the window can read the file
    /// the command line just wrote before the command line has recorded it as applied.
    private bool WasAdopted(ControllerDocument document) => Ledger.Has(document.Hash);

    /// Reads the applied state again when another process wrote it since this one last read or
    /// wrote it.
    private void ReloadApplied()
    {
        var signature = Signature(StatePath);
        if (signature == _appliedSignature) return;
        _appliedSignature = signature;
        _applied = AppliedSetup.Load(StatePath);
    }

    /// Rewrites only the `controller` block of the file on disk, keeping everything else exactly as
    /// the user left it. When the file is an edit made on this device, [editedFrom] is what it was
    /// made to, and [LocalEdit] is raised before the write.
    private string? StampIdentity(ControllerIdentity identity, byte[] current, string description, EditedFrom? editedFrom)
    {
        try
        {
            var root = JsonNode.Parse(Encoding.UTF8.GetString(Canonical.Bytes(current))) as JsonObject;
            if (root is null) return "The setup file is not a JSON object.";
            root["controller"] = IdentityJson(identity, root["controller"] as JsonObject);
            var stamped = ControllerDocument.FromText(root.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));
            // The previous bytes go into the ledger before the file changes: a stamp that lost the
            // document it stamped would leave a merge with no base.
            if (Document is not null) Ledger.Remember(Document);
            else Ledger.Remember(ControllerDocument.FromRaw(current));
            if (editedFrom is not null) LocalEdit?.Invoke(editedFrom.Bytes, stamped);
            AtomicWrite.Bytes(Path, stamped.Bytes);
            return null;
        }
        catch (Exception error)
        {
            return $"Could not write {Path}: {error.Message} ({description})";
        }
    }

    internal static JsonObject IdentityJson(ControllerIdentity identity, JsonObject? original = null)
    {
        var block = original?.DeepClone() as JsonObject ?? new JsonObject();
        block["id"] = identity.Id;
        block["revision"] = identity.RevisionNumber;
        block["updatedAt"] = identity.UpdatedAt ?? DateTimeOffset.UtcNow.ToString("o");
        block["source"] = identity.Source ?? ControllerIdentity.ThisSource;
        if (identity.Name is not null) block["name"] = identity.Name;
        if (identity.Device is not null) block["device"] = identity.Device;
        block["lineage"] = new JsonArray(identity.Lineage.Select(hash => (JsonNode)hash!).ToArray());
        return block;
    }

    /// Applies a document this device was handed by a machine, or the result of a merge.
    ///
    /// The document being replaced goes into the ledger first, because adopting is the one action
    /// here that can lose something a person typed.
    public string? Apply(ControllerDocument document, string description = "applied")
    {
        lock (_gate) return ApplyCore(document, description);
    }

    /// Refuses a fetched revision if an edit or another peer changed the local head while the
    /// network request was in flight. The next poll compares the new branches from scratch.
    public string? ApplyIfCurrent(ControllerDocument document, string expectedHash, string description)
    {
        lock (_gate)
        {
            ReloadIfChanged();
            if (Document?.Hash != expectedHash) return "The local setup changed while reading the peer. Nothing was replaced; compare again.";
            return ApplyCore(document, description);
        }
    }

    private string? ApplyCore(ControllerDocument document, string description)
    {
        try
        {
            var json = Value.Parse(document.Text);
            if (!json.IsObject) return "The setup is not a JSON object.";
            var problems = document.Decoded.Problems();
            if (problems.Count > 0) return string.Join(" ", problems);
            if (Document is not null) Ledger.Remember(Document);
            // Kept before it is written, so that another process reading the file in the moment
            // before it is recorded as applied still knows it for an adoption (see [WasAdopted]).
            Ledger.Remember(document);
            AtomicWrite.Bytes(Path, document.Bytes);
            RecordApplied(document);
            _lastSignature = null;
            Load();
            return null;
        }
        catch (Exception error)
        {
            return $"Could not write {Path}: {error.Message} ({description})";
        }
    }

    /// Applies an edit: the same setup, the next revision, this document in the lineage.
    public (ControllerDocument? Document, string? Problem) ApplyEdit(string editedText, string description = "edit")
    {
        lock (_gate) return ApplyEditCore(editedText, description);
    }

    /// Applies an edit only while the setup is still [expectedHash], the document its text was read
    /// from (null for none). Saved over a later one, the text would put back whatever the earlier
    /// document held, a value reverted since included, and an edit approves what it changes.
    public (ControllerDocument? Document, string? Problem) ApplyEditIfCurrent(string editedText, string? expectedHash, string description)
    {
        lock (_gate)
        {
            if (Signature(Path) != _lastSignature) LoadCore();
            if (Document?.Hash != expectedHash) return (null, MovedOn);
            return ApplyEditCore(editedText, description);
        }
    }

    /// Why an edit of an earlier document was not applied.
    public const string MovedOn =
        "The setup changed since this edit started from it. Nothing was applied: start again from the setup as it is now.";

    private (ControllerDocument? Document, string? Problem) ApplyEditCore(string editedText, string description)
    {
        ControllerDocument candidate;
        try
        {
            candidate = ControllerDocument.FromText(editedText);
        }
        catch (Canonical.NotUtf8)
        {
            return (null, "That is not valid UTF-8.");
        }

        var json = Value.Parse(candidate.Text);
        if (!json.IsObject) return (null, "That is not a JSON object.");
        var decoded = ControllerConfig.From(json);
        var problems = decoded.Problems();
        if (problems.Count > 0) return (null, string.Join(" ", problems));

        var parentHash = Document?.Hash;
        var identity = Document?.Identity ?? decoded.Identity;
        var next = identity.HasIdentity && parentHash is not null
            ? identity.Next(parentHash, DeviceName)
            : ControllerIdentity.NewSetup(decoded.Identity.Name, DeviceName);

        JsonObject root;
        try
        {
            root = JsonNode.Parse(candidate.Text) as JsonObject ?? new JsonObject();
        }
        catch (JsonException error)
        {
            return (null, $"That is not valid JSON: {error.Message}");
        }
        root["controller"] = IdentityJson(next, root["controller"] as JsonObject);
        var stamped = ControllerDocument.FromText(root.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));

        LocalEdit?.Invoke(Document?.Bytes, stamped);
        var problem = Apply(stamped, description);
        return problem is null ? (Document, null) : (null, problem);
    }

    /// Writes the example into empty space, and only into empty space.
    public string? WriteExample()
    {
        if (File.Exists(Path)) return $"There is already a setup file at {Path}.";
        try
        {
            var example = ControllerDocument.FromText(SetupTemplates.Example);
            LocalEdit?.Invoke(null, example);
            AtomicWrite.Bytes(Path, example.Bytes);
            _lastSignature = null;
            Load();
            return null;
        }
        catch (Exception error)
        {
            return $"Could not write {Path}: {error.Message}";
        }
    }

    private void RecordApplied(ControllerDocument document)
    {
        var applied = new AppliedSetup
        {
            Hash = document.Hash,
            Id = document.Identity.Id,
            Revision = document.Identity.RevisionNumber,
            Lineage = document.Identity.Lineage,
        };
        ReloadApplied();
        if (applied == _applied) return;
        _applied = applied;
        try
        {
            applied.Save(StatePath);
            _appliedSignature = Signature(StatePath);
        }
        catch (Exception)
        {
            // Losing the pointer costs the ability to tell a hand edit from this app's own write
            // once, on the next load. It is not worth refusing a document over.
        }
    }

    /// The directory is watched rather than the file. Editors save by writing a temporary file and
    /// renaming it over the top, which replaces the inode, and a handle on the old one would never
    /// see another thing.
    private void StartWatching()
    {
        try
        {
            var directory = System.IO.Path.GetDirectoryName(Path);
            if (string.IsNullOrEmpty(directory) || !Directory.Exists(directory)) return;
            var watcher = new FileSystemWatcher(directory)
            {
                NotifyFilter = NotifyFilters.FileName | NotifyFilters.LastWrite | NotifyFilters.Size,
                EnableRaisingEvents = true,
            };
            watcher.Changed += (_, _) => ReloadIfChanged();
            watcher.Created += (_, _) => ReloadIfChanged();
            watcher.Renamed += (_, _) => ReloadIfChanged();
            watcher.Deleted += (_, _) => ReloadIfChanged();
            _watcher = watcher;
        }
        catch (Exception)
        {
            // Watching is an optimisation. Without it the poll still notices, one interval later.
        }
    }

    /// Modification time and size together. Either alone misses an edit, and hashing the whole file
    /// on every poll would be work for nothing.
    private static string? Signature(string path)
    {
        try
        {
            var info = new FileInfo(path);
            if (!info.Exists) return null;
            return $"{info.LastWriteTimeUtc.Ticks}:{info.Length}";
        }
        catch (Exception)
        {
            return null;
        }
    }

    public void Dispose()
    {
        _watcher?.Dispose();
        _watcher = null;
    }
}

/// What this device last applied. Remembered so that a file that changed underneath can be told
/// apart from one this app wrote itself.
public sealed record AppliedSetup
{
    public string? Hash { get; init; }
    public string? Id { get; init; }
    public long Revision { get; init; }
    public IReadOnlyList<string> Lineage { get; init; } = Array.Empty<string>();

    public bool MatchesIdentityOf(ControllerIdentity identity) =>
        Id == identity.Id
        && Revision == identity.RevisionNumber
        && Lineage.SequenceEqual(identity.Lineage, StringComparer.Ordinal);

    public static AppliedSetup Load(string path)
    {
        try
        {
            if (!File.Exists(path)) return new AppliedSetup();
            var json = Value.Parse(File.ReadAllText(path));
            return new AppliedSetup
            {
                Hash = json["hash"].AsText(),
                Id = json["id"].AsText(),
                Revision = json["revision"].AsLong() ?? 0,
                Lineage = json["lineage"].AsStringList(),
            };
        }
        catch (Exception)
        {
            return new AppliedSetup();
        }
    }

    public void Save(string path) => AtomicWrite.Text(path, JsonSerializer.Serialize(new
    {
        hash = Hash,
        id = Id,
        revision = Revision,
        lineage = Lineage,
    }, new JsonSerializerOptions { WriteIndented = true }));

    public bool Equals(AppliedSetup? other) =>
        other is not null && Hash == other.Hash && Id == other.Id && Revision == other.Revision
        && Lineage.SequenceEqual(other.Lineage, StringComparer.Ordinal);

    public override int GetHashCode() => HashCode.Combine(Hash, Id, Revision, Lineage.Count);
}
