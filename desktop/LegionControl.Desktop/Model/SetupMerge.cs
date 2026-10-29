using System.Globalization;
using System.Numerics;
using System.Text;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Model;

/// One entry two copies of the setup disagree about (contract section 9.8).
///
/// The granularity is the thing a person recognises: a machine, a site, one address on a machine,
/// its wake block, one of its systems, an unknown key somebody else's client wrote. A diff at the
/// level of JSON paths would be accurate and unreadable, and "the setups differ" would be readable
/// and useless.
public sealed record SetupDifference(string Entry, SetupChange Kind)
{
    /// The ones a person answers. With a base, both sides moved the entry away from it, a deletion
    /// against an edit included; without one, nobody can say who moved what. Both are preselected
    /// theirs and always shown.
    public bool NeedsChoice => Kind is SetupChange.Both or SetupChange.NoBase;

    public string Description => Kind switch
    {
        SetupChange.Mine => $"{Entry} changed here only.",
        SetupChange.Theirs => $"{Entry} changed there only.",
        SetupChange.Both => $"{Entry} changed on both sides.",
        _ => $"{Entry} differs, and no revision both sides share says who changed it.",
    };
}

public enum SetupChange
{
    /// Only this device changed it, so the merge keeps this side.
    Mine,
    /// Only the other side changed it, so the merge takes theirs.
    Theirs,
    /// Both changed it since the base.
    Both,
    /// There is no base to tell who changed it.
    NoBase,
}

/// Which side of a difference to keep.
public enum SetupChoice
{
    Mine,
    Theirs,
}

/// Compares and merges two setup documents: the reference of contract section 9.8,
/// contract/tools/setup-sync.mjs, ported and held to contract/vectors/setup-merge.json.
///
/// This is what makes concurrent offline edits survive. Two peers that both edited revision 5 have
/// two revision 6s, and neither number nor hash can say which to keep: the answer is to keep both
/// people's work, entry by entry, and to ask about the entries where that is impossible. The merge
/// before this one let a deletion beat an edit without asking, appended an entry taken from the
/// other side at the end of its list, which changes dial order, and compared values by a sorted
/// re-serialisation. This one keeps mine's order and spelling, inserts what theirs added after
/// its predecessor there, and asks about every entry both sides changed.
public static class SetupMerge
{
    /// Who writes the merge: their `source`, `device` and `updatedAt` go into its identity.
    public sealed record Writer(string Source, string Device, string Now)
    {
        public static Writer Here(string? device) =>
            new(ControllerIdentity.ThisSource, device ?? Environment.MachineName, DateTimeOffset.UtcNow.ToString("o"));
    }

    /// What a merge came to: the differences, and the merged document, or the first section 9.7
    /// rule it would break when it would be refused, in which case nothing is written.
    public sealed record Result(IReadOnlyList<SetupDifference> Differences, string? Text, string? Error);

    /// The section 9.7 errors mine may carry into a merge. Each is about how parts that are well
    /// formed on their own refer to each other: a site or a helper that names nothing, a helper that
    /// is its own, a loop, an alias that disagrees. The applied document is kept with those, and a
    /// device in that state still merges: the result is checked like any other, so taking theirs'
    /// side of the entry at fault repairs it. Every other error leaves no single answer to what the
    /// document's entries are, and mine is fixed in the editor before it is merged.
    public static readonly IReadOnlySet<string> MergeableErrors =
        new HashSet<string>(StringComparer.Ordinal) { "unknown-site", "helper-self", "helper-unknown", "helper-alias", "helper-cycle" };

    /// Why [mineText] cannot be merged or kept as it is, or null when it can. Take theirs is never
    /// held up by this.
    public static string? WhyMineCannotMerge(string mineText)
    {
        var verdict = DocumentAcceptance.Check(mineText, Intake.Applied);
        var blocking = verdict.Errors.Where(error => !MergeableErrors.Contains(error.Code)).ToList();
        if (blocking.Count > 0)
            return $"The setup here has to be fixed in the editor before it can be merged: {string.Join(" ", blocking.Select(error => error.Message))}";
        return Side(mineText, "mine")["controller"] is DocumentObject block && block["id"] is DocumentString
            ? null
            : "The setup here has no setup id yet, so it takes no part in a merge.";
    }

