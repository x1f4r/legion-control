using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// Documents built the way the tests need them, and hashed the way everything else does.
internal static class Documents
{
    public static ControllerDocument Make(
        string setupId, long revision, IReadOnlyList<string> lineage, string machineName = "Beacon",
        string? extra = null)
    {
        var root = new JsonObject
        {
            ["version"] = 1,
            ["controller"] = new JsonObject
            {
                ["id"] = setupId,
                ["revision"] = revision,
                ["source"] = "desktop",
                ["device"] = "test device",
                ["lineage"] = new JsonArray(lineage.Select(hash => (JsonNode)hash!).ToArray()),
            },
            ["machines"] = new JsonArray(new JsonObject
            {
                ["id"] = "pi",
                ["name"] = machineName,
                ["endpoints"] = new JsonArray(new JsonObject
                {
                    ["id"] = "remote",
                    ["host"] = "pi.example",
                    ["user"] = "me",
                }),
                ["systems"] = new JsonArray(new JsonObject
                {
                    ["id"] = "linux",
                    ["platform"] = "linux",
                    ["agent"] = new JsonArray("node", "/agent/src/index.mjs"),
                }),
            }),
        };
        if (extra is not null) root["extra"] = extra;
        return ControllerDocument.FromText(root.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));
    }
}

/// Ancestry, which is the only thing that can tell "you are behind" from "we both edited", read
/// through the rule every client shares (contract section 9.5). The shared vectors pin each rule;
/// these walk real documents through it.
public class LineageTests
{
    private static Decision Decide(ControllerDocument mine, ControllerDocument? theirs, params ControllerDocument[] filed) =>
        SetupSync.Decide(LocalHead.Of(mine), 3,
            theirs is null ? new ControllerMark() : new ControllerMark { Hash = theirs.Hash, Id = theirs.Identity.Id },
            filed.ToDictionary(document => document.Hash,
                document => new ControllerMark { Hash = document.Hash, Id = document.Identity.Id, Lineage = document.Identity.Lineage },
                StringComparer.Ordinal));

    [Fact]
    public void TheSameBytesAreTheSameDocument()
    {
        var mine = Documents.Make("setup-a", 5, Array.Empty<string>());
        Assert.Equal(SetupDecision.InSync, Decide(mine, mine).Kind);
    }

    [Fact]
    public void AMachineHoldingAnAncestorIsBehindAndStatusAloneSaysSo()
    {
        var parent = Documents.Make("setup-a", 5, Array.Empty<string>());
        var child = Documents.Make("setup-a", 6, new[] { parent.Hash });
        Assert.Equal(new Decision(SetupDecision.Push, "machine-behind"), Decide(child, parent));
    }

    [Fact]
    public void AMachineHoldingADescendantIsFetchedFromOnceItsLineageIsRead()
    {
        var parent = Documents.Make("setup-a", 5, Array.Empty<string>());
        var child = Documents.Make("setup-a", 6, new[] { parent.Hash });
        // Status cannot say that on its own: this device's lineage does not mention the child.
        Assert.Equal(SetupDecision.ReadMeta, Decide(parent, child).Kind);
        Assert.Equal(new Decision(SetupDecision.Fetch, Expect: child.Hash), Decide(parent, child, child));
    }

    [Fact]
    public void TwoEditsOfTheSameRevisionAreDivergentAndNotOrdered()
    {
        // The whole reason revision numbers cannot decide this: both are revision 6.
        var parent = Documents.Make("setup-a", 5, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 6, new[] { parent.Hash }, "Beacon one");
        var theirs = Documents.Make("setup-a", 6, new[] { parent.Hash }, "Beacon two");
        Assert.NotEqual(mine.Hash, theirs.Hash);
        Assert.Equal(SetupDecision.Diverged, Decide(mine, theirs, theirs).Kind);
    }

    [Fact]
    public void TwoDifferentSetupsAreNeverOrderedEvenWhenOneHashIsAnAncestor()
    {
        var mine = Documents.Make("setup-a", 9, Array.Empty<string>());
        var theirs = Documents.Make("setup-b", 2, Array.Empty<string>());
        Assert.Equal(SetupDecision.DifferentSetup, Decide(mine, theirs).Kind);
        // Pushing over another setup would replace somebody else's fleet, whatever the lineage says.
        var child = Documents.Make("setup-a", 10, new[] { theirs.Hash });
        Assert.Equal(SetupDecision.DifferentSetup, Decide(child, theirs).Kind);
    }

    [Fact]
    public void AMachineHoldingNothingOrACopyNobodyClaimsIsPublishedTo()
    {
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        Assert.Equal(new Decision(SetupDecision.Push, "machine-empty"), Decide(mine, null));
        Assert.Equal(new Decision(SetupDecision.Push, "machine-unidentified"),
            SetupSync.Decide(LocalHead.Of(mine), 3, new ControllerMark { Hash = "0".PadLeft(64, '0') },
                new Dictionary<string, ControllerMark>()));
    }

