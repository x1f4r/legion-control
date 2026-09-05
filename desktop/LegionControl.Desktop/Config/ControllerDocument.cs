using System.Text;

namespace LegionControl.Desktop.Config;

/// The controller document as a machine gets it: the bytes, and the sha256 the two sides compare.
///
/// The bytes are canonical, not raw. The agent stores what it is given in canonical form and hashes
/// what it stored, so a file saved without a final newline would otherwise hash one way here and
/// another way on every machine, and this app would push the same document forever without ever
/// agreeing with anybody. There is one canonical form, in [Canonical], and every implementation
/// uses it.
public sealed record ControllerDocument
{
    public byte[] Bytes { get; }
    public string Hash { get; }
    public ControllerIdentity Identity { get; init; }

    private ControllerDocument(byte[] bytes, string hash, ControllerIdentity identity)
    {
        Bytes = bytes;
        Hash = hash;
        Identity = identity;
    }

    /// Builds a document from whatever was read off a disk or a machine.
    ///
    /// Throws [Canonical.NotUtf8] for bytes that are not a document at all. Callers turn that into
    /// a sentence rather than a hash: a document nobody can decode has no canonical form, and
    /// hashing its replacement characters would make two sides agree about rubbish.
    public static ControllerDocument FromRaw(byte[] raw, ControllerIdentity? identity = null)
    {
        var canonical = Canonical.Bytes(raw);
        var text = Encoding.UTF8.GetString(canonical);
        var resolved = identity ?? ControllerIdentity.From(Contract.Value.Parse(text)["controller"]);
        return new ControllerDocument(canonical, Canonical.Sha256Hex(canonical), resolved);
    }

    public static ControllerDocument FromText(string text, ControllerIdentity? identity = null) =>
        FromRaw(Encoding.UTF8.GetBytes(text), identity);

    public string Text => Encoding.UTF8.GetString(Bytes);

    /// The decoded document, for the readers that need the machines rather than the bytes.
    public ControllerConfig Decoded => ControllerConfig.From(Contract.Value.Parse(Text));
}

/// How far the setup has got to one machine, as its section draws it.
public abstract record SetupSharing
{
    /// The machine reports the hash this desktop's document has. Nothing to do.
    public sealed record UpToDate : SetupSharing;
    /// The document went over and was accepted, and the machine has not been read since.
    public sealed record JustShared : SetupSharing;
    /// An agent from before the setup was shared, or one that cannot carry a document at all. It is
    /// excluded from reconciliation rather than counted as a conflict.
    public sealed record Unsupported : SetupSharing;
    public sealed record Failed(string Sentence) : SetupSharing;
    /// The machine holds an ancestor of this document. Safe to hand it the current one.
    public sealed record Behind : SetupSharing;
    /// This device holds an ancestor of the machine's document. Safe to adopt it.
    public sealed record Ahead : SetupSharing;
    /// Same setup, and neither side descends from the other. Somebody has to decide.
    public sealed record Diverged(string Sentence) : SetupSharing;
    /// A different setup altogether. Neither is an older copy of the other.
    public sealed record DifferentSetup(string Sentence) : SetupSharing;
    /// Nothing to say: not read yet, or nothing usable to share.
    public sealed record Unknown : SetupSharing;

    public string? Sentence_ => this switch
    {
        Failed failed => failed.Sentence,
        Diverged diverged => diverged.Sentence,
        DifferentSetup different => different.Sentence,
        UpToDate => "up to date",
        JustShared => "sent, waiting for the next reading",
        Unsupported => "this agent cannot carry the setup",
        Behind => "the machine is behind this device's copy",
        Ahead => "this device is behind the machine's copy",
        _ => null,
    };

    /// Whether a person has to answer something before anything moves.
    public bool NeedsDecision => this is Diverged or DifferentSetup;
}

/// What would change if a document were adopted. Shown before anything is applied, because
/// "replace the setup" is not a decision anyone should make from a hash.
public sealed record SetupConflictPreview(
    IReadOnlyList<string> MachinesAdded,
    IReadOnlyList<string> MachinesRemoved,
    IReadOnlyList<string> MachinesChanged,
    int EndpointsBefore,
    int EndpointsAfter)
{
    public bool IsEmpty => MachinesAdded.Count == 0 && MachinesRemoved.Count == 0 && MachinesChanged.Count == 0
        && EndpointsBefore == EndpointsAfter;

    public IEnumerable<string> Lines()
    {
        if (MachinesAdded.Count > 0) yield return $"Machines added: {string.Join(", ", MachinesAdded)}";
        if (MachinesRemoved.Count > 0) yield return $"Machines removed: {string.Join(", ", MachinesRemoved)}";
        if (MachinesChanged.Count > 0) yield return $"Machines changed: {string.Join(", ", MachinesChanged)}";
        if (EndpointsBefore != EndpointsAfter) yield return $"Addresses: {EndpointsBefore} before, {EndpointsAfter} after";
        if (IsEmpty) yield return "Nothing this app reads would change.";
    }

    /// What one document would do to another, in the terms the setup section shows.
    public static SetupConflictPreview Between(ControllerConfig? current, ControllerConfig incoming)
    {
        var before = current?.Machines ?? Array.Empty<MachineConfig>();
        var beforeIds = before.Select(machine => machine.Id).ToHashSet(StringComparer.Ordinal);
        var afterIds = incoming.Machines.Select(machine => machine.Id).ToHashSet(StringComparer.Ordinal);

        var changed = new List<string>();
        foreach (var machine in incoming.Machines)
        {
            var existing = before.FirstOrDefault(candidate => candidate.Id == machine.Id);
            if (existing is not null && existing != machine) changed.Add(machine.Id);
        }

        return new SetupConflictPreview(
            afterIds.Except(beforeIds).OrderBy(id => id, StringComparer.Ordinal).ToList(),
            beforeIds.Except(afterIds).OrderBy(id => id, StringComparer.Ordinal).ToList(),
            changed,
            before.Sum(machine => machine.Endpoints.Count),
            incoming.Machines.Sum(machine => machine.Endpoints.Count));
    }
}