    /// Why [theirsText] cannot be merged or taken, or null when it can.
    public static string? WhyTheirsCannotMerge(string theirsText) =>
        DocumentAcceptance.Check(theirsText, Intake.Peer) is { Valid: false } refused
            ? $"The machine's setup is one this device does not accept: {refused.Sentence}"
            : null;

    /// The entries where mine and theirs differ, in code-point order of their keys. A reorder
    /// alone, a different spelling of the same number and a member written in another order are
    /// not differences.
    public static IReadOnlyList<SetupDifference> Differences(string mineText, string theirsText, string? baseText) =>
        Differing(Side(mineText, "mine"), Side(theirsText, "theirs"), baseText is null ? null : Side(baseText, "base"))
            .Select(found => new SetupDifference(found.Entry, found.Kind)).ToList();

    /// Merges two documents. [choices] answers entries by key; an entry it does not name takes the
    /// side its kind says: mine for a `mine` difference and theirs for every other, so `both` and
    /// `noBase` are preselected theirs. Keep mine is every entry answered mine.
    ///
    /// The result starts from mine. Every entry taken from theirs is removed where theirs lacks it
    /// and replaced in place where both have it; then theirs' arrays are walked to insert what
    /// theirs adds. Member order is mine's with new members appended, and no value that is not
    /// replaced is written differently. The identity is mine's id, one more than the higher
    /// revision, the merged lineage, and the writer's `updatedAt`, `source` and `device`.
    public static Result Merge(
        string? baseText, string mineText, string theirsText, string mineHash, string theirsHash, Writer writer,
        IReadOnlyDictionary<string, SetupChoice>? choices = null)
    {
        var mine = Side(mineText, "mine");
        var theirs = Side(theirsText, "theirs");
        var baseDocument = baseText is null ? null : Side(baseText, "base");
        if (mine["controller"] is not DocumentObject { } mineBlock || mineBlock["id"] is not DocumentString)
            throw new InvalidOperationException("mine has no setup id; contract section 9.5 gives a document an identity before it takes part in reconciliation");

        var found = Differing(mine, theirs, baseDocument);
        SetupChoice Answer(Found difference) =>
            choices is not null && choices.TryGetValue(difference.Entry, out var chosen) ? chosen
            : difference.Kind == SetupChange.Mine ? SetupChoice.Mine
            : SetupChoice.Theirs;
        var taken = found
            .Where(difference => Answer(difference) == SetupChoice.Theirs)
            .Select(difference => difference.Entry)
            .ToHashSet(StringComparer.Ordinal);

        var result = (DocumentObject)mine.Clone();
        foreach (var difference in found.Where(difference => taken.Contains(difference.Entry))) TakeTheirs(result, difference);

        WalkTheirs(result, "sites", theirs["sites"], id => $"sites[{id}]", taken);
        WalkTheirs(result, "machines", theirs["machines"], id => $"machines[{id}]", taken);
        var paired = PairedMachines(mine, theirs);
        foreach (var machine in ObjectsOf(theirs["machines"]))
        {
            var id = IdOf(machine);
            if (id is null || !paired.Contains(id)) continue;
            var at = $"machines[{id}]";
            var holder = MachineIn(result, id)!;
            WalkTheirs(holder, "systems", machine["systems"], system => $"{at}.systems[{system}]", taken);
            WalkTheirs(holder, "endpoints", machine["endpoints"],
                endpoint => endpoint is null ? $"{at}.endpoints[]" : $"{at}.endpoints[{endpoint}]", taken);
        }

        // The identity. Mine's id stays where it is; the rest is written in place, or appended in
        // this order when mine's block lacks it.
        var block = (DocumentObject)result["controller"]!;
        var higher = BigInteger.Max(RevisionOf(mine), RevisionOf(theirs));
        block.Set("revision", new DocumentNumber((higher + 1).ToString(CultureInfo.InvariantCulture)));
        block.Set("updatedAt", new DocumentString(writer.Now));
        block.Set("source", new DocumentString(writer.Source));
        block.Set("device", new DocumentString(writer.Device));
        block.Set("lineage", new DocumentArray(SetupSync.MergedLineage(mineHash, theirsHash, LineageOf(mine), LineageOf(theirs))
            .Select(hash => new DocumentString(hash))));

        // The result is held to peer intake like anything else a peer could read. One that would be
        // refused, a kept helper naming a machine taken out for one, is not written, and the person
        // is told the first rule it breaks.
        var text = DocumentJson.Print(result);
        var verdict = DocumentAcceptance.Check(text, Intake.Peer);
        var differences = found.Select(difference => new SetupDifference(difference.Entry, difference.Kind)).ToList();
        if (verdict.Valid) return new Result(differences, text, null);
        var codes = verdict.Errors.Select(error => error.Code).Distinct(StringComparer.Ordinal).ToList();
        codes.Sort(CompareCodePoints);
        return new Result(differences, null, codes[0]);
    }