    [Fact]
    public void AnEditPutsTheParentAtTheHeadOfTheLineageAndBumpsTheRevision()
    {
        var parent = Documents.Make("setup-a", 5, new[] { "a".PadLeft(64, 'a') });
        var next = parent.Identity.Next(parent.Hash, "my tower");
        Assert.Equal(6, next.RevisionNumber);
        Assert.Equal(parent.Hash, next.Lineage[0]);
        Assert.Equal("a".PadLeft(64, 'a'), next.Lineage[1]);
        Assert.Equal("my tower", next.Device);
        Assert.Equal("desktop", next.Source);
        Assert.Equal("setup-a", next.Id);
    }

    [Fact]
    public void TheLineageIsCappedAndKeepsTheNewest()
    {
        var identity = new ControllerIdentity
        {
            Id = "setup-a",
            Revision = 40,
            Lineage = Enumerable.Range(0, 32).Select(index => index.ToString().PadLeft(64, '0')).ToList(),
        };
        var next = identity.Next("f".PadLeft(64, 'f'), "device");
        Assert.Equal(ControllerIdentity.LineageLimit, next.Lineage.Count);
        Assert.Equal("f".PadLeft(64, 'f'), next.Lineage[0]);
    }

    [Fact]
    public void AMergeDescendsFromBothHeads()
    {
        var mine = Documents.Make("setup-a", 6, new[] { "1".PadLeft(64, '1') });
        var theirs = Documents.Make("setup-a", 7, new[] { "2".PadLeft(64, '2') }, "other");
        var lineage = SetupSync.MergedLineage(mine.Hash, theirs.Hash, mine.Identity.Lineage, theirs.Identity.Lineage);
        Assert.Equal(new[] { mine.Hash, theirs.Hash, "1".PadLeft(64, '1'), "2".PadLeft(64, '2') }, lineage);
        // So both sides fast-forward to it rather than either being overwritten.
        var merged = new LocalHead("merged", "setup-a", lineage);
        Assert.Equal(new Decision(SetupDecision.Push, "machine-behind"),
            SetupSync.Decide(merged, 3, new ControllerMark { Hash = mine.Hash, Id = "setup-a" }, new Dictionary<string, ControllerMark>()));
        Assert.Equal(new Decision(SetupDecision.Push, "machine-behind"),
            SetupSync.Decide(merged, 3, new ControllerMark { Hash = theirs.Hash, Id = "setup-a" }, new Dictionary<string, ControllerMark>()));
    }

    [Fact]
    public void ARubbishLineageIsRefusedRatherThanCarried()
    {
        string With(string lineage) =>
            $$"""{ "version": 1, "controller": { "id": "setup-a", "revision": 1, "lineage": {{lineage}} }, "machines": [] }""";
        Assert.Contains(DocumentAcceptance.Check(With("[\"not-a-hash\"]"), Intake.Peer).Errors, error => error.Code == "schema");
        var hash = "\"" + "a".PadLeft(64, 'a') + "\"";
        Assert.Contains(DocumentAcceptance.Check(With($"[{hash}, {hash}]"), Intake.Peer).Errors, error => error.Message.Contains("twice"));
        var tooMany = "[" + string.Join(", ", Enumerable.Range(0, 33).Select(index => "\"" + index.ToString().PadLeft(64, '0') + "\"")) + "]";
        Assert.Contains(DocumentAcceptance.Check(With(tooMany), Intake.Peer).Errors, error => error.Message.Contains("at most"));
    }
}

/// The store of earlier documents, which is what makes a three-way merge possible at all.
public class SetupLedgerTests
{
    [Fact]
    public void RemembersDocumentsByHashAndReadsThemBack()
    {
        using var home = new TempHome();
        var ledger = new SetupLedger(Path.Combine(home.Path, "revisions"));
        var document = Documents.Make("setup-a", 1, Array.Empty<string>());
        ledger.Remember(document);
        ledger.Remember(document);
        Assert.True(ledger.Has(document.Hash));
        Assert.Single(ledger.Hashes());
        Assert.Equal(document.Hash, ledger.Read(document.Hash)!.Hash);
    }

    [Fact]
    public void RefusesAFileWhoseContentsNoLongerHashToItsName()
    {
        using var home = new TempHome();
        var ledger = new SetupLedger(Path.Combine(home.Path, "revisions"));
        var document = Documents.Make("setup-a", 1, Array.Empty<string>());
        ledger.Remember(document);
        File.WriteAllText(ledger.PathFor(document.Hash), "{\"version\":1,\"machines\":[]}");
        Assert.Null(ledger.Read(document.Hash));
    }

    [Fact]
    public void FindsTheNewestCommonAncestorItStillHas()
    {
        using var home = new TempHome();
        var ledger = new SetupLedger(Path.Combine(home.Path, "revisions"));
        var older = Documents.Make("setup-a", 1, Array.Empty<string>());
        var newer = Documents.Make("setup-a", 2, new[] { older.Hash }, "second");
        ledger.Remember(older);
        ledger.Remember(newer);

        var mine = new[] { newer.Hash, older.Hash };
        var theirs = new[] { "z".PadLeft(64, 'z'), newer.Hash, older.Hash };
        Assert.Equal(newer.Hash, ledger.CommonBase("m".PadLeft(64, 'm'), "t".PadLeft(64, 't'), mine, theirs));

        // A common hash whose bytes are gone is no use to a merge, and is not offered as one.
        Assert.Null(ledger.CommonBase("m".PadLeft(64, 'm'), "t".PadLeft(64, 't'), new[] { "y".PadLeft(64, 'y') }, new[] { "y".PadLeft(64, 'y') }));
        // Nor is either head: a base that is one side only ever takes the other.
        Assert.Equal(older.Hash, ledger.CommonBase(newer.Hash, "t".PadLeft(64, 't'), mine, theirs));
    }
}

