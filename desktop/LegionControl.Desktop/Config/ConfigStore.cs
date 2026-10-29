using System.Globalization;
using System.Text;
using System.Text.Json;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// Holds whatever the setup document last said, and notices when it changes.
///
/// Two rules everything above depends on. A file that is no document at all (contract section 12)
/// never replaces one that was: a half-saved file is the normal state of a file being edited, and
/// throwing the machines away every time an editor writes a partial document would make the app
/// flicker through its own setup page. A document that is one is applied even when section 9.7
/// finds errors in it (applied intake): the errors are shown, nothing is published until they are
/// fixed, and a machine an error is inside is left out of what this device dials and runs. And
/// every document this device applies is remembered by hash, because a merge needs the common
/// ancestor and a person who adopted the wrong thing needs the previous one back.
///
/// There is no authority here. Every device is a peer: this one edits, publishes, and fast-forwards
/// to what another peer wrote, all through the same lineage rule.
public sealed class ConfigStore : IDisposable
{
    public string Path { get; }
    /// Whether this store writes nothing at all (see the constructor).
    public bool ReadOnly { get; }
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
    ///
    /// [readOnly] is for a run that reads and changes nothing, as a smoke run does: the file is
    /// applied in memory exactly as it is, an identity it lacks or a hand edit it holds is said
    /// rather than recorded, and nothing is written, here or in the ledger.
    public ConfigStore(
        string? path = null,
        string? statePath = null,
        SetupLedger? ledger = null,
        string? deviceName = null,
        bool watch = true,
        Action<byte[]?, ControllerDocument>? localEdit = null,
        bool readOnly = false)
    {
        ReadOnly = readOnly;
        Path = path ?? AppPaths.ConfigFile;
        StatePath = statePath ?? System.IO.Path.Combine(AppPaths.Home, "setup-state.json");
        Ledger = ledger ?? new SetupLedger();
        DeviceName = deviceName ?? Environment.MachineName;
        if (localEdit is not null) LocalEdit += localEdit;
        Load();
        if (watch) StartWatching();
    }

    /// The machines of the applied document, without any that a section 9.7 error is inside, or
    /// null when no document was ever applied.
    public ControllerConfig? Config { get; private set; }
    /// Why the file on disk was not applied, or what is wrong with the document that was, or null
    /// when it read cleanly.
    public string? Problem { get; private set; }
    /// What section 9.7 said about the applied document. A document with errors is applied, and
    /// publishes nothing until they are fixed.
    public Acceptance? Acceptance { get; private set; }
    /// Worth saying without refusing the document: helpers that may be asleep, machines nothing can
    /// wake, a helper at another site.
    public IReadOnlyList<string> Warnings { get; private set; } = Array.Empty<string>();
    /// Whether the file holds something other than the applied document that could not be applied
    /// yet: a file half saved, or an edit that cannot be given a revision. Contract section 9.5
    /// takes no part in reconciliation for such a device (rule 2), and a fetch decided before it
    /// never lands over it.
    public bool IsPendingEdit { get; private set; }
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
    /// `controller` block, by a file with no identity that is given one here while no setup is
    /// applied, and by
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
        // Taken before the read, so a write that lands in between is read again next time, and
        // kept only once the read succeeded: a file that could not be read this once, held open by
        // an editor saving it or a virus scanner, is read again on the next check rather than
        // taken for read until it changes again.
        var signature = Signature(Path);

