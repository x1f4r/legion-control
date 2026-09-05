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
        string setupId, long revision, IReadOnlyList<string> lineage, string machineName = "Atlas",
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

/// Ancestry, which is the only thing that can tell "you are behind" from "we both edited".
public class LineageTests
{
    [Fact]
    public void TheSameBytesAreTheSameDocument()
    {
        var mine = Documents.Make("setup-a", 5, Array.Empty<string>());
        Assert.Equal(Descent.Same, SetupLineage.Decide(mine.Hash, mine.Identity, mine.Hash, mine.Identity));
    }

    [Fact]
    public void AMachineHoldingAnAncestorIsBehind()
    {
        var parent = Documents.Make("setup-a", 5, Array.Empty<string>());
        var child = Documents.Make("setup-a", 6, new[] { parent.Hash });
        Assert.Equal(Descent.TheyAreBehind,
            SetupLineage.Decide(child.Hash, child.Identity, parent.Hash, parent.Identity));
        // And status alone is enough to say so, because the ancestor is in my own lineage.
        Assert.Equal(Descent.TheyAreBehind,
            SetupLineage.DecideFromStatus(child.Hash, child.Identity, parent.Hash));
    }

    [Fact]
    public void AMachineHoldingADescendantMeansThisDeviceIsBehind()
    {
        var parent = Documents.Make("setup-a", 5, Array.Empty<string>());
        var child = Documents.Make("setup-a", 6, new[] { parent.Hash });
        Assert.Equal(Descent.IAmBehind,
            SetupLineage.Decide(parent.Hash, parent.Identity, child.Hash, child.Identity));
        // Status cannot say that on its own: my lineage does not mention the child.
        Assert.Equal(Descent.Unknown,
            SetupLineage.DecideFromStatus(parent.Hash, parent.Identity, child.Hash));
    }

    [Fact]
    public void TwoEditsOfTheSameRevisionAreDivergentAndNotOrdered()
    {
        // The whole reason revision numbers cannot decide this: both are revision 6.
        var parent = Documents.Make("setup-a", 5, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 6, new[] { parent.Hash }, "Atlas one");
        var theirs = Documents.Make("setup-a", 6, new[] { parent.Hash }, "Atlas two");
        Assert.NotEqual(mine.Hash, theirs.Hash);
        Assert.Equal(6, mine.Identity.RevisionNumber);
        Assert.Equal(6, theirs.Identity.RevisionNumber);
        Assert.Equal(Descent.Diverged, SetupLineage.Decide(mine.Hash, mine.Identity, theirs.Hash, theirs.Identity));
    }

    [Fact]
    public void TwoDifferentSetupsAreNeverOrdered()
    {
        var mine = Documents.Make("setup-a", 9, Array.Empty<string>());
        var theirs = Documents.Make("setup-b", 2, Array.Empty<string>());
        Assert.Equal(Descent.DifferentSetup, SetupLineage.Decide(mine.Hash, mine.Identity, theirs.Hash, theirs.Identity));
    }

    [Fact]
    public void AMachineHoldingNothingOrALegacyCopyIsSafeToPublishTo()
    {
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        Assert.Equal(Descent.TheyHaveNothing, SetupLineage.Decide(mine.Hash, mine.Identity, null, null));
        Assert.Equal(Descent.TheyHaveNothing,
            SetupLineage.Decide(mine.Hash, mine.Identity, "0".PadLeft(64, '0'), new ControllerIdentity()));
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
        var merged = ControllerIdentity.Merged(mine.Identity, mine.Hash, theirs.Identity, theirs.Hash, "device");
        Assert.Equal(8, merged.RevisionNumber);
        Assert.Contains(mine.Hash, merged.Lineage);
        Assert.Contains(theirs.Hash, merged.Lineage);
        // So both sides fast-forward to it rather than either being overwritten.
        Assert.Equal(Descent.TheyAreBehind,
            SetupLineage.Decide("merged", merged, mine.Hash, mine.Identity));
        Assert.Equal(Descent.TheyAreBehind,
            SetupLineage.Decide("merged", merged, theirs.Hash, theirs.Identity));
    }

    [Fact]
    public void ARubbishLineageIsRefusedRatherThanCarried()
    {
        var identity = new ControllerIdentity { Id = "setup-a", Revision = 1, Lineage = new[] { "not-a-hash" } };
        Assert.NotEmpty(identity.Problems());
        var duplicated = new ControllerIdentity
        {
            Id = "setup-a",
            Lineage = new[] { "a".PadLeft(64, 'a'), "a".PadLeft(64, 'a') },
        };
        Assert.Contains(duplicated.Problems(), problem => problem.Contains("repeats"));
        var tooMany = new ControllerIdentity
        {
            Id = "setup-a",
            Lineage = Enumerable.Range(0, 33).Select(index => index.ToString().PadLeft(64, '0')).ToList(),
        };
        Assert.Contains(tooMany.Problems(), problem => problem.Contains("at most"));
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
        Assert.Equal(newer.Hash, ledger.CommonBase(mine, theirs));

        // A common hash whose bytes are gone is no use to a merge, and is not offered as one.
        Assert.Null(ledger.CommonBase(new[] { "y".PadLeft(64, 'y') }, new[] { "y".PadLeft(64, 'y') }));
    }
}