/// The setup file on disk: how it is read, and what happens when a person edits it by hand.
public class ConfigStoreTests
{
    [Theory]
    [InlineData("{\"id\":\"setup-a\",\"revision\":\"3\"}")]
    [InlineData("{\"id\":\"setup-a\",\"revision\":3,\"lineage\":[42]}")]
    [InlineData("{\"revision\":3}")]
    [InlineData("{\"id\":\"a@b\",\"revision\":3}")]
    [InlineData("{\"id\":\"setup/a\",\"revision\":3}")]
    public void MalformedIdentityCannotBeAdoptedAsAnUnrevisionedSetup(string identity)
    {
        // A setup id is held to the agent's own grammar, not the wider token grammar, so an id the
        // agent would refuse with bad-argument is refused here before anything is sent.
        var text = $"{{\"version\":1,\"machines\":[],\"controller\":{identity}}}";
        Assert.Contains(DocumentAcceptance.Check(text, Intake.Peer).Errors, error => error.Code == "schema");
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 2, Array.Empty<string>()).Bytes);
        using var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.NotNull(store.Apply(ControllerDocument.FromText(text)));
    }
    [Fact]
    public void RestampingKeepsUnknownControllerFields()
    {
        using var home = new TempHome();
        var raw = JsonNode.Parse(Documents.Make("setup-a", 3, Array.Empty<string>()).Text)!.AsObject();
        raw["controller"]!["future"] = new JsonObject { ["nested"] = true, ["weight"] = JsonNode.Parse("22.0") };
        File.WriteAllText(home.ConfigPath, raw.ToJsonString());
        using var store = new ConfigStore(home.ConfigPath, watch: false);
        var (edited, problem) = store.ApplyEdit(store.Document!.Text.Replace("Beacon", "Renamed"));
        Assert.Null(problem);
        Assert.True(JsonNode.Parse(edited!.Text)!["controller"]!["future"]!["nested"]!.GetValue<bool>());
        // And spelled as it was: an edit never rewrites a number somebody else wrote.
        Assert.Contains("\"weight\": 22.0", edited.Text);
    }

    [Fact]
    public void InvalidAdoptionDoesNotReplaceTheFileOrAppliedPointer()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        File.WriteAllBytes(home.ConfigPath, mine.Bytes);
        using var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.NotNull(store.Apply(ControllerDocument.FromText("{\"version\":999,\"machines\":[]}")));
        Assert.Equal(mine.Hash, store.Document!.Hash);
        Assert.Equal(mine.Bytes, File.ReadAllBytes(home.ConfigPath));
    }
    [Fact]
    public void GivesADocumentWithNoIdentityOneOfItsOwn()
    {
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, """
            { "version": 1, "machines": [ { "id": "pi", "name": "Beacon" } ] }
            """);
        var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.NotNull(store.Document);
        Assert.True(store.Document!.Identity.HasIdentity);
        Assert.StartsWith("setup-", store.Document.Identity.Id);
        Assert.Equal(1, store.Document.Identity.RevisionNumber);
        Assert.NotNull(store.AssignedSetupId);
        // The file itself now carries it, so every other device that adopts it agrees.
        Assert.Contains("setup-", File.ReadAllText(home.ConfigPath));
    }

    [Fact]
    public void AHandEditBecomesTheNextRevisionWithTheOldDocumentInItsLineage()
    {
        using var home = new TempHome();
        var first = Documents.Make("setup-a", 4, Array.Empty<string>());
        File.WriteAllBytes(home.ConfigPath, first.Bytes);
        var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.Equal(4, store.Document!.Identity.RevisionNumber);
        var firstHash = store.Document.Hash;

        // Somebody edits the file in an editor and leaves the controller block alone.
        var edited = File.ReadAllText(home.ConfigPath).Replace("\"Beacon\"", "\"Beacon renamed\"");
        File.WriteAllText(home.ConfigPath, edited);
        store.Load();

        Assert.Equal(5, store.Document!.Identity.RevisionNumber);
        Assert.Equal(firstHash, store.Document.Identity.Lineage[0]);
        Assert.Equal("Beacon renamed", store.Config!.Machines[0].Name);
        // The bytes that were replaced are still there to go back to.
        Assert.True(store.Ledger.Has(firstHash));
    }

    [Fact]
    public void AnEditThroughTheAppIsTheSameKindOfEdit()
    {
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 2, Array.Empty<string>()).Bytes);
        var store = new ConfigStore(home.ConfigPath, watch: false, deviceName: "tower");
        var before = store.Document!.Hash;

        var plan = ControllerEditor.Plan(store.Document.Text, store.Config, "add a site",
            ControllerEditor.UpsertSite(new JsonObject
            {
                ["id"] = "attic",
                ["name"] = "Attic",
                ["lanPrefixes"] = new JsonArray("10.0.0."),
            }));
        Assert.True(plan.IsApplicable);

        var (document, problem) = store.ApplyEdit(plan.Text, "add a site");
        Assert.Null(problem);
        Assert.Equal(3, document!.Identity.RevisionNumber);
        Assert.Equal(before, document.Identity.Lineage[0]);
        Assert.Equal("tower", document.Identity.Device);
        Assert.Single(store.Config!.Sites);
    }

    [Fact]
    public void AnEditMadeWhileTheAppWasClosedIsStillAnEdit()
    {
        // The one case a poll cannot catch: somebody edits the file with the app shut, and the app
        // comes back with no memory of what it last applied except what is on disk. Without the
        // pointer it would take the edited bytes for the document it already published and hand
        // every machine a revision with no ancestry, which is a conflict on arrival.
        using var home = new TempHome();
        var first = Documents.Make("setup-a", 7, Array.Empty<string>());
        File.WriteAllBytes(home.ConfigPath, first.Bytes);
        var statePath = Path.Combine(home.Path, "setup-state.json");
        var ledger = new SetupLedger(Path.Combine(home.Path, "revisions"));

        var before = new ConfigStore(home.ConfigPath, statePath, ledger, "tower", watch: false);
        var appliedHash = before.Document!.Hash;
        before.Dispose();

        // The app is not running. The file changes.
        File.WriteAllText(home.ConfigPath,
            File.ReadAllText(home.ConfigPath).Replace("\"Beacon\"", "\"Beacon edited offline\""));

        var after = new ConfigStore(home.ConfigPath, statePath, ledger, "tower", watch: false);
        Assert.Equal(8, after.Document!.Identity.RevisionNumber);
        Assert.Equal(appliedHash, after.Document.Identity.Lineage[0]);
        Assert.Equal("Beacon edited offline", after.Config!.Machines[0].Name);
        Assert.True(ledger.Has(appliedHash));
    }

    [Fact]
    public void ReadingTheSameFileTwiceIsNotAnEdit()
    {
        // The counterpart of the test above: a start that changes nothing must not bump anything,
        // or every launch would publish a new revision to every machine.
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 3, Array.Empty<string>()).Bytes);
        var statePath = Path.Combine(home.Path, "setup-state.json");
        var ledger = new SetupLedger(Path.Combine(home.Path, "revisions"));

        var first = new ConfigStore(home.ConfigPath, statePath, ledger, "tower", watch: false);
        var hash = first.Document!.Hash;
        first.Dispose();

        var second = new ConfigStore(home.ConfigPath, statePath, ledger, "tower", watch: false);
        Assert.Equal(hash, second.Document!.Hash);
        Assert.Equal(3, second.Document.Identity.RevisionNumber);
    }

    [Fact]
    public void ADocumentThatDoesNotParseNeverReplacesOneThatDid()
    {
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 1, Array.Empty<string>()).Bytes);
        var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.NotNull(store.Config);

        File.WriteAllText(home.ConfigPath, "{ \"version\": 1, \"machines\": [ { \"id\": ");
        store.Load();
        Assert.NotNull(store.Problem);
        Assert.NotNull(store.Config);
        Assert.Equal("pi", store.Config!.Machines[0].Id);
    }

    [Fact]
    public void RefusesBytesThatAreNotADocumentAtAll()
    {
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, new byte[] { 0x7B, 0xFF, 0xFE, 0x7D });
        var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.Contains("UTF-8", store.Problem);
        Assert.Null(store.Config);
    }

    [Fact]
    public void AnAppliedDocumentWithAnErrorIsKeptAndOnlyTheMachineItIsInsideIsLeftOut()
    {
        // Applied intake (contract section 9.7): the document this device runs on is never refused
        // while its bytes are a document. The error is shown, and the one machine it is inside is
        // neither dialled nor run; the rest of the setup still works.
        using var home = new TempHome();
        var root = JsonNode.Parse(Documents.Make("setup-a", 3, Array.Empty<string>()).Text)!.AsObject();
        root["machines"]!.AsArray().Add(new JsonObject
        {
            ["id"] = "tower",
            ["endpoints"] = new JsonArray(new JsonObject { ["id"] = "lan", ["host"] = "-oProxyCommand=x" }),
        });
        var text = root.ToJsonString();
        File.WriteAllText(home.ConfigPath, text);
        using var store = new ConfigStore(home.ConfigPath, watch: false);

        Assert.Equal(ControllerDocument.FromText(text).Hash, store.Document!.Hash);
        Assert.False(store.Acceptance!.Valid);
        Assert.Equal(new[] { "pi" }, store.Config!.Machines.Select(machine => machine.Id));
        Assert.Contains("machines[1].endpoints[0].host", store.Problem);
        Assert.False(store.IsPendingEdit);
    }

    [Fact]
    public void AnEditWithAMemberWrittenTwiceWaitsUnappliedUntilItIsFixed()
    {
        using var home = new TempHome();
        var first = Documents.Make("setup-a", 4, Array.Empty<string>());
        File.WriteAllBytes(home.ConfigPath, first.Bytes);
        using var store = new ConfigStore(home.ConfigPath, watch: false);

        // Which "name" is the edit is exactly what devices disagree about, so it gets no revision.
        File.WriteAllText(home.ConfigPath, first.Text.Replace("\"name\": \"Beacon\"", "\"name\": \"Beacon\", \"name\": \"Other\""));
        store.Load();
        Assert.True(store.IsPendingEdit);
        Assert.Equal(first.Hash, store.Document!.Hash);
        Assert.Contains("written twice", store.Problem);
        Assert.IsType<ApplyResult.HeadMoved>(store.ApplyIfCurrent(Documents.Make("setup-a", 5, new[] { first.Hash }), first.Hash, "adopted"));

        // Fixed, it is an ordinary hand edit of the setup before it.
        File.WriteAllText(home.ConfigPath, first.Text.Replace("\"name\": \"Beacon\"", "\"name\": \"Other\""));
        store.Load();
        Assert.False(store.IsPendingEdit);
        Assert.Equal(5, store.Document!.Identity.RevisionNumber);
        Assert.Equal(first.Hash, store.Document.Identity.Lineage[0]);
    }

    [Fact]
    public void ApplyingIsACompareAndSwapAgainstTheFileItself()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        File.WriteAllBytes(home.ConfigPath, mine.Bytes);
        using var store = new ConfigStore(home.ConfigPath, watch: false);
        var theirs = Documents.Make("setup-a", 4, new[] { mine.Hash }, "Theirs");

        // Chosen against another head, or bytes that are not the ones that were verified.
        Assert.IsType<ApplyResult.HeadMoved>(store.ApplyIfCurrent(theirs, "0".PadLeft(64, '0'), "adopted"));
        Assert.IsType<ApplyResult.Refused>(store.ApplyIfCurrent(theirs, mine.Hash, "adopted", expectedHash: mine.Hash));

        // A file half way through being saved is an edit in progress, not the head to replace.
        File.WriteAllText(home.ConfigPath, "{ \"version\": 1, \"machines\": [");
        Assert.IsType<ApplyResult.HeadMoved>(store.ApplyIfCurrent(theirs, mine.Hash, "adopted"));
        Assert.Equal("{ \"version\": 1, \"machines\": [", File.ReadAllText(home.ConfigPath));

        File.WriteAllBytes(home.ConfigPath, mine.Bytes);
        Assert.IsType<ApplyResult.Applied>(store.ApplyIfCurrent(theirs, mine.Hash, "adopted", expectedHash: theirs.Hash));
        Assert.Equal(theirs.Bytes, File.ReadAllBytes(home.ConfigPath));
    }

    [Fact]
    public void WritesTheExampleOnlyIntoEmptySpace()
    {
        using var home = new TempHome();
        var store = new ConfigStore(home.ConfigPath, watch: false);
        Assert.Null(store.WriteExample());
        Assert.NotNull(store.Config);
        Assert.Contains("already", store.WriteExample());
    }
}