    /// Keep mine: a merge with every entry answered mine. A new revision whose lineage holds the
    /// machine's head, so the machine fast-forwards to it, and never `--replace`.
    public static Result KeepMine(string? baseText, string mineText, string theirsText, string mineHash, string theirsHash, Writer writer) =>
        Merge(baseText, mineText, theirsText, mineHash, theirsHash, writer,
            Differences(mineText, theirsText, baseText).ToDictionary(difference => difference.Entry, _ => SetupChoice.Mine, StringComparer.Ordinal));

    /// Strings in code-point order. C#'s ordinal comparison compares UTF-16 code units, which puts
    /// U+FFFD after U+1F600; the contract lists differences in the same order on every platform.
    public static int CompareCodePoints(string a, string b)
    {
        var x = CodePoints(a);
        var y = CodePoints(b);
        for (var index = 0; index < Math.Min(x.Count, y.Count); index += 1)
        {
            if (x[index] != y[index]) return x[index] - y[index];
        }
        return x.Count - y.Count;
    }

    /// A string's code points. A surrogate without its partner is a code point of its own, as
    /// JavaScript counts it, never the replacement character.
    private static List<int> CodePoints(string text)
    {
        var points = new List<int>(text.Length);
        for (var index = 0; index < text.Length; index += 1)
        {
            if (char.IsHighSurrogate(text[index]) && index + 1 < text.Length && char.IsLowSurrogate(text[index + 1]))
            {
                points.Add(char.ConvertToUtf32(text[index], text[index + 1]));
                index += 1;
            }
            else points.Add(text[index]);
        }
        return points;
    }

    // MARK: sides

    /// Parses one side of a merge. Theirs and the base are held to section 9.7 under peer intake,
    /// mine to applied intake with only [MergeableErrors] allowed. Anything else throws: a caller
    /// checks the same with [WhyMineCannotMerge] and [WhyTheirsCannotMerge] before it offers Merge.
    private static DocumentObject Side(string text, string side)
    {
        var verdict = DocumentAcceptance.Check(text, side == "mine" ? Intake.Applied : Intake.Peer);
        var blocking = verdict.Errors.Where(error => side != "mine" || !MergeableErrors.Contains(error.Code)).ToList();
        if (blocking.Count > 0)
            throw new InvalidOperationException($"{side} breaks section 9.7 ({string.Join(", ", blocking.Select(error => error.Code))}), so it is not merged");
        return (DocumentObject)DocumentJson.Parse(Encoding.UTF8.GetString(Canonical.Bytes(Encoding.UTF8.GetBytes(text))));
    }

    private static BigInteger RevisionOf(DocumentObject document) =>
        document["controller"] is DocumentObject block && block["revision"] is DocumentNumber revision
        && JsonEquality.Integer(revision.Lexeme) is { } whole
            ? whole
            : BigInteger.Zero;