        byte[] data;
        try
        {
            data = File.ReadAllBytes(Path);
            _lastSignature = signature;
        }
        catch (Exception)
        {
            _lastSignature = null;
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
                Acceptance = null;
                IsPendingEdit = false;
                RawBytes = null;
                Changed?.Invoke();
            }
            return;
        }

        IsMissing = false;
        // Section 12 first: bytes that are no document at all are not applied, and the document
        // before them stays. Anything section 12 accepts is applied, errors and all.
        var acceptance = DocumentAcceptance.Check(data, Intake.Applied);
        if (!acceptance.Keep)
        {
            Problem = !Canonical.IsValidUtf8(data)
                ? "The setup file is not valid UTF-8, so it has no canonical form and no hash."
                : Document is null
                    ? $"The setup file is not a setup document. {acceptance.Sentence}"
                    : $"The setup file is not a setup document yet, so the one before it stays applied. {acceptance.Sentence}";
            IsPendingEdit = Document is not null;
            return;
        }
        var document = ControllerDocument.FromRaw(data);
        var decoded = ControllerConfig.From(Value.Parse(document.Text));
        var tree = DocumentJson.Read(document.Text, out var repeated);

        // The window and the command line both adopt documents into this file. What the other one
        // applied counts as applied here too, or its Take theirs would read as an edit of the
        // revision it replaced.
        ReloadApplied();

        // A document with a member written twice cannot be given a revision: which copy is the
        // edit is exactly what devices disagree about. One a process here adopted, or the one
        // applied, is what it was; anything else is an edit that waits, unapplied, until the name
        // is written once.
        if (repeated is not null && _applied.Hash != document.Hash && !WasAdopted(document))
        {
            Problem = $"{acceptance.Sentence} The edit is applied once that is fixed; until then the setup before it stays.";
            IsPendingEdit = Document is not null;
            return;
        }

        // A run that writes nothing applies the file as it is, and says what the next run that may
        // write will make of it.
        string? unrecorded = null;
        if (ReadOnly && repeated is null)
        {
            if (DocumentAcceptance.NeedsIdentity(tree))
                unrecorded = "The setup file has no controller block yet. The next run that may change something gives it one, as a setup of its own.";
            else if (IsHandEdit(document))
                unrecorded = "The setup file was edited by hand since this device applied it. The next run that may change something records the edit as the next revision.";
        }

        // A document that carries no identity of its own becomes a setup here and now, before it is
        // applied or published. Without one, two devices holding the same hand-written file would
        // look like two different setups to every machine they reach.
        if (!ReadOnly && repeated is null && DocumentAcceptance.NeedsIdentity(tree))
        {
            var assigned = ControllerIdentity.NewSetup(decoded.Identity.Name, DeviceName);
            // A file with no identity becomes a setup of its own, so it is no edit of the one
            // applied here: contract section 9.9 lets only a hand edit that keeps its controller
            // block approve. On a device that applies none it is the person's first setup, and
            // approves what it holds. Over one that is applied it approves nothing, and what it
            // changed waits for review: an old copy with the block taken out would otherwise put
            // back a command reverted since and approve it. One that a process here adopted is
            // stamped onto exactly itself, so nothing it carries counts as changed.
            var editedFrom = WasAdopted(document) ? new EditedFrom(document.Bytes)
                : _applied.Hash is null ? new EditedFrom(null)
                : null;
            if (StampIdentity(assigned, data, "assigned-setup-id", editedFrom) is { } stampProblem)
            {
                Problem = stampProblem;
                IsPendingEdit = Document is not null;
                return;
            }
            AssignedSetupId = assigned.Id;
            Load();
            return;
        }

        // A file somebody edited by hand carries the identity of the revision it was edited from.
        // That is an edit like any other: it gets the next revision, and the document it came from
        // goes into the lineage so every machine holding that document fast-forwards to this one.
        if (!ReadOnly && repeated is null && IsHandEdit(document))
        {
            var next = document.Identity.Next(_applied.Hash!, DeviceName);
            if (StampIdentity(next, data, "hand-edit", AppliedEditedFrom()) is { } stampProblem)
            {
                Problem = stampProblem;
                IsPendingEdit = Document is not null;
                return;
            }
            Load();
            return;
        }

        Problem = acceptance.Errors.Count == 0
            ? null
            : $"{acceptance.Sentence} Nothing is published until this is fixed"
              + (acceptance.BrokenMachines.Count > 0 ? ", and a machine it is inside is not reached or run from here." : ".");
        Acceptance = acceptance;
        Warnings = acceptance.Warnings.Select(warning => warning.Message)
            .Concat(unrecorded is null ? Array.Empty<string>() : new[] { unrecorded })
            .ToList();
        IsPendingEdit = false;
        RawBytes = data;
        var usable = decoded.WithoutMachinesAt(acceptance.BrokenMachines);
        var unchanged = Config == usable && Document?.Hash == document.Hash;
        Document = document;
        if (!ReadOnly)
        {
            Ledger.Remember(document);
            RecordApplied(document);
        }
        if (unchanged) return;
        Config = usable;
        Changed?.Invoke();
    }

    /// Whether [document] is a hand edit of the one applied here: it carries that one's identity
    /// and is not it.
    ///
    /// Two branches edited from the same parent carry the same identity, so a sibling that a
    /// process here adopted looks exactly like a hand edit of the one it replaced, and would
    /// approve every launch value it changed. It is told apart by being kept, which it already is
    /// when the file changes.
    private bool IsHandEdit(ControllerDocument document) =>
        _applied.Hash is { } applied
        && applied != document.Hash
        && _applied.MatchesIdentityOf(document.Identity)
        && !WasAdopted(document);

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
        // As the setup file: only a read that succeeded counts as having read it.
        if (AppliedSetup.TryLoad(StatePath) is not { } applied) return;
        _appliedSignature = signature;
        _applied = applied;
    }

    /// Rewrites only the `controller` block of the file on disk, keeping everything else exactly as
    /// the user left it. When the file is an edit made on this device, [editedFrom] is what it was
    /// made to, and [LocalEdit] is raised before the write.
    private string? StampIdentity(ControllerIdentity identity, byte[] current, string description, EditedFrom? editedFrom)
    {
        try
        {
            if (DocumentJson.Parse(Encoding.UTF8.GetString(Canonical.Bytes(current))) is not DocumentObject root)
                return "The setup file is not a JSON object.";
            root.Set("controller", IdentityBlock(identity, root["controller"] as DocumentObject));
            var stamped = ControllerDocument.FromText(DocumentJson.Print(root));
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

    /// The `controller` block for [identity], keeping every other key [original] has where it
    /// stands, unknown ones included.
    internal static DocumentObject IdentityBlock(ControllerIdentity identity, DocumentObject? original = null)
    {
        var block = original?.Clone() as DocumentObject ?? new DocumentObject();
        block.Set("id", new DocumentString(identity.Id ?? ""));
        block.Set("revision", new DocumentNumber(identity.RevisionNumber.ToString(CultureInfo.InvariantCulture)));
        block.Set("updatedAt", new DocumentString(identity.UpdatedAt ?? DateTimeOffset.UtcNow.ToString("o")));
        block.Set("source", new DocumentString(identity.Source ?? ControllerIdentity.ThisSource));
        if (identity.Name is not null) block.Set("name", new DocumentString(identity.Name));
        if (identity.Device is not null) block.Set("device", new DocumentString(identity.Device));
        block.Set("lineage", new DocumentArray(identity.Lineage.Select(hash => new DocumentString(hash))));
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

    /// Applies [document] only while the setup here is still [expectedHead], the document the
    /// decision to apply it was made from (null for none): adopting is a compare-and-swap
    /// (contract section 9.5). The head is read again from the file at the moment of writing, so an
    /// edit saved while a peer was being read, or a file half way through being saved, is never
    /// replaced by bytes that were chosen against the document before it. The next reading decides
    /// from the new head.
    ///
    /// [expectedHash], when given, binds the write to the hash that was verified: the bytes stored
    /// are exactly the ones that hash to it, canonicalised and never re-serialised.
    public ApplyResult ApplyIfCurrent(ControllerDocument document, string? expectedHead, string description, string? expectedHash = null)
    {
        lock (_gate)
        {
            if (expectedHash is not null && document.Hash != expectedHash)
                return new ApplyResult.Refused($"These bytes hash to {document.Hash}, not to the {expectedHash} that was checked. Nothing was replaced ({description}).");
            if (Signature(Path) != _lastSignature) LoadCore();
            if (IsPendingEdit || DiskHash() != Document?.Hash || Document?.Hash != expectedHead)
                return new ApplyResult.HeadMoved(Document?.Hash);
            return ApplyCore(document, description) is { } problem
                ? new ApplyResult.Refused(problem)
                : new ApplyResult.Applied(document.Hash);
        }
    }

    /// The applied hash read again from storage now: what a fetch decided against has to still be
    /// the head when its bytes arrive (contract section 9.5, admission row a). Null while the file
    /// holds anything other than the applied document, so an edit being saved always reads as a
    /// head that moved.
    public string? CurrentHead()
    {
        lock (_gate)
        {
            if (Signature(Path) != _lastSignature) LoadCore();
            if (IsPendingEdit || Document is not { } document) return null;
            return DiskHash() == document.Hash ? document.Hash : null;
        }
    }

    /// Why a document was not applied over the setup it was chosen against.
    public const string HeadMovedSentence =
        "The setup here changed while this was being decided. Nothing was replaced; the next reading compares again.";

    /// The hash of what the file holds now, read from the disk rather than from what this store
    /// last loaded; null when there is no file or it is no document.
    private string? DiskHash()
    {
        try
        {
            return File.Exists(Path) ? Canonical.Hash(File.ReadAllBytes(Path)) : null;
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// Why a read-only store applied nothing.
    public const string ReadOnlySentence = "This run changes nothing on disk, so nothing was written.";

    private string? ApplyCore(ControllerDocument document, string description)
    {
        if (ReadOnly) return ReadOnlySentence;
        try
        {
            // What is adopted came from a peer, a kept revision or a merge, which contract section
            // 9.7 holds to peer intake: nothing with an error replaces the setup here.
            if (DocumentAcceptance.Check(document.Bytes, Intake.Peer) is { Valid: false } refused) return refused.Sentence;
            if (Document is not null) Ledger.Remember(Document);
            // Kept before it is written, so that another process reading the file in the moment
            // before it is recorded as applied still knows it for an adoption (see [WasAdopted]).
            Ledger.Remember(document);
            // The ledger never says why it could not keep a document. Without that copy, another
            // process here would take a sibling branch with the same identity for a hand edit of
            // what is applied, and one with no identity for a person's first setup where none is
            // applied, and either would approve every launch value it changed, so neither is
            // written at all.
            ReloadApplied();
            if (!WasAdopted(document) && _applied.Hash != document.Hash
                && (!document.Identity.HasIdentity || _applied.MatchesIdentityOf(document.Identity)))
            {
                return $"Could not keep a copy of this setup in {Ledger.Directory}, which is how the window and the command line tell it from a hand edit. Nothing was replaced ({description}).";
            }
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
        // A document typed or pasted here is held to section 9.7 under local intake: one without an
        // identity is checked as the setup of its own it is about to become.
        var acceptance = DocumentAcceptance.Check(editedText, Intake.Local);
        if (!acceptance.Valid) return (null, acceptance.Sentence);
        var candidate = ControllerDocument.FromText(editedText);
        var decoded = candidate.Decoded;

        var parentHash = Document?.Hash;
        var identity = Document?.Identity ?? decoded.Identity;
        var next = identity.HasIdentity && parentHash is not null
            ? identity.Next(parentHash, DeviceName)
            : ControllerIdentity.NewSetup(decoded.Identity.Name, DeviceName);

        var root = (DocumentObject)DocumentJson.Parse(candidate.Text);
        root.Set("controller", IdentityBlock(next, root["controller"] as DocumentObject));
        var stamped = ControllerDocument.FromText(DocumentJson.Print(root));

        LocalEdit?.Invoke(Document?.Bytes, stamped);
        var problem = Apply(stamped, description);
        return problem is null ? (Document, null) : (null, problem);
    }

    /// Writes the example into empty space, and only into empty space.
    public string? WriteExample()
    {
        if (ReadOnly) return ReadOnlySentence;
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

/// What came of applying a document over the setup it was chosen against.
public abstract record ApplyResult
{
    /// It is the setup here now.
    public sealed record Applied(string Hash) : ApplyResult;
    /// The setup here is no longer the one the document was chosen against, so nothing was
    /// written. [Current] is the hash it holds now.
    public sealed record HeadMoved(string? Current) : ApplyResult;
    /// It could not be applied, in the words of what has to change.
    public sealed record Refused(string Sentence) : ApplyResult;

    /// What a person is told when it did not apply, or null when it did.
    public string? Problem => this switch
    {
        HeadMoved => ConfigStore.HeadMovedSentence,
        Refused refused => refused.Sentence,
        _ => null,
    };
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

    public static AppliedSetup Load(string path) => TryLoad(path) ?? new AppliedSetup();

    /// What [path] says was applied, nothing when there is no file, or null when it is there but
    /// could not be read this time.
    public static AppliedSetup? TryLoad(string path)
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
            return null;
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