/// The shared document fixtures, read by the code that ships.
public class DocumentFixtureTests
{
    private static ControllerConfig Read(string name) =>
        ControllerConfig.From(Value.Parse(Repo.ReadFixture(name)));

    private static Acceptance Judged(string name) => DocumentAcceptance.Check(Repo.ReadFixture(name), Intake.Peer);

    /// Every document fixture, judged by section 9.7 under peer intake exactly as the index says:
    /// valid ones accepted, invalid ones refused. A client that refuses a valid document stops
    /// replicating it without anybody noticing.
    [Fact]
    public void EveryDocumentFixtureIsJudgedAsTheIndexSays()
    {
        var index = Value.Parse(Repo.ReadContract("fixtures/index.json"));
        var documents = index["fixtures"].AsArray()
            .Where(entry => entry["kind"].AsText() == "document" && entry["schema"].AsText() == "controller-document.schema.json")
            .ToList();
        Assert.True(documents.Count >= 10, "the document fixtures should not have shrunk to nothing");
        Assert.All(documents, entry =>
        {
            var verdict = Judged(entry["file"].AsText()!);
            Assert.True(verdict.Valid == (entry["expect"].AsText() == "valid"),
                $"{entry["file"].AsText()}: expected {entry["expect"].AsText()}, got {string.Join(", ", verdict.Errors.Select(error => error.Code))}");
        });
    }