    private static IReadOnlyList<string> LineageOf(DocumentObject document) =>
        document["controller"] is DocumentObject block && block["lineage"] is DocumentArray lineage
            ? lineage.Items.OfType<DocumentString>().Select(hash => hash.Value).ToList()
            : Array.Empty<string>();

    // MARK: entries

    /// The controller keys that are identity, which a merge writes itself and never offers.
    private static readonly string[] IdentityKeys = { "id", "revision", "updatedAt", "source", "device", "lineage" };
    /// The top-level keys whose contents are entries of their own.
    private static readonly string[] StructureKeys = { "controller", "machines", "sites" };
    /// The members of a machine that are entries of their own when both sides have the machine.
    private static readonly string[] MachineParts = { "endpoints", "systems", "wake" };
    /// How the other entries' keys begin, and the quote that marks a top-level key written in quotes.
    private static readonly string[] QuotedBeginnings = { "\"", "controller.", "sites[", "machines[" };

    private enum Scope { Top, Controller, Site, Machine, Fields, Wake, System, Endpoint, Idless }

    /// Where an entry lives: what kind of part, the machine it belongs to, and its own id or name.
    private sealed record Place(Scope Scope, string? Name = null, string? Machine = null, string? Id = null);

    private sealed record Found(string Entry, Place Place, DocumentValue? Mine, DocumentValue? Theirs, DocumentValue? Base)
    {
        public SetupChange Kind { get; init; }
    }

    /// The entry key of a top-level member: its name, unless the name begins the way another
    /// entry's key does. Then it is written inside double quotes with nothing escaped, and so is a
    /// name that already begins with a quote, which keeps the quoting reversible. Without this a
    /// peer document with a member called `sites[home]` beside a site `home` would give one key to
    /// two entries.
    private static string TopLevelKey(string name) =>
        QuotedBeginnings.Any(beginning => name.StartsWith(beginning, StringComparison.Ordinal)) ? $"\"{name}\"" : name;

    private static IEnumerable<DocumentObject> ObjectsOf(DocumentValue? value) =>
        value is DocumentArray array ? array.Items.OfType<DocumentObject>() : Enumerable.Empty<DocumentObject>();

    private static string? IdOf(DocumentValue? value) => value is DocumentObject holder && holder["id"] is DocumentString id ? id.Value : null;

    private static HashSet<string> MachineIds(DocumentObject document) =>
        ObjectsOf(document["machines"]).Select(IdOf).OfType<string>().ToHashSet(StringComparer.Ordinal);

    /// The ids of the machines both documents have, which merge part by part.
    private static HashSet<string> PairedMachines(DocumentObject mine, DocumentObject theirs)
    {
        var paired = MachineIds(mine);
        paired.IntersectWith(MachineIds(theirs));
        return paired;
    }