/// The setup file on disk: how it is read, and what happens when a person edits it by hand.
public class ConfigStoreTests
{
    [Theory]
    [InlineData("{\"id\":\"setup-a\",\"revision\":\"3\"}")]
    [InlineData("{\"id\":\"setup-a\",\"revision\":3,\"lineage\":[42]}")]
    [InlineData("{\"revision\":3}")]
    public void MalformedIdentityCannotBeAdoptedAsAnUnrevisionedSetup(string identity)
    {
        var config = ControllerConfig.From(Value.Parse($"{{\"version\":1,\"machines\":[],\"controller\":{identity}}}"));
        Assert.NotEmpty(config.Problems());
    }
    [Fact]
    public void RestampingKeepsUnknownControllerFields()
    {
        using var home = new TempHome();
        var raw = JsonNode.Parse(Documents.Make("setup-a", 3, Array.Empty<string>()).Text)!.AsObject();
        raw["controller"]!["future"] = new JsonObject { ["nested"] = true };
        File.WriteAllText(home.ConfigPath, raw.ToJsonString());
        using var store = new ConfigStore(home.ConfigPath, watch: false);
        var (edited, problem) = store.ApplyEdit(store.Document!.Text.Replace("Atlas", "Renamed"));
        Assert.Null(problem);
        Assert.True(JsonNode.Parse(edited!.Text)!["controller"]!["future"]!["nested"]!.GetValue<bool>());
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
            { "version": 1, "machines": [ { "id": "pi", "name": "Atlas" } ] }
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
        var edited = File.ReadAllText(home.ConfigPath).Replace("\"Atlas\"", "\"Atlas renamed\"");
        File.WriteAllText(home.ConfigPath, edited);
        store.Load();

        Assert.Equal(5, store.Document!.Identity.RevisionNumber);
        Assert.Equal(firstHash, store.Document.Identity.Lineage[0]);
        Assert.Equal("Atlas renamed", store.Config!.Machines[0].Name);
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
            File.ReadAllText(home.ConfigPath).Replace("\"Atlas\"", "\"Atlas edited offline\""));

        var after = new ConfigStore(home.ConfigPath, statePath, ledger, "tower", watch: false);
        Assert.Equal(8, after.Document!.Identity.RevisionNumber);
        Assert.Equal(appliedHash, after.Document.Identity.Lineage[0]);
        Assert.Equal("Atlas edited offline", after.Config!.Machines[0].Name);
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