    [Fact]
    public void ReadsTheSitesAndHelpersDocument()
    {
        var config = Read("controller-document.sites-helpers.json");
        Assert.True(Judged("controller-document.sites-helpers.json").Valid);
        Assert.Equal(2, config.Sites.Count);
        var legion = config.Machine("legion")!;
        Assert.Equal("attic", legion.Site);
        Assert.False(legion.AlwaysOn);
        Assert.Equal(2, legion.Wake!.EffectiveHelpers.Count);
        Assert.Equal("pi", legion.Wake.EffectiveHelpers[0].Machine);
        // The singular alias is the first of the list, for clients that read only that.
        Assert.Equal(legion.Wake.Helpers[0], legion.Wake.Helper);
        // A restricted key is written for the dispatcher whatever the platform says.
        Assert.True(config.Machine("pi")!.Systems[0].Restricted);
        Assert.Equal(RemoteShell.Dispatch, config.Machine("pi")!.Systems[0].Grammar.Shell);
        Assert.Equal(RemoteShell.Cmd, config.Machine("tower")!.System("windows")!.Grammar.Shell);
    }

    [Fact]
    public void KeepsKeysItHasNeverHeardOf()
    {
        var text = Repo.ReadFixture("controller-document.unknown-keys.json");
        var document = ControllerDocument.FromText(text);
        var plan = ControllerEditor.Plan(document.Text, document.Decoded, "rename",
            root => ((JsonObject)((JsonArray)root["machines"]!)[0]!)["name"] = "Renamed");
        Assert.True(plan.IsApplicable);
        // Everything the editor did not touch survives, top level and nested.
        Assert.Contains("futureTopLevelKey", plan.Text);
        Assert.Contains("futureMachineKey", plan.Text);
        Assert.Contains("futureEndpointKey", plan.Text);
        Assert.Contains("Renamed", plan.Text);
    }

