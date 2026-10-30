using System.Text.RegularExpressions;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// Who a setup is, how far along it is, and where it came from.
///
/// The identity is the setup's, not a device's. Every peer that edits this setup writes the same
/// `controller.id`; what tells two copies apart is the revision and, where revisions cannot, the
/// lineage. A hash proves two documents differ and says nothing about which is newer; ancestry says
/// which is newer, and that is the whole difference between fast-forwarding and losing somebody's
/// evening of work.
public sealed record ControllerIdentity
{
    /// The setup id. Stable for the life of the setup, shared by every device.
    public string? Id { get; init; }
    /// Display only. The name of the setup, not of the device that wrote it.
    public string? Name { get; init; }
    /// Monotonic within one id: one more than the highest revision this document descends from.
    public long? Revision { get; init; }
    /// Display only, never compared: clocks disagree.
    public string? UpdatedAt { get; init; }
    /// Which kind of client wrote it: mac | desktop | phone | cli | legacy.
    public string? Source { get; init; }
    /// The human name of the device that wrote it. Display only.
    public string? Device { get; init; }
    /// The hashes of the documents this one descends from, newest first, at most 32.
    public IReadOnlyList<string> Lineage { get; init; } = Array.Empty<string>();

    public const int LineageLimit = 32;

    /// The kind this client is, on the wire. Fixed by the contract: never a platform name.
    public const string ThisSource = "desktop";

    private static readonly Regex HashGrammar = new(@"\A[0-9a-f]{64}\z", RegexOptions.Compiled);

    /// Whether a value is a sha256 the way the contract writes one: 64 lower-case hex digits and
    /// nothing else, not even a final newline.
    public static bool IsHash(string? value) => CommandSurface.Matches(HashGrammar, value);

    public static ControllerIdentity From(Value json)
    {
        if (!json.IsObject) return new ControllerIdentity();
        return new ControllerIdentity
        {
            Id = json["id"].AsText(),
            Name = json["name"].AsText(),
            Revision = json["revision"].AsLong(),
            UpdatedAt = json["updatedAt"].AsText(),
            Source = json["source"].AsText(),
            Device = json["device"].AsText(),
            Lineage = json["lineage"].AsStringList(),
        };
    }

    public long RevisionNumber => Revision ?? 0;

    public bool HasIdentity => !string.IsNullOrWhiteSpace(Id);

    /// A fresh setup, for a document that carries no identity: a hand written file, or the example.
    public static ControllerIdentity NewSetup(string? name, string? device) => new()
    {
        Id = "setup-" + Guid.NewGuid().ToString("d"),
        Name = name,
        Revision = 1,
        Source = ThisSource,
        Device = device,
        UpdatedAt = DateTimeOffset.UtcNow.ToString("o"),
        Lineage = Array.Empty<string>(),
    };

    /// The identity an edit of this document gets: same setup, next revision, this document's hash
    /// at the head of the lineage.
    public ControllerIdentity Next(string parentHash, string? device) => this with
    {
        Revision = RevisionNumber + 1,
        Source = ThisSource,
        Device = device ?? Device,
        UpdatedAt = DateTimeOffset.UtcNow.ToString("o"),
        Lineage = new[] { parentHash }
            .Concat(Lineage.Where(hash => hash != parentHash))
            .Take(LineageLimit)
            .ToList(),
    };
}

/// Every document this device has applied, kept by hash.
///
/// Two things need this. A merge needs the common ancestor's bytes to tell "they changed it" from
/// "I removed it", and a person who adopted something by mistake needs the previous document back.
/// Thirty is enough for both and small enough to never be noticed.
public sealed class SetupLedger(string? directory = null)
{
    public string Directory { get; } = directory ?? AppPaths.RevisionsDirectory;

    public const int Keep = 30;

    public string PathFor(string hash) => Path.Combine(Directory, $"{hash}.json");

    /// Records a document. Writing the same document twice is not an error and not a second copy.
    public void Remember(ControllerDocument document)
    {
        try
        {
            System.IO.Directory.CreateDirectory(Directory);
            var path = PathFor(document.Hash);
            if (!File.Exists(path)) AtomicWrite.Bytes(path, document.Bytes);
            Prune();
        }
        catch (Exception)
        {
            // A ledger that cannot be written costs a merge its base document, which the merge
            // already copes with by asking about every differing entry. It is not worth failing an
            // edit over.
        }
    }

    public ControllerDocument? Read(string hash)
    {
        try
        {
            var path = PathFor(hash);
            if (!File.Exists(path)) return null;
            var document = ControllerDocument.FromRaw(File.ReadAllBytes(path));
            // A file whose contents no longer hash to its name is not the document that was asked
            // for, whatever it is.
            return document.Hash == hash ? document : null;
        }
        catch (Exception)
        {
            return null;
        }
    }

    public bool Has(string hash) => File.Exists(PathFor(hash));

    public IReadOnlyList<string> Hashes()
    {
        try
        {
            if (!System.IO.Directory.Exists(Directory)) return Array.Empty<string>();
            return System.IO.Directory.EnumerateFiles(Directory, "*.json")
                .Select(Path.GetFileNameWithoutExtension)
                .Where(name => name is { Length: 64 })
                .Select(name => name!)
                .ToList();
        }
        catch (Exception)
        {
            return Array.Empty<string>();
        }
    }

    /// The base a three-way merge needs (contract section 9.8): the newest hash of mine's lineage
    /// that is also in theirs', whose bytes are still here and still hash to it. Neither head is
    /// ever a base.
    public string? CommonBase(string mineHash, string theirsHash, IReadOnlyList<string> mine, IReadOnlyList<string> theirs) =>
        SetupSync.CommonBase(mineHash, theirsHash, mine, theirs, hash => Read(hash)?.Text);

    private void Prune()
    {
        try
        {
            var files = System.IO.Directory.EnumerateFiles(Directory, "*.json")
                .Select(file => new FileInfo(file))
                .OrderByDescending(file => file.LastWriteTimeUtc)
                .Skip(Keep)
                .ToList();
            foreach (var file in files) file.Delete();
        }
        catch (Exception)
        {
        }
    }
}