    /// One document's entries. Keys are unique in every document a merge takes: section 9.7 refuses
    /// two machines, two sites, or two systems or endpoints of one machine with one id, and
    /// [TopLevelKey] keeps unknown names apart from the rest. A second entry under one key is a bug,
    /// and it throws rather than merge one of the two.
    private static Dictionary<string, (Place Place, DocumentValue Value)> EntryMap(DocumentObject? document, HashSet<string> paired)
    {
        var map = new Dictionary<string, (Place, DocumentValue)>(StringComparer.Ordinal);
        void Add(string key, Place place, DocumentValue value)
        {
            if (!map.TryAdd(key, (place, value))) throw new InvalidOperationException($"two entries of one document are both called {key}");
        }
        if (document is null) return map;
        foreach (var (name, value) in document.Members)
            if (!StructureKeys.Contains(name)) Add(TopLevelKey(name), new Place(Scope.Top, Name: name), value);
        if (document["controller"] is DocumentObject controller)
        {
            foreach (var (name, value) in controller.Members)
                if (!IdentityKeys.Contains(name)) Add($"controller.{name}", new Place(Scope.Controller, Name: name), value);
        }
        foreach (var site in ObjectsOf(document["sites"]))
            if (IdOf(site) is { } id) Add($"sites[{id}]", new Place(Scope.Site, Id: id), site);
        foreach (var machine in ObjectsOf(document["machines"]))
        {
            if (IdOf(machine) is not { } id) continue;
            var at = $"machines[{id}]";
            if (!paired.Contains(id))
            {
                Add(at, new Place(Scope.Machine, Id: id), machine);
                continue;
            }
            Add($"{at}.fields", new Place(Scope.Fields, Machine: id),
                new DocumentObject(machine.Members.Where(member => !MachineParts.Contains(member.Key))));
            if (machine["wake"] is { } wake) Add($"{at}.wake", new Place(Scope.Wake, Machine: id), wake);
            foreach (var system in ObjectsOf(machine["systems"]))
                if (IdOf(system) is { } systemId) Add($"{at}.systems[{systemId}]", new Place(Scope.System, Machine: id, Id: systemId), system);
            // Endpoints without an id cannot be told apart, so together, in order, they are one
            // entry. None at all is absent.
            var idless = new List<DocumentValue>();
            foreach (var endpoint in ObjectsOf(machine["endpoints"]))
            {
                if (IdOf(endpoint) is { } endpointId) Add($"{at}.endpoints[{endpointId}]", new Place(Scope.Endpoint, Machine: id, Id: endpointId), endpoint);
                else idless.Add(endpoint);
            }
            if (idless.Count > 0) Add($"{at}.endpoints[]", new Place(Scope.Idless, Machine: id), new DocumentArray(idless));
        }
        return map;
    }

    /// Every entry where mine and theirs differ, in code-point order of its key, with its kind.
    private static List<Found> Differing(DocumentObject mine, DocumentObject theirs, DocumentObject? baseDocument)
    {
        var paired = PairedMachines(mine, theirs);
        var ofMine = EntryMap(mine, paired);
        var ofTheirs = EntryMap(theirs, paired);
        var ofBase = EntryMap(baseDocument, paired);
        var keys = ofMine.Keys.Union(ofTheirs.Keys, StringComparer.Ordinal).ToList();
        keys.Sort(CompareCodePoints);
        var found = new List<Found>();
        foreach (var key in keys)
        {
            var place = ofMine.TryGetValue(key, out var inMine) ? inMine.Place : ofTheirs[key].Place;
            var mineValue = ofMine.TryGetValue(key, out var m) ? m.Value : null;
            var theirsValue = ofTheirs.TryGetValue(key, out var t) ? t.Value : null;
            var baseValue = ofBase.TryGetValue(key, out var b) ? b.Value : null;
            if (JsonEquality.Equal(mineValue, theirsValue)) continue;
            // With a base, the side that left the entry as the base had it did not change it, so the
            // other side's value is taken; when both changed it, a deletion against an edit included,
            // it is both. Without a base nothing says who changed what.
            var kind = baseDocument is null ? SetupChange.NoBase
                : JsonEquality.Equal(theirsValue, baseValue) ? SetupChange.Mine
                : JsonEquality.Equal(mineValue, baseValue) ? SetupChange.Theirs
                : SetupChange.Both;
            found.Add(new Found(key, place, mineValue, theirsValue, baseValue) { Kind = kind });
        }
        return found;
    }

    // MARK: building the result

    private static DocumentObject? MachineIn(DocumentObject result, string id) =>
        ObjectsOf(result["machines"]).FirstOrDefault(machine => IdOf(machine) == id);

    /// In an array of objects keyed by id: removes the element (value null) or replaces it where it
    /// stands. Whether the id was there.
    private static bool ReplaceById(DocumentValue? array, string id, DocumentValue? value)
    {
        if (array is not DocumentArray items) return false;
        var index = items.Items.FindIndex(item => IdOf(item) == id);
        if (index < 0) return false;
        if (value is null) items.Items.RemoveAt(index);
        else items.Items[index] = value.Clone();
        return true;
    }