    [Fact]
    public void RefusesTheInvalidDocumentsForTheRuleTheyBreak()
    {
        Assert.Contains(Judged("controller-document.invalid-helper-cycle.json").Errors, error => error.Code == "helper-cycle");
        Assert.Contains(Judged("controller-document.invalid-self-helper.json").Errors, error => error.Code == "helper-self");
        Assert.Contains(Judged("controller-document.invalid-unknown-site.json").Errors, error => error.Code == "unknown-site");
        Assert.Contains(Judged("controller-document.invalid-helper-mismatch.json").Errors, error => error.Code == "helper-alias");
        Assert.Contains(Judged("controller-document.invalid-host.json").Errors, error => error.Code == "schema");
    }

    [Fact]
    public void AHalfWrittenMachineIsAWarningRatherThanARefusal()
    {
        // The screen where the systems are added is the screen a refusal would take away.
        var verdict = Judged("controller-document.minimal.json");
        Assert.True(verdict.Valid);
        Assert.Contains(verdict.Warnings, warning => warning.Code == "machine-no-systems");
    }

    [Fact]
    public void WhatTheOtherClientsWriteIsAcceptedHere()
    {
        // Each of these was refused here before section 9.7: a site named in a setup with no
        // sites, two endpoints without an id, an endpoint without a user, and a wake block that
        // names helpers but no hardware address. None is a reason to stop replicating.
        var verdict = DocumentAcceptance.Check("""
            { "version": 1,
              "controller": { "id": "setup-a", "revision": 2, "lineage": [] },
              "machines": [
                { "id": "pi", "site": "attic", "alwaysOn": true,
                  "endpoints": [ { "host": "pi.example" }, { "host": "pi.example" } ],
                  "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ] },
                { "id": "tower", "site": "attic",
                  "endpoints": [ { "id": "lan", "host": "tower.example", "user": "me" } ],
                  "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ],
                  "wake": { "helpers": [ { "machine": "pi", "action": "wake-tower" } ] } } ] }
            """, Intake.Peer);
        Assert.True(verdict.Valid, string.Join(" ", verdict.Errors.Select(error => error.Message)));
        Assert.Contains(verdict.Warnings, warning => warning.Code == "wake-no-mac");
        Assert.Contains(verdict.Warnings, warning => warning.Code == "endpoint-no-id");

        // And the helper still wakes the machine with no hardware address of its own.
        var config = ControllerConfig.From(Value.Parse(DocumentJson.Print(DocumentJson.Parse("""
            { "version": 1, "machines": [ { "id": "pi" },
              { "id": "tower", "wake": { "helpers": [ { "machine": "pi", "action": "wake-tower" } ] } } ] }
            """))));
        var wake = config.Machine("tower")!.Wake!;
        Assert.Null(wake.MacBytes);
        Assert.Equal("pi", Assert.Single(wake.EffectiveHelpers).Machine);
    }

    [Fact]
    public void ReadsTheBindingsFixturesAndRefusesAShellOnTheLocalAgent()
    {
        var desktop = Value.Parse(Repo.ReadFixture("bindings.desktop-self.json"));
        Assert.Empty(Bindings.Problems(desktop));

        using var home = new TempHome();
        var path = home.Write("bindings.json", Repo.ReadFixture("bindings.desktop-self.json"));
        var bindings = Bindings.Load(path);
        Assert.Equal("Desk tower", bindings.DeviceName);
        Assert.Equal("tower", bindings.Self!.Machine);
        Assert.Equal(new[] { "node", "/home/me/.legion-control/agent/src/index.mjs" }, bindings.LocalAgent);
        Assert.True(bindings.CanRunLocally("tower"));
        Assert.False(bindings.CanRunLocally("pi"));
        Assert.Equal("~/.ssh/id_legion", bindings.IdentityFor("legion", null));
        Assert.Equal("~/.ssh/legion-control_ed25519", bindings.IdentityFor("pi", null));
        Assert.Equal("beacon", bindings.AliasFor("pi"));
        Assert.Equal("flat", bindings.CurrentSite);

        var invalid = Value.Parse(Repo.ReadFixture("bindings.invalid-local-agent-shell.json"));
        Assert.Contains(Bindings.Problems(invalid), problem => problem.Contains("shell"));
    }

