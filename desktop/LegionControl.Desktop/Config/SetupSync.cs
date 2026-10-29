using System.Text;

namespace LegionControl.Desktop.Config;

/// The lineage arithmetic of setup sync (contract sections 9.5 and 9.8, reference
/// contract/tools/setup-sync.mjs): which kept revision two branches share, and what a merge of
/// them descends from.
public static class SetupSync
{
    /// The base of a merge (contract section 9.8): the first hash of mine's lineage, newest first,
    /// that is also in theirs' lineage and whose kept bytes still hash to it; null when there is
    /// none. The heads themselves are never a base: a merge whose base was one side would only
    /// ever take the other side.
    public static string? CommonBase(
        string mineHash, string theirsHash, IReadOnlyList<string> mineLineage, IReadOnlyList<string> theirsLineage,
        Func<string, string?> kept)
    {
        foreach (var hash in mineLineage)
        {
            if (hash == mineHash || hash == theirsHash || !theirsLineage.Contains(hash, StringComparer.Ordinal)) continue;
            if (kept(hash) is { } text && Canonical.Hash(Encoding.UTF8.GetBytes(text)) == hash) return hash;
        }
        return null;
    }

    /// The lineage of a merge: mine's hash, theirs' hash, then the two lineages interleaved, mine
    /// first, each hash once, cut at 32. Both heads are in it, so both branches fast-forward onto
    /// the result. Interleaved rather than concatenated: with a cap of 32, the recent history of
    /// both sides is worth more than all of one side's and none of the other's.
    public static IReadOnlyList<string> MergedLineage(
        string mineHash, string theirsHash, IReadOnlyList<string> mineLineage, IReadOnlyList<string> theirsLineage)
    {
        var lineage = new List<string>();
        void Add(string hash)
        {
            if (!lineage.Contains(hash, StringComparer.Ordinal)) lineage.Add(hash);
        }
        Add(mineHash);
        Add(theirsHash);
        for (var index = 0; index < Math.Max(mineLineage.Count, theirsLineage.Count); index += 1)
        {
            if (index < mineLineage.Count) Add(mineLineage[index]);
            if (index < theirsLineage.Count) Add(theirsLineage[index]);
        }
        return lineage.Take(ControllerIdentity.LineageLimit).ToList();
    }
}
