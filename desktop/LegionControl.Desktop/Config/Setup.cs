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

    private static readonly Regex HashGrammar = new("^[0-9a-f]{64}$", RegexOptions.Compiled);
    private static readonly Regex IdGrammar = new(@"^[A-Za-z0-9][A-Za-z0-9._:@/\\~=+-]*$", RegexOptions.Compiled);

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

    /// What is wrong with this block, in the words of the thing that has to change. An empty list
    /// means the editor may apply and publish.
    public IReadOnlyList<string> Problems()
    {
        var problems = new List<string>();
        if (Id is not null && !IdGrammar.IsMatch(Id))
        {
            problems.Add($"The setup id \"{Id}\" is not a plain token. Use letters, digits and . _ : @ / ~ = + -");
        }
        if (Revision is { } revision && revision < 0) problems.Add("The setup revision cannot be negative.");
        if (Lineage.Count > LineageLimit)
        {
            problems.Add($"The setup lineage carries {Lineage.Count} hashes; at most {LineageLimit} are allowed.");
        }
        foreach (var hash in Lineage)
        {
            if (!HashGrammar.IsMatch(hash)) problems.Add($"\"{hash}\" in the setup lineage is not a sha256 hash.");
        }
        if (Lineage.Distinct(StringComparer.Ordinal).Count() != Lineage.Count)
        {
            problems.Add("The setup lineage repeats a hash.");
        }
        return problems;
    }

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

    /// The identity a merge of two branches gets: one more than the higher revision, and both
    /// heads at the front of a lineage that keeps as much of both ancestries as it can.
    public static ControllerIdentity Merged(
        ControllerIdentity mine, string myHash,
        ControllerIdentity theirs, string theirHash,
        string? device)
    {
        var parents = new List<string> { myHash, theirHash };
        // Interleaved rather than concatenated: with a cap of 32 the recent history of both sides
        // is worth more than all of one side's and none of the other's.
        var mineRest = mine.Lineage.ToList();
        var theirsRest = theirs.Lineage.ToList();
        for (var index = 0; index < Math.Max(mineRest.Count, theirsRest.Count); index += 1)
        {
            if (index < mineRest.Count) parents.Add(mineRest[index]);
            if (index < theirsRest.Count) parents.Add(theirsRest[index]);
        }
        return mine with
        {
            Id = mine.Id ?? theirs.Id,
            Revision = Math.Max(mine.RevisionNumber, theirs.RevisionNumber) + 1,
            Source = ThisSource,
            Device = device ?? mine.Device,
            UpdatedAt = DateTimeOffset.UtcNow.ToString("o"),
            Lineage = parents.Distinct(StringComparer.Ordinal).Take(LineageLimit).ToList(),
        };
    }
}

/// How two copies of a setup are related.
public enum Descent
{
    /// The same bytes.
    Same,
    /// The other side holds nothing, or holds a copy with no identity at all.
    TheyHaveNothing,
    /// The other side holds an ancestor of mine: safe to publish.
    TheyAreBehind,
    /// I hold an ancestor of theirs: safe to adopt.
    IAmBehind,
    /// Same setup, neither descends from the other. A person decides.
    Diverged,
    /// Two different setups. Neither is an older copy of the other.
    DifferentSetup,
    /// Not enough was read to say. Never treated as any of the others.
    Unknown,
}

/// The one rule that decides whether anything moves on its own.
///
/// Revision numbers deliberately do not appear. Two peers editing revision 5 both produce a
/// revision 6, and taking the higher number would throw one of them away without telling anybody.
/// Ancestry is what distinguishes "behind" from "different", so ancestry is what is checked.
public static class SetupLineage
{
    public static Descent Decide(
        string mineHash, ControllerIdentity mine,
        string? theirsHash, ControllerIdentity? theirs,
        IReadOnlyList<string>? theirLineage = null)
    {
        if (theirsHash is null) return Descent.TheyHaveNothing;
        if (theirsHash == mineHash) return Descent.Same;
        if (theirs is null) return Descent.Unknown;
        if (!theirs.HasIdentity) return Descent.TheyHaveNothing;
        if (mine.HasIdentity && theirs.Id != mine.Id) return Descent.DifferentSetup;

        if (mine.Lineage.Contains(theirsHash, StringComparer.Ordinal)) return Descent.TheyAreBehind;
        var lineage = theirLineage ?? theirs.Lineage;
        if (lineage.Contains(mineHash, StringComparer.Ordinal)) return Descent.IAmBehind;
        return Descent.Diverged;
    }

    /// What can be said from a status reply alone, which carries a hash but no lineage.
    ///
    /// Deliberately incomplete: everything except "the same", "they hold nothing" and "they are
    /// behind" needs the machine's own lineage, which costs one extra round trip and is only spent
    /// when it is the only way to tell "behind" from "diverged".
    public static Descent DecideFromStatus(string mineHash, ControllerIdentity mine, string? theirsHash)
    {
        if (theirsHash is null) return Descent.TheyHaveNothing;
        if (theirsHash == mineHash) return Descent.Same;
        if (mine.Lineage.Contains(theirsHash, StringComparer.Ordinal)) return Descent.TheyAreBehind;
        return Descent.Unknown;
    }
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

    /// The newest hash the two lineages have in common, and whose bytes are still here. This is the
    /// base a three-way merge needs.
    public string? CommonBase(IReadOnlyList<string> mine, IReadOnlyList<string> theirs)
    {
        var theirSet = theirs.ToHashSet(StringComparer.Ordinal);
        foreach (var hash in mine)
        {
            if (theirSet.Contains(hash) && Has(hash)) return hash;
        }
        return null;
    }

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