    [Fact]
    public void APhoneCarriesNoLocalAgentAndThatIsFine()
    {
        using var home = new TempHome();
        var path = home.Write("bindings.json", Repo.ReadFixture("bindings.phone-minimal.json"));
        var bindings = Bindings.Load(path);
        Assert.Empty(bindings.LocalAgent);
        Assert.Null(bindings.Self);
        Assert.Equal("Pixel 9", bindings.Device);
    }
}

/// Merging two branches, entry by entry (contract section 9.8). The shared vectors pin each rule;
/// these are the cases a person would describe.
public class SetupMergeTests
{
    private const string Base = """
        {
          "version": 1,
          "controller": { "id": "setup-a", "revision": 1, "lineage": [] },
          "machines": [
            { "id": "pi", "name": "Beacon", "endpoints": [ { "id": "lan", "host": "10.0.0.5" } ] },
            { "id": "tower", "name": "Tower" }
          ]
        }
        """;

    private static readonly SetupMerge.Writer Writer = new("desktop", "test device", "2026-01-01T00:00:00.000Z");

    private static SetupMerge.Result Merge(string mine, string theirs, string? baseText = Base,
        IReadOnlyDictionary<string, SetupChoice>? choices = null) =>
        SetupMerge.Merge(baseText, mine, theirs, Canonical.Hash(Encoding.UTF8.GetBytes(mine)),
            Canonical.Hash(Encoding.UTF8.GetBytes(theirs)), Writer, choices);

    [Fact]
    public void KeepsBothSidesWhenTheyChangedDifferentThings()
    {
        var mine = Base.Replace("\"Beacon\"", "\"Beacon renamed here\"");
        var theirs = Base.Replace("\"Tower\"", "\"Tower renamed there\"");

        var result = Merge(mine, theirs);
        Assert.Equal(new[] { new SetupDifference("machines[pi].fields", SetupChange.Mine), new SetupDifference("machines[tower].fields", SetupChange.Theirs) },
            result.Differences);
        Assert.DoesNotContain(result.Differences, difference => difference.NeedsChoice);
        Assert.Contains("Beacon renamed here", result.Text);
        Assert.Contains("Tower renamed there", result.Text);
    }

    [Fact]
    public void AsksWhenBothSidesChangedTheSameThing()
    {
        var mine = Base.Replace("\"Beacon\"", "\"Beacon mine\"");
        var theirs = Base.Replace("\"Beacon\"", "\"Beacon theirs\"");

        var contested = Assert.Single(Merge(mine, theirs).Differences);
        Assert.True(contested.NeedsChoice);

        // The default is theirs: the side already on the machines is not the one to overwrite by
        // accident.
        Assert.Contains("Beacon theirs", Merge(mine, theirs).Text);
        Assert.Contains("Beacon mine", Merge(mine, theirs, choices: new Dictionary<string, SetupChoice> { [contested.Entry] = SetupChoice.Mine }).Text);
    }

    [Fact]
    public void ADeletionAgainstAnEditIsAQuestionAndNeverDecidedSilently()
    {
        // Mine took tower out; theirs gave it an address. The merge before section 9.8 dropped the
        // edit without saying so.
        var root = JsonNode.Parse(Base)!.AsObject();
        root["machines"]!.AsArray().RemoveAt(1);
        var mine = root.ToJsonString();
        var theirs = Base.Replace("{ \"id\": \"tower\", \"name\": \"Tower\" }",
            "{ \"id\": \"tower\", \"name\": \"Tower\", \"endpoints\": [ { \"id\": \"lan\", \"host\": \"10.0.0.7\" } ] }");
        var difference = Assert.Single(Merge(mine, theirs).Differences);
        Assert.Equal(new SetupDifference("machines[tower]", SetupChange.Both), difference);
        Assert.Contains("10.0.0.7", Merge(mine, theirs).Text);
        Assert.DoesNotContain("\"tower\"", Merge(mine, theirs, choices: new Dictionary<string, SetupChoice> { ["machines[tower]"] = SetupChoice.Mine }).Text);
    }

    [Fact]
    public void WithoutACommonAncestorEveryDifferenceIsAQuestion()
    {
        var mine = Base.Replace("\"Beacon\"", "\"Beacon mine\"");
        var theirs = Base.Replace("\"Tower\"", "\"Tower theirs\"");
        var differences = Merge(mine, theirs, baseText: null).Differences;
        Assert.Equal(2, differences.Count);
        Assert.All(differences, difference => Assert.Equal(SetupChange.NoBase, difference.Kind));
        Assert.All(differences, difference => Assert.True(difference.NeedsChoice));
    }

    [Fact]
    public void AMachineAddedOnOneSideLandsAfterItsPredecessorThere()
    {
        // Dial and wake order follow the list, so what theirs added goes where theirs put it.
        var theirs = Base.Replace(
            "{ \"id\": \"pi\", \"name\": \"Beacon\", \"endpoints\": [ { \"id\": \"lan\", \"host\": \"10.0.0.5\" } ] },",
            "{ \"id\": \"pi\", \"name\": \"Beacon\", \"endpoints\": [ { \"id\": \"lan\", \"host\": \"10.0.0.5\" } ] }, { \"id\": \"nas\", \"name\": \"NAS\" },");
        var merged = DocumentJson.Parse(Merge(Base, theirs).Text!);
        var ids = ((DocumentArray)((DocumentObject)merged)["machines"]!).Items.Select(machine => ((DocumentString)((DocumentObject)machine)["id"]!).Value);
        Assert.Equal(new[] { "pi", "nas", "tower" }, ids);
    }