    /// Takes one entry from theirs: removes what theirs lacks and replaces in place what both have.
    /// Insertions come later, in [WalkTheirs].
    private static void TakeTheirs(DocumentObject result, Found found)
    {
        var theirs = found.Theirs;
        void SetOrRemove(DocumentObject holder, string name)
        {
            if (theirs is null) holder.Remove(name);
            else holder.Set(name, theirs.Clone());
        }
        var place = found.Place;
        switch (place.Scope)
        {
            case Scope.Top:
                SetOrRemove(result, place.Name!);
                return;
            case Scope.Controller:
                SetOrRemove((DocumentObject)result["controller"]!, place.Name!);
                return;
            case Scope.Site:
                ReplaceById(result["sites"], place.Id!, theirs);
                return;
            case Scope.Machine:
                ReplaceById(result["machines"], place.Id!, theirs);
                return;
            case Scope.Fields:
            {
                // The machine's own members, other than its parts, become theirs: each where mine
                // has it, gone where theirs lacks it, appended where it is new.
                var machine = MachineIn(result, place.Machine!)!;
                var fields = (DocumentObject)theirs!;
                foreach (var name in machine.Members.Select(member => member.Key).Where(name => !MachineParts.Contains(name)).ToList())
                {
                    if (fields[name] is { } value) machine.Set(name, value.Clone());
                    else machine.Remove(name);
                }
                foreach (var (name, value) in fields.Members)
                    if (!machine.Has(name)) machine.Set(name, value.Clone());
                return;
            }
            case Scope.Wake:
                SetOrRemove(MachineIn(result, place.Machine!)!, "wake");
                return;
            case Scope.System:
                ReplaceById(MachineIn(result, place.Machine!)!["systems"], place.Id!, theirs);
                return;
            case Scope.Endpoint:
                ReplaceById(MachineIn(result, place.Machine!)!["endpoints"], place.Id!, theirs);
                return;
            case Scope.Idless:
                if (MachineIn(result, place.Machine!)!["endpoints"] is DocumentArray endpoints)
                    endpoints.Items.RemoveAll(item => IdOf(item) is null);
                return;
        }
    }

    /// Walks one of theirs' arrays in order with a cursor (section 9.8, "Building the result"). An
    /// element whose key is in the result moves the cursor to it; an element taken from theirs that
    /// is not in the result is inserted after the cursor, first when there is none, and becomes the
    /// cursor. Anything else is passed over. So an element theirs added lands after its
    /// predecessor in theirs, and everything else keeps mine's order.
    ///
    /// An element without an id has no key of its own. When the id-less entry is taken from theirs,
    /// mine's id-less elements are already gone and each of theirs' is inserted; otherwise theirs'
    /// n-th id-less element stands for the result's n-th, and only moves the cursor.
    private static void WalkTheirs(DocumentObject holder, string name, DocumentValue? theirsArray, Func<string?, string> entryOf, HashSet<string> taken)
    {
        var array = holder[name] as DocumentArray;
        var cursor = -1;
        var idless = 0;
        foreach (var element in ObjectsOf(theirsArray))
        {
            var id = IdOf(element);
            var entry = entryOf(id);
            var items = array?.Items ?? new List<DocumentValue>();
            int index;
            if (id is not null)
            {
                index = items.FindIndex(item => IdOf(item) == id);
            }
            else if (taken.Contains(entry))
            {
                index = -1;
            }
            else
            {
                var positions = Enumerable.Range(0, items.Count).Where(position => items[position] is DocumentObject && IdOf(items[position]) is null).ToList();
                index = idless < positions.Count ? positions[idless] : -1;
                idless += 1;
            }
            if (index >= 0)
            {
                cursor = index;
                continue;
            }
            if (!taken.Contains(entry)) continue;
            // An array mine never had is created only for something that goes in it.
            if (array is null)
            {
                array = new DocumentArray();
                holder.Set(name, array);
            }
            cursor += 1;
            array.Items.Insert(cursor, element.Clone());
        }
    }
}