    [Fact]
    public void ReadsTheSitesAndHelpersDocument()
    {
        var config = Read("controller-document.sites-helpers.json");
        Assert.Empty(config.Problems());
        Assert.Equal(2, config.Sites.Count);
        var legion = config.Machine("legion")!;
        Assert.Equal("attic", legion.Site);
        Assert.False(legion.AlwaysOn);
        Assert.Equal(2, legion.Wake!.EffectiveHelpers.Count);
        Assert.Equal("pi", legion.Wake.EffectiveHelpers[0].Machine);
        // The singular alias is the first of the list, for clients that read only that.
        Assert.Equal(legion.Wake.Helpers[0], legion.Wake.Helper);
        // A restricted key is serialised as POSIX argv whatever the platform says.
        Assert.True(config.Machine("pi")!.Systems[0].Restricted);
        Assert.Equal(RemoteShell.Posix, config.Machine("pi")!.Systems[0].RemoteShell);
        Assert.Equal(RemoteShell.Cmd, config.Machine("tower")!.System("windows")!.RemoteShell);
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
    public void RefusesTheInvalidDocuments()
    {
        Assert.Contains(Read("controller-document.invalid-helper-cycle.json").Problems(),
            problem => problem.Contains("cycle"));
        Assert.Contains(Read("controller-document.invalid-self-helper.json").Problems(),
            problem => problem.Contains("itself"));
        Assert.Contains(Read("controller-document.invalid-unknown-site.json").Problems(),
            problem => problem.Contains("not in sites"));
        Assert.Contains(Read("controller-document.invalid-helper-mismatch.json").Problems(),
            problem => problem.Contains("have to agree"));
    }

    [Fact]
    public void AHalfWrittenMachineIsAWarningRatherThanARefusal()
    {
        // The screen where the systems are added is the screen a refusal would take away.
        var config = Read("controller-document.minimal.json");
        Assert.Empty(config.Problems());
        Assert.Contains(config.Warnings(), warning => warning.Contains("no systems"));
    }

    [Fact]
    public void ReadsTheBindingsFixturesAndRefusesAShellOnTheLocalAgent()
    {
        var desktop = Value.Parse(Repo.ReadFixture("bindings.desktop-self.json"));
        Assert.Empty(Bindings.Problems(desktop));

        using var home = new TempHome();
        var path = home.Write("bindings.json", Repo.ReadFixture("bindings.desktop-self.json"));
        var bindings = Bindings.Load(path);
        Assert.Equal("Robert's tower", bindings.DeviceName);
        Assert.Equal("tower", bindings.Self!.Machine);
        Assert.Equal(new[] { "node", "/home/x1f4r/.legion-control/agent/src/index.mjs" }, bindings.LocalAgent);
        Assert.True(bindings.CanRunLocally("tower"));
        Assert.False(bindings.CanRunLocally("pi"));
        Assert.Equal("~/.ssh/id_legion", bindings.IdentityFor("legion", null));
        Assert.Equal("~/.ssh/legion-control_ed25519", bindings.IdentityFor("pi", null));
        Assert.Equal("atlas", bindings.AliasFor("pi"));
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

/// Merging two branches, entry by entry.
public class SetupMergeTests
{
    private static ControllerDocument Document(string json) => ControllerDocument.FromText(json);

    private const string Base = """
        {
          "version": 1,
          "controller": { "id": "setup-a", "revision": 1, "lineage": [] },
          "machines": [
            { "id": "pi", "name": "Atlas", "endpoints": [ { "id": "lan", "host": "10.0.0.5" } ] },
            { "id": "tower", "name": "Tower" }
          ]
        }
        """;

    [Fact]
    public void KeepsBothSidesWhenTheyChangedDifferentThings()
    {
        var baseDocument = Document(Base);
        var mine = Document(Base.Replace("\"Atlas\"", "\"Atlas renamed here\""));
        var theirs = Document(Base.Replace("\"Tower\"", "\"Tower renamed there\""));

        var differences = SetupMerge.Compare(mine, theirs, baseDocument);
        Assert.Equal(2, differences.Count);
        Assert.Contains(differences, difference => difference.Change == SetupChange.Mine);
        Assert.Contains(differences, difference => difference.Change == SetupChange.Theirs);
        Assert.DoesNotContain(differences, difference => difference.NeedsChoice);

        var merged = SetupMerge.Merge(mine, mine.Hash, theirs, theirs.Hash, baseDocument, null, "device");
        Assert.Contains("Atlas renamed here", merged.Text);
        Assert.Contains("Tower renamed there", merged.Text);
    }

    [Fact]
    public void AsksWhenBothSidesChangedTheSameThing()
    {
        var baseDocument = Document(Base);
        var mine = Document(Base.Replace("\"Atlas\"", "\"Atlas mine\""));
        var theirs = Document(Base.Replace("\"Atlas\"", "\"Atlas theirs\""));

        var differences = SetupMerge.Compare(mine, theirs, baseDocument);
        var contested = Assert.Single(differences);
        Assert.True(contested.NeedsChoice);

        // The default is theirs: the side already on the machines is not the one to overwrite by
        // accident.
        var byDefault = SetupMerge.Merge(mine, mine.Hash, theirs, theirs.Hash, baseDocument, null, "device");
        Assert.Contains("Atlas theirs", byDefault.Text);

        var chosen = SetupMerge.Merge(mine, mine.Hash, theirs, theirs.Hash, baseDocument,
            new Dictionary<string, SetupChoice> { [contested.Key] = SetupChoice.Mine }, "device");
        Assert.Contains("Atlas mine", chosen.Text);
    }

    [Fact]
    public void WithoutACommonAncestorEveryDifferenceIsAQuestion()
    {
        var mine = Document(Base.Replace("\"Atlas\"", "\"Atlas mine\""));
        var theirs = Document(Base.Replace("\"Tower\"", "\"Tower theirs\""));
        var differences = SetupMerge.Compare(mine, theirs, null);
        Assert.All(differences, difference => Assert.True(difference.NeedsChoice));
    }

    [Fact]
    public void AMachineAddedOnOneSideSurvivesTheMerge()
    {
        var baseDocument = Document(Base);
        var mine = Document(Base);
        var theirs = Document(Base.Replace(
            "{ \"id\": \"tower\", \"name\": \"Tower\" }",
            "{ \"id\": \"tower\", \"name\": \"Tower\" }, { \"id\": \"nas\", \"name\": \"NAS\" }"));
        var merged = SetupMerge.Merge(mine, mine.Hash, theirs, theirs.Hash, baseDocument, null, "device");
        Assert.Contains("\"nas\"", merged.Text);
        Assert.Contains("\"pi\"", merged.Text);
    }

    [Fact]
    public void TheMergedDocumentDescendsFromBothAndIsValid()
    {
        var baseDocument = Document(Base);
        var mine = Document(Base.Replace("\"Atlas\"", "\"Atlas mine\""));
        var theirs = Document(Base.Replace("\"Tower\"", "\"Tower theirs\""));
        var merged = SetupMerge.Merge(mine, mine.Hash, theirs, theirs.Hash, baseDocument, null, "device");
        Assert.Contains(mine.Hash, merged.Identity.Lineage);
        Assert.Contains(theirs.Hash, merged.Identity.Lineage);
        Assert.Empty(merged.Decoded.Problems());
    }
}