    [Fact]
    public void UnknownKeysAreEntriesOfTheirOwnAndKeepTheirSpelling()
    {
        var mine = Base.Replace("\"version\": 1,", "\"version\": 1, \"weight\": 22.0, \"notes\": \"here\",");
        var theirs = Base.Replace("\"version\": 1,", "\"version\": 1, \"weight\": 2.2e1, \"notes\": \"there\",");
        var result = Merge(mine, theirs);
        // Two spellings of one number are one value; the note both added is asked about.
        Assert.Equal(new SetupDifference("notes", SetupChange.Both), Assert.Single(result.Differences));
        Assert.Contains("\"weight\": 22.0", result.Text);
    }

    [Fact]
    public void TheMergedDocumentDescendsFromBothAndIsValid()
    {
        var mine = Base.Replace("\"Beacon\"", "\"Beacon mine\"");
        var theirs = Base.Replace("\"Tower\"", "\"Tower theirs\"");
        var merged = ControllerDocument.FromText(Merge(mine, theirs).Text!);
        Assert.Contains(Canonical.Hash(Encoding.UTF8.GetBytes(mine)), merged.Identity.Lineage);
        Assert.Contains(Canonical.Hash(Encoding.UTF8.GetBytes(theirs)), merged.Identity.Lineage);
        Assert.Equal(2, merged.Identity.RevisionNumber);
        Assert.True(DocumentAcceptance.Check(merged.Bytes, Intake.Peer).Valid);
    }

    [Fact]
    public void MergesAMegabyteOfNamesInTimeThatGrowsWithThem()
    {
        // Every unknown top-level key is an entry of its own: looked up on three sides, compared,
        // and taken one by one. Taking one looked through every member of the merged document
        // before, so taking them all cost the square of their number.
        //
        // A wall-clock bound alone is either loose enough to pass a square on a fast machine or
        // tight enough to fail a slow runner that did nothing wrong, so the growth is checked too:
        // four times the names may take less than ten times as long, where linear is four and a
        // square sixteen. Each size counts its faster of two runs, the one least disturbed by
        // whatever else the machine was doing, and the smaller runs first, which warms the code up
        // as well.
        const int count = 100_000;
        SetupMerge.Result? result = null;
        TimeSpan Fastest(int size)
        {
            var mine = DocumentJsonTests.Names(size).Replace("\"machines\":[]",
                "\"controller\":{\"id\":\"setup-a\",\"revision\":1,\"lineage\":[]},\"machines\":[]");
            var root = (DocumentObject)DocumentJson.Parse(mine);
            for (var index = 0; index < size; index += 1)
            {
                if (index % 2 == 0) root.Remove(DocumentJsonTests.Name(index));
                else root.Set(DocumentJsonTests.Name(index), new DocumentNumber("1"));
            }
            var theirs = DocumentJson.Print(root);
            var fastest = TimeSpan.MaxValue;
            for (var run = 0; run < 2; run += 1)
            {
                var watch = Stopwatch.StartNew();
                result = Merge(mine, theirs, baseText: mine);
                if (watch.Elapsed < fastest) fastest = watch.Elapsed;
            }
            return fastest;
        }

        var quarter = Fastest(count / 4);
        var whole = Fastest(count);
        Assert.True(whole < TimeSpan.FromSeconds(30), $"the merge took {whole}");
        Assert.True(whole < quarter * 10, $"the merge took {whole}, and {quarter} for a quarter of the names");
        Assert.Equal(count, result!.Differences.Count);
        Assert.All(result.Differences, difference => Assert.Equal(SetupChange.Theirs, difference.Kind));
        var merged = (DocumentObject)DocumentJson.Parse(result.Text!);
        Assert.False(merged.Has(DocumentJsonTests.Name(0)));
        Assert.Equal("1", ((DocumentNumber)merged[DocumentJsonTests.Name(count - 1)]!).Lexeme);
        Assert.Equal(3 + count / 2, merged.Members.Count);
    }

    [Fact]
    public void AMergeThatWouldBreakARuleIsNotWritten()
    {
        // Theirs makes pi tower's wake helper; mine takes pi out. Keeping mine's side of pi leaves a
        // helper that names nothing, so there is no result, and the rule is named.
        var mine = Base.Replace("{ \"id\": \"pi\", \"name\": \"Beacon\", \"endpoints\": [ { \"id\": \"lan\", \"host\": \"10.0.0.5\" } ] },", "");
        var theirs = Base.Replace("{ \"id\": \"tower\", \"name\": \"Tower\" }",
            "{ \"id\": \"tower\", \"name\": \"Tower\", \"wake\": { \"mac\": \"02:00:00:00:00:01\", \"helpers\": [ { \"machine\": \"pi\", \"action\": \"wake-tower\" } ] } }")
            .Replace("\"Beacon\"", "\"Beacon there\"");
        var result = Merge(mine, theirs, choices: new Dictionary<string, SetupChoice> { ["machines[pi]"] = SetupChoice.Mine });
        Assert.Null(result.Text);
        Assert.Equal("helper-unknown", result.Error);
    }
}
