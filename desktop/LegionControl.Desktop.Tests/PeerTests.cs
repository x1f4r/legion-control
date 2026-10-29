using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// A machine that answers with whatever document it is currently holding.
///
/// This is the other half of the peer story: the client's rules are only worth anything against an
/// agent that enforces the same descent rule, so the fake does exactly that. Every setup reply is
/// one of the shared fixtures with the held document written into it, and written out compactly the
/// way an agent prints it, so a client that hashed the reply's own spacing of `controller` rather
/// than `raw` fails here as it fails against a real agent.
internal sealed class FakeMachine
{
    public ControllerDocument? Held { get; set; }
    public int Contract { get; set; } = 3;
    public string AgentVersion { get; set; } = "3.1.0";
    /// False for an agent before 3.1.0, which sends `controller` and no `raw`.
    public bool SendsRaw { get; set; } = true;
    public bool Running { get; set; }
    /// Fixtures the next pushes are refused with, one per push, before the descent rule is asked.
    public Queue<string> Refusals { get; } = new();
    public int MetaReads { get; private set; }
    public int Reads { get; private set; }
    public Action? OnRead { get; set; }
    public Action? OnMeta { get; set; }
    public List<string> Pushes { get; } = new();
    public bool SawReplace { get; private set; }

    public IProcessRunner Runner => new FakeRunner(Answer);

    private static readonly JsonSerializerOptions AsAnAgentPrints = new()
    {
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
        // An agent prints a document of any depth, one level down in its reply.
        MaxDepth = DocumentJson.MaxDepth + 1,
    };

    private CommandResult Answer(FakeCall call)
    {
        var command = call.RemoteCommand;
        if (command.Contains("config set")) return Store(call);
        if (command.Contains("config meta")) return Meta();
        if (command.EndsWith(" config", StringComparison.Ordinal)) return Read();
        return Status();
    }

    /// A shared fixture with [change] made to it, printed compactly.
    private static CommandResult Reply(string fixture, Action<JsonObject>? change = null)
    {
        var reply = JsonNode.Parse(Repo.ReadFixture(fixture + ".json"))!.AsObject();
        change?.Invoke(reply);
        return FakeRunner.Ok(reply.ToJsonString(AsAnAgentPrints));
    }

    private JsonObject Mark(ControllerDocument document) => new()
    {
        ["id"] = document.Identity.Id,
        ["revision"] = document.Identity.RevisionNumber,
        ["updatedAt"] = document.Identity.UpdatedAt,
        ["source"] = document.Identity.Source,
        ["lineage"] = new JsonArray(document.Identity.Lineage.Select(hash => (JsonNode)hash!).ToArray()),
        ["device"] = document.Identity.Device,
        ["hash"] = document.Hash,
        ["bytes"] = document.Bytes.Length,
    };

    private void Stamp(JsonObject reply)
    {
        reply["agentVersion"] = AgentVersion;
        reply["contract"] = Contract;
    }

    private CommandResult Status()
    {
        // A 3.x agent that holds nothing still reports the block, with no hash (status.no-services).
        var mark = Held is null
            ? """{ "hash": null, "id": null, "revision": 0, "updatedAt": null, "source": null }"""
            : $$"""{ "hash": "{{Held.Hash}}", "id": "{{Held.Identity.Id}}", "revision": {{Held.Identity.RevisionNumber}}, "source": "cli" }""";
        var running = Running
            ? """{ "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "kind": "update", "state": "running", "phase": "installing" }"""
            : "";
        return FakeRunner.Ok($$"""
            { "ok": true, "contract": {{Contract}}, "agentVersion": "{{AgentVersion}}", "system": { "id": "linux" },
              "controller": {{mark}},
              "operations": { "running": [{{running}}], "queued": [], "recent": [] } }
            """);
    }

    private CommandResult Meta()
    {
        MetaReads++;
        var held = Held;
        OnMeta?.Invoke();
        if (held is null) return Reply("config.meta-empty", Stamp);
        return Reply("config.meta", reply =>
        {
            Stamp(reply);
            reply["meta"] = Mark(held);
            reply["hash"] = held.Hash;
        });
    }

    private CommandResult Read()
    {
        Reads++;
        OnRead?.Invoke();
        if (Held is not { } held) return Reply("config.read-empty", Stamp);
        return Reply(SendsRaw ? "config.read" : "config.read-without-raw", reply =>
        {
            Stamp(reply);
            reply["controller"] = JsonNode.Parse(held.Text, documentOptions: DocumentJson.Nesting);
            if (SendsRaw) reply["raw"] = held.Text;
            reply["hash"] = held.Hash;
            reply["bytes"] = held.Bytes.Length;
            reply["meta"] = Mark(held);
        });
    }

    /// The agent's own acceptance rule, which is the thing the client's rules have to agree with.
    private CommandResult Store(FakeCall call)
    {
        var replace = call.Arguments.Any(argument => argument.Contains("--replace"));
        if (replace) SawReplace = true;
        ControllerDocument pushed;
        try
        {
            pushed = ControllerDocument.FromRaw(call.Input ?? Array.Empty<byte>());
        }
        catch (Canonical.NotUtf8)
        {
            return Reply("error.bad-argument");
        }
        Pushes.Add(pushed.Hash);
        var held = Held;
        void Pushed(JsonObject reply)
        {
            Stamp(reply);
            reply["id"] = pushed.Identity.Id;
            reply["revision"] = pushed.Identity.RevisionNumber;
            if (held is not null) reply["current"] = Mark(held);
        }
        void Stored(JsonObject reply)
        {
            Stamp(reply);
            reply["hash"] = pushed.Hash;
            reply["bytes"] = pushed.Bytes.Length;
            reply["id"] = pushed.Identity.Id;
            reply["revision"] = pushed.Identity.RevisionNumber;
            reply["source"] = pushed.Identity.Source;
            reply["lineage"] = new JsonArray(pushed.Identity.Lineage.Select(hash => (JsonNode)hash!).ToArray());
        }
        if (Refusals.TryDequeue(out var refusal)) return Reply(refusal, Pushed);

        if (held is null)
        {
            Held = pushed;
            return Reply("config.set.stored", Stored);
        }
        if (held.Hash == pushed.Hash) return Reply("config.set.noop", Stored);
        if (held.Identity.Id != pushed.Identity.Id)
        {
            if (!replace) return Reply("config.set.conflict-other-id", Pushed);
            Held = pushed;
            return Reply("config.set.replaced", Stored);
        }
        if (pushed.Identity.Lineage.Contains(held.Hash))
        {
            Held = pushed;
            return Reply("config.set.stored", Stored);
        }
        if (held.Identity.Lineage.Contains(pushed.Hash)) return Reply("config.set.stale-revision", Pushed);
        if (replace)
        {
            Held = pushed;
            return Reply("config.set.replaced", Stored);
        }
        return Reply("config.set.conflict-divergent", Pushed);
    }
}

/// Reconciliation: what happens on its own, and what stops and asks.
public class ReconcilerTests
{
    private static (ConfigStore Store, MachineModel Model, FakeMachine Machine) Build(
        TempHome home, ControllerDocument mine, OperationTracker? tracker = null)
    {
        File.WriteAllBytes(home.ConfigPath, mine.Bytes);
        var store = Store(home);
        var fake = new FakeMachine();
        var machine = new MachineModel(
            store.Config!.Machines[0], fake.Runner,
            tracker ?? new OperationTracker(Path.Combine(home.Path, "operations.json")));
        return (store, machine, fake);
    }

    /// The store a process on this device keeps over the shared setup files.
    private static ConfigStore Store(TempHome home) => new(home.ConfigPath,
        statePath: Path.Combine(home.Path, "setup-state.json"),
        ledger: new SetupLedger(Path.Combine(home.Path, "revisions")),
        deviceName: "test device",
        watch: false);

    /// A document written the way JSON.stringify(document, null, 2) writes it, which is the one
    /// spelling an agent before 3.1.0 lets a client rebuild.
    private static ControllerDocument AsJavaScriptWritesIt(ControllerDocument document) =>
        ControllerDocument.FromText(DocumentJson.Print(DocumentJson.Parse(document.Text)));

    /// The same document spelled in a way no client's rebuild reproduces: compact, with a name
    /// escaped that JSON.stringify writes as itself.
    private static ControllerDocument Unrebuildable(ControllerDocument document) =>
        ControllerDocument.FromText(JsonNode.Parse(document.Text)!.ToJsonString().Replace("Beacon", "B\\u0065acon"));

    [Fact]
    public async Task PublishesToAMachineThatHoldsNothing()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var outcome = await reconciler.ReconcileAsync(model);
        Assert.IsType<ReconcileOutcome.Published>(outcome);
        Assert.Equal(store.Document!.Hash, fake.Held!.Hash);
    }

    [Fact]
    public async Task PublishesToAMachineThatHoldsAnAncestor()
    {
        using var home = new TempHome();
        var parent = Documents.Make("setup-a", 3, Array.Empty<string>());
        var child = Documents.Make("setup-a", 4, new[] { parent.Hash }, "Beacon renamed");
        var (store, model, fake) = Build(home, child);
        fake.Held = parent;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Published>(await reconciler.ReconcileAsync(model));
        Assert.Equal(child.Hash, fake.Held!.Hash);
        // One push, not one per poll.
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.InSync>(await reconciler.ReconcileAsync(model));
        Assert.Single(fake.Pushes);
    }

    [Fact]
    public async Task AdoptsADescendantWithoutAsking()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 4, Array.Empty<string>());
        var theirs = Documents.Make("setup-a", 5, new[] { mine.Hash }, "Beacon from elsewhere");
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var outcome = await reconciler.ReconcileAsync(model);
        Assert.IsType<ReconcileOutcome.Adopted>(outcome);
        Assert.Equal(theirs.Hash, store.Document!.Hash);
        Assert.Equal("Beacon from elsewhere", store.Config!.Machines[0].Name);
        // The document that was replaced is still there to go back to.
        Assert.True(store.Ledger.Has(mine.Hash));
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task AdoptsTheAgentsRawTextByteForByte()
    {
        // A document whose stored spelling no rebuild of the parsed `controller` reproduces. The
        // desktop used to hash the reply's own spacing of `controller`, which matches nothing an
        // agent stores, so it could never adopt a peer's document at all.
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 4, Array.Empty<string>());
        var theirs = Unrebuildable(Documents.Make("setup-a", 5, new[] { mine.Hash }, "Beacon from elsewhere"));
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Adopted>(await reconciler.ReconcileAsync(model));
        Assert.Equal(theirs.Bytes, store.Document!.Bytes);
        Assert.Equal(theirs.Bytes, File.ReadAllBytes(home.ConfigPath));
    }

    [Fact]
    public async Task AnAgentWithoutRawIsAdoptedFromWhenItsDocumentRebuildsExactly()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 4, Array.Empty<string>());
        var theirs = AsJavaScriptWritesIt(Documents.Make("setup-a", 5, new[] { mine.Hash }, "Büro's +1 tower 🎉"));
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        fake.SendsRaw = false;
        fake.AgentVersion = "3.0.1";
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Adopted>(await reconciler.ReconcileAsync(model));
        Assert.Equal(theirs.Hash, store.Document!.Hash);
    }

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task ADocumentAsDeepAsAcceptanceAllowsIsAdoptedFromAMachine(bool sendsRaw)
    {
        // A key nobody knows, nested as deep as section 9.7 lets this app hold a document. The
        // client refused it at 65 levels, where validate.mjs and the agent read on, and a peer's
        // valid document was then never adopted here.
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 4, Array.Empty<string>());
        var theirs = ControllerDocument.FromText(DocumentJsonTests.Deep(DocumentJson.MaxDepth,
            Documents.Make("setup-a", 5, new[] { mine.Hash }, "Beacon from elsewhere")));
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        fake.SendsRaw = sendsRaw;
        if (!sendsRaw) fake.AgentVersion = "3.0.1";
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Adopted>(await reconciler.ReconcileAsync(model));
        Assert.Equal(theirs.Hash, store.Document!.Hash);
        Assert.Equal("Beacon from elsewhere", store.Config!.Machines[0].Name);
    }

    [Fact]
    public async Task AnUnverifiableDocumentIsNeitherAdoptedNorReadAgainUntilTheAgentChanges()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 4, Array.Empty<string>());
        var theirs = Unrebuildable(Documents.Make("setup-a", 5, new[] { mine.Hash }, "Beacon from elsewhere"));
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        fake.SendsRaw = false;
        fake.AgentVersion = "3.0.1";
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var failed = Assert.IsType<ReconcileOutcome.Failed>(await reconciler.ReconcileAsync(model));
        Assert.Contains("updated", failed.Sentence);
        Assert.Equal(mine.Hash, store.Document!.Hash);
        Assert.Equal(1, fake.Reads);

        // Remembered per agent: the same document is not read again on every poll ...
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Failed>(await reconciler.ReconcileAsync(model, now: DateTimeOffset.UtcNow.AddMinutes(5)));
        Assert.Equal(1, fake.Reads);

        // ... and read again once the agent is one that sends `raw`.
        fake.AgentVersion = "3.1.0";
        fake.SendsRaw = true;
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Adopted>(await reconciler.ReconcileAsync(model));
        Assert.Equal(2, fake.Reads);
    }

    [Fact]
    public void TheSharedConfigReadFixtureIsAdoptedFromRaw()
    {
        // The reply a 3.1.0 agent sends, exactly as the shared corpus has it: the bytes adopted are
        // `raw`, and they hash to `hash`.
        var fixture = Value.Parse(Repo.ReadFixture("config.read.json"));
        var reply = ConfigReply.From(fixture);
        Assert.Equal(fixture["raw"].Raw.GetString(), reply.Document);
        Assert.Equal(fixture["hash"].AsText(), ControllerDocument.FromText(reply.Document!).Hash);

        // An agent before 3.1.0 sends no `raw`. Its stored document is rebuilt from `controller`
        // in the order received, whatever spacing the reply itself used, and hashes to the same.
        var older = JsonNode.Parse(Repo.ReadFixture("config.read-without-raw.json"))!;
        foreach (var spelling in new[] { older.ToJsonString(), older.ToJsonString(new JsonSerializerOptions { WriteIndented = true }) })
        {
            var rebuilt = ConfigReply.From(Value.Parse(spelling));
            Assert.Null(rebuilt.Raw);
            Assert.Equal(older["hash"]!.GetValue<string>(), ControllerDocument.FromText(rebuilt.Document!).Hash);
        }
    }

    [Fact]
    public async Task TwoOfflineEditsStopAtAPersonAndNothingIsOverwritten()
    {
        using var home = new TempHome();
        var shared = Documents.Make("setup-a", 5, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from here");
        var theirs = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from there");
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var outcome = await reconciler.ReconcileAsync(model);
        var decision = Assert.IsType<ReconcileOutcome.NeedsDecision>(outcome);
        Assert.IsType<SetupSharing.Diverged>(decision.Sharing);
        // Neither side moved.
        Assert.Equal(theirs.Hash, fake.Held!.Hash);
        Assert.Equal(mine.Hash, store.Document!.Hash);
    }

    [Fact]
    public async Task UnchangedDivergenceReusesMetadataAcrossPolls()
    {
        using var home = new TempHome();
        var ancestor = Documents.Make("setup-a", 3, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 4, new[] { ancestor.Hash }, "Mine");
        var (store, model, fake) = Build(home, mine);
        fake.Held = Documents.Make("setup-a", 4, new[] { ancestor.Hash }, "Theirs");
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        await reconciler.ReconcileAsync(model);
        await model.RefreshAsync();
        await reconciler.ReconcileAsync(model);
        Assert.Equal(1, fake.MetaReads);
    }

    [Fact]
    public async Task AMergeIsAcceptedByTheMachineWithoutBeingForced()
    {
        using var home = new TempHome();
        var shared = Documents.Make("setup-a", 5, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from here", extra: "mine");
        var theirs = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from there");
        var (store, model, fake) = Build(home, mine);
        store.Ledger.Remember(shared);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        await reconciler.ReconcileAsync(model);

        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.Equal(shared.Hash, session!.Base!.Hash);
        Assert.Null(await reconciler.MergeAsync(model, session, null));
        var merged = store.Document!;
        Assert.Contains(mine.Hash, merged.Identity.Lineage);
        Assert.Contains(theirs.Hash, merged.Identity.Lineage);
        // Only this device changed `extra`, so it is kept; both changed the name, and theirs is the
        // default.
        Assert.Contains("\"extra\": \"mine\"", merged.Text);
        Assert.Equal("Beacon from there", store.Config!.Machines[0].Name);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Published>(await reconciler.ReconcileAsync(model));
        Assert.Equal(merged.Hash, fake.Held!.Hash);
        Assert.False(fake.SawReplace);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task AKeptBaseThisDeviceRefusesIsSkippedLikeOneThatIsNotKept(bool keepsAnOlderOne)
    {
        // The newest revision both sides descend from, kept here, is one peer intake refuses: an
        // older client could publish it through an agent before 3.1.0. Section 9.8 does not say yet
        // whether such a base stops the merge, as the reference's does, or is skipped like one
        // that is not kept. Here it is skipped: an older shared revision this device accepts is
        // the base, and with none every difference is asked about. Either way the merge writes.
        using var home = new TempHome();
        var older = Documents.Make("setup-a", 4, Array.Empty<string>());
        var refused = JsonNode.Parse(Documents.Make("setup-a", 5, new[] { older.Hash }).Text)!.AsObject();
        refused["sites"] = new JsonArray(new JsonObject { ["id"] = "home", ["lanPrefixes"] = new JsonArray("10.0.0") });
        refused["machines"]![0]!["site"] = "elsewhere";
        var shared = ControllerDocument.FromText(refused.ToJsonString());
        Assert.False(DocumentAcceptance.Check(shared.Bytes, Intake.Peer).Valid);
        var mine = Documents.Make("setup-a", 6, new[] { shared.Hash, older.Hash }, "Beacon from here", extra: "mine");
        var theirs = Documents.Make("setup-a", 6, new[] { shared.Hash, older.Hash }, "Beacon from there");
        var (store, model, fake) = Build(home, mine);
        store.Ledger.Remember(shared);
        if (keepsAnOlderOne) store.Ledger.Remember(older);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        await reconciler.ReconcileAsync(model);

        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.Equal(keepsAnOlderOne ? older.Hash : null, session!.Base?.Hash);
        Assert.True(session.CanMerge);
        Assert.Equal(new[] { "extra", "machines[pi].fields" }, session.Differences.Select(difference => difference.Entry));
        Assert.Equal(keepsAnOlderOne ? new[] { SetupChange.Mine, SetupChange.Both } : new[] { SetupChange.NoBase, SetupChange.NoBase },
            session.Differences.Select(difference => difference.Kind));

        Assert.Null(await reconciler.MergeAsync(model, session, null));
        Assert.Contains(mine.Hash, store.Document!.Identity.Lineage);
        Assert.Contains(theirs.Hash, store.Document.Identity.Lineage);
        // Only this device added `extra` since the older base, so it is kept; with no base it is a
        // question, and theirs is the default answer. The name is a question either way.
        Assert.Equal(keepsAnOlderOne, store.Document.Text.Contains("\"extra\": \"mine\""));
        Assert.Equal("Beacon from there", store.Config!.Machines[0].Name);
    }

    [Fact]
    public async Task KeepingMineProducesARevisionTheMachineAcceptsRatherThanAReplacement()
    {
        using var home = new TempHome();
        var shared = Documents.Make("setup-a", 5, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from here");
        var theirs = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from there");
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        await reconciler.ReconcileAsync(model);

        var (session, _) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(await reconciler.KeepMineAsync(model, session!));
        Assert.Contains(theirs.Hash, store.Document!.Identity.Lineage);
        Assert.Equal("Beacon from here", store.Config!.Machines[0].Name);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Published>(await reconciler.ReconcileAsync(model));
        Assert.False(fake.SawReplace);
    }

    [Fact]
    public async Task ADecisionActsOnlyOnWhatTheScreenShowedAndOnlyOverTheSetupItWasBuiltFrom()
    {
        using var home = new TempHome();
        var shared = Documents.Make("setup-a", 5, Array.Empty<string>());
        var mine = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from here");
        var theirs = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from there");
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        var (session, _) = await reconciler.PrepareMergeAsync(model);

        // The machine moves on while the screen is open. Take theirs adopts what was shown, not
        // what the machine holds now, and sends nothing.
        fake.Held = Documents.Make("setup-a", 7, new[] { theirs.Hash }, "Beacon moved on");
        var reads = fake.Reads;
        Assert.Null(await reconciler.TakeTheirsAsync(model, session!));
        Assert.Equal(theirs.Hash, store.Document!.Hash);
        Assert.Equal(reads, fake.Reads);

        // A screen built over a setup that has since changed acts on nothing.
        var (stale, _) = await reconciler.PrepareMergeAsync(model);
        store.ApplyEdit(store.Document.Text.Replace("Beacon from there", "Edited meanwhile"));
        var edited = store.Document!.Hash;
        Assert.Equal(ConfigStore.HeadMovedSentence, await reconciler.TakeTheirsAsync(model, stale!));
        Assert.Equal(ConfigStore.HeadMovedSentence, await reconciler.MergeAsync(model, stale!, null));
        Assert.Equal(edited, store.Document!.Hash);
    }

    [Fact]
    public async Task ADifferentSetupIsAQuestionAndOnlyThenAReplacement()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 2, Array.Empty<string>());
        var theirs = Documents.Make("setup-b", 9, Array.Empty<string>(), "somebody else's");
        var (store, model, fake) = Build(home, mine);
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var decision = Assert.IsType<ReconcileOutcome.NeedsDecision>(await reconciler.ReconcileAsync(model));
        Assert.IsType<SetupSharing.DifferentSetup>(decision.Sharing);
        Assert.False(fake.SawReplace);

        // The preview is what a person reads before deciding, and a different setup is never merged.
        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.True(session!.DifferentSetup);
        Assert.False(session.CanMerge);
        Assert.Contains("Machines changed: pi", session.Preview.Lines());

        Assert.Null(await reconciler.ReplaceTheirsAsync(model, session));
        Assert.True(fake.SawReplace);
        Assert.Equal(mine.Hash, fake.Held!.Hash);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task ADifferentSetupThisDeviceCannotAdoptCanStillBeReplaced(bool unverifiable)
    {
        // An agent before 3.1.0 holding a file no client rebuilds byte for byte, or a document this
        // device refuses. Replacing it never uses its bytes, and refusing the whole screen left no
        // way to replace it from here at all.
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 2, Array.Empty<string>());
        ControllerDocument theirs;
        var (store, model, fake) = Build(home, mine);
        if (unverifiable)
        {
            theirs = Unrebuildable(Documents.Make("setup-b", 9, Array.Empty<string>(), "somebody else's"));
            fake.SendsRaw = false;
            fake.AgentVersion = "3.0.1";
        }
        else
        {
            var refused = JsonNode.Parse(Documents.Make("setup-b", 9, Array.Empty<string>(), "somebody else's").Text)!.AsObject();
            refused["sites"] = new JsonArray(new JsonObject { ["id"] = "home", ["lanPrefixes"] = new JsonArray("10.0.0") });
            refused["machines"]![0]!["site"] = "elsewhere";
            theirs = ControllerDocument.FromText(refused.ToJsonString());
            Assert.Contains(DocumentAcceptance.Check(theirs.Bytes, Intake.Peer).Errors, error => error.Code == "unknown-site");
        }
        fake.Held = theirs;
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        Assert.IsType<SetupSharing.DifferentSetup>(Assert.IsType<ReconcileOutcome.NeedsDecision>(await reconciler.ReconcileAsync(model)).Sharing);

        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.True(session!.DifferentSetup);
        Assert.Equal(theirs.Hash, session.TheirsHash);
        Assert.Contains(unverifiable ? "has to be updated" : "does not accept", session.TakeProblem);
        Assert.False(session.CanTakeTheirs);
        Assert.False(session.CanMerge);
        Assert.True(session.CanReplace);
        Assert.Contains("Machines changed: pi", session.Preview.Lines());

        Assert.Equal(session.TakeProblem, await reconciler.TakeTheirsAsync(model, session));
        Assert.Equal(mine.Hash, store.Document!.Hash);
        Assert.Null(await reconciler.ReplaceTheirsAsync(model, session));
        Assert.True(fake.SawReplace);
        Assert.Equal(mine.Hash, fake.Held!.Hash);
    }

    [Fact]
    public async Task APreviewOfADocumentThisDeviceRefusesHidesNothingInItsIds()
    {
        // A different setup this device refuses still gets a screen, and its machine ids are
        // whatever the peer wrote. A bidirectional override in one would turn the rest of the line
        // around in the sheet a person reads before overwriting the machine.
        using var home = new TempHome();
        var (store, model, fake) = Build(home, Documents.Make("setup-a", 2, Array.Empty<string>()));
        var refused = JsonNode.Parse(Documents.Make("setup-b", 9, Array.Empty<string>()).Text)!.AsObject();
        refused["machines"]![0]!["id"] = "pi‮exe.txt";
        fake.Held = ControllerDocument.FromText(refused.ToJsonString());
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();

        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.Contains("does not accept", session!.TakeProblem);
        var lines = session.Preview.Lines().ToList();
        Assert.Contains("Machines added: \"pi\\u202Eexe.txt\"", lines);
        Assert.Contains("Machines removed: pi", lines);
        Assert.DoesNotContain(lines, line => line.Contains('‮'));
    }

    [Fact]
    public async Task AReplacementGoesOnlyOverTheCopyThePreviewShowed()
    {
        // `--replace` takes whatever the machine holds by the time it arrives, so a machine whose
        // latest reading names another document is not replaced from a screen built before.
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 2, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Held = Documents.Make("setup-b", 9, Array.Empty<string>(), "somebody else's");
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        var (session, _) = await reconciler.PrepareMergeAsync(model);

        fake.Held = Documents.Make("setup-b", 10, new[] { fake.Held.Hash }, "edited there meanwhile");
        await model.RefreshAsync();
        Assert.Contains("another document", await reconciler.ReplaceTheirsAsync(model, session!));
        Assert.False(fake.SawReplace);
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task ACopyOfThisSetupIsNeverReplaced()
    {
        // The agent takes `--replace` over a divergent copy of the same setup too. Only a different
        // setup is ever replaced; a copy of this one is merged, kept or taken.
        using var home = new TempHome();
        var shared = Documents.Make("setup-a", 5, Array.Empty<string>());
        var (store, model, fake) = Build(home, Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from here"));
        fake.Held = Documents.Make("setup-a", 6, new[] { shared.Hash }, "Beacon from there");
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        var (session, _) = await reconciler.PrepareMergeAsync(model);

        Assert.False(session!.CanReplace);
        Assert.Contains("holds a copy of this setup", await reconciler.ReplaceTheirsAsync(model, session));
        Assert.False(fake.SawReplace);
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task ASetupWithAnErrorReplacesNothing()
    {
        // Section 9.5: a device whose setup has an error pushes nothing until it is fixed, and a
        // replacement least of all. This machine stores whatever it is sent, the way an agent
        // before 3.1.0 does.
        using var home = new TempHome();
        var broken = ControllerDocument.FromText(Documents.Make("setup-a", 3, Array.Empty<string>()).Text
            .Replace("\"source\": \"desktop\"", "\"source\": \"android\""));
        var (store, model, fake) = Build(home, broken);
        Assert.False(store.Acceptance!.Valid);
        fake.Held = Documents.Make("setup-b", 9, Array.Empty<string>(), "somebody else's");
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();

        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.True(session!.DifferentSetup);
        Assert.True(session.CanTakeTheirs);
        Assert.False(session.CanReplace);
        Assert.Contains("has errors", session.ReplaceProblem);
        Assert.Equal(session.ReplaceProblem, await reconciler.ReplaceTheirsAsync(model, session));
        // A screen that was not told is refused the same: the setup here is asked again before
        // anything is sent.
        Assert.Equal(session.ReplaceProblem, await reconciler.ReplaceTheirsAsync(model, session with { ReplaceProblem = null }));
        Assert.False(fake.SawReplace);
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task ADeviceWithNoSetupIsToldItHasNothingToSend()
    {
        // A device with no setup gets a screen for whatever a machine holds, and can only take it.
        // Asked to replace it, it says what is missing, not that the machine holds a copy of a
        // setup it does not have.
        using var home = new TempHome();
        var (_, model, fake) = Build(home, Documents.Make("setup-a", 2, Array.Empty<string>()));
        fake.Held = Documents.Make("setup-b", 9, Array.Empty<string>(), "somebody else's");
        File.Delete(home.ConfigPath);
        var reconciler = new Reconciler(Store(home), Bindings.Empty);
        await model.RefreshAsync();

        var (session, problem) = await reconciler.PrepareMergeAsync(model);
        Assert.Null(problem);
        Assert.Null(session!.Mine);
        Assert.True(session.CanTakeTheirs);
        Assert.False(session.CanReplace);
        Assert.Equal("this device has no setup to send", await reconciler.ReplaceTheirsAsync(model, session));
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task NothingIsPublishedWhileAnOperationIsRunningThere()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 2, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Running = true;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var held = Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model));
        Assert.Contains("running", held.Reason);
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task NothingIsPublishedWhileThisDeviceHasAnActionInFlightThere()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 2, Array.Empty<string>());
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var (store, model, fake) = Build(home, mine, tracker);
        tracker.Begin(CommandSurface.NewOperationId(), model.Id, null, new OperationIntent("update", Service: "t3"));
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model));
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task AnAgentThatCannotCarryASetupIsLeftOutRatherThanFoughtWith()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 2, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Contract = 0;
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Unsupported>(await reconciler.ReconcileAsync(model));
        Assert.Empty(fake.Pushes);
    }

    [Fact]
    public async Task AFailedPushBacksOffForAMinuteThenRetriesIdenticalBytes()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Refusals.Enqueue("error.internal");
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Failed>(await reconciler.ReconcileAsync(model));
        Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model, now: DateTimeOffset.UtcNow.AddSeconds(59)));
        Assert.IsType<ReconcileOutcome.Published>(await reconciler.ReconcileAsync(model, now: DateTimeOffset.UtcNow.AddSeconds(61)));
        Assert.Equal(new[] { mine.Hash, mine.Hash }, fake.Pushes);
    }

    [Fact]
    public async Task ALockedPushWaitsForALaterStatusRatherThanAMinute()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Refusals.Enqueue("config.set.locked");
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model));
        // The status that was read before the refusal says nothing about after it.
        Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model));
        Assert.Single(fake.Pushes);

        await Task.Delay(5);
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Published>(await reconciler.ReconcileAsync(model));
        Assert.Equal(2, fake.Pushes.Count);
    }

    [Fact]
    public async Task ARestrictedRefusalIsHeldUntilTheSetupChangesOrAPersonAsks()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        for (var index = 0; index < 3; index += 1) fake.Refusals.Enqueue("config.set.restricted-launch");
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var withheld = Assert.IsType<ReconcileOutcome.Withheld>(await reconciler.ReconcileAsync(model));
        Assert.IsType<SetupSharing.AwaitingFullAccess>(withheld.Sharing);
        Assert.Contains("not restricted", withheld.Sentence_);

        // Not a failure to back off from and come back to: not sent again at all.
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Withheld>(await reconciler.ReconcileAsync(model, now: DateTimeOffset.UtcNow.AddMinutes(5)));
        Assert.Single(fake.Pushes);

        // A person asks.
        Assert.IsType<ReconcileOutcome.Withheld>(await reconciler.TryAgainAsync(model));
        Assert.Equal(2, fake.Pushes.Count);

        // The setup here changes.
        store.ApplyEdit(store.Document!.Text.Replace("Beacon", "Beacon renamed"));
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Withheld>(await reconciler.ReconcileAsync(model));
        Assert.Equal(3, fake.Pushes.Count);
        Assert.Equal(store.Document!.Hash, fake.Pushes[^1]);
    }

    [Fact]
    public async Task AnInvalidRefusalIsHeldAndSaysWhatTheMachineFound()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Refusals.Enqueue("config.set.invalid-document");
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        var withheld = Assert.IsType<ReconcileOutcome.Withheld>(await reconciler.ReconcileAsync(model));
        Assert.IsType<SetupSharing.Refused>(withheld.Sharing);
        Assert.Contains("machines[0].endpoints[0].host", withheld.Sentence_);
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Withheld>(await reconciler.ReconcileAsync(model, now: DateTimeOffset.UtcNow.AddMinutes(5)));
        Assert.Single(fake.Pushes);
    }

    [Fact]
    public async Task ASetupWithAnErrorIsNotPublishedButStillFetches()
    {
        // The device applies the document with its error shown (applied intake) and publishes
        // nothing until it is fixed, but it still takes what a peer fixed.
        using var home = new TempHome();
        var broken = ControllerDocument.FromText(Documents.Make("setup-a", 3, Array.Empty<string>()).Text
            .Replace("\"source\": \"desktop\"", "\"source\": \"android\""));
        var (store, model, fake) = Build(home, broken);
        Assert.False(store.Acceptance!.Valid);
        var reconciler = new Reconciler(store, Bindings.Empty);

        await model.RefreshAsync();
        Assert.Contains("errors", Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model)).Reason);
        Assert.Empty(fake.Pushes);

        fake.Held = Documents.Make("setup-a", 4, new[] { broken.Hash }, "Beacon fixed there");
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Adopted>(await reconciler.ReconcileAsync(model));
        Assert.True(store.Acceptance!.Valid);
    }

    [Fact]
    public async Task AHandEditSavedDuringAFetchStaysAndTheMachineIsThenDivergent()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        fake.Held = Documents.Make("setup-a", 4, new[] { mine.Hash }, "Peer edit");
        fake.OnRead = () => File.WriteAllText(home.ConfigPath, mine.Text.Replace("Beacon", "Local offline edit"));
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();

        var decision = Assert.IsType<ReconcileOutcome.NeedsDecision>(await reconciler.ReconcileAsync(model));
        Assert.IsType<SetupSharing.Diverged>(decision.Sharing);
        Assert.Equal("Local offline edit", store.Config!.Machines[0].Name);
        Assert.Contains(mine.Hash, store.Identity.Lineage);
    }

    [Fact]
    public async Task TwoReadsCannotOverwriteANewHead()
    {
        // The window and the command line on one device, each fetching from a machine that holds
        // its own descendant of the setup here. The command line adopts while the window is still
        // reading; the window's bytes were chosen against the setup before, so they are not
        // written, and the window decides again from the new head.
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (window, model, fake) = Build(home, mine);
        var commandLine = Store(home);
        var adoptedThere = Documents.Make("setup-a", 4, new[] { mine.Hash }, "From the other machine");
        fake.Held = Documents.Make("setup-a", 4, new[] { mine.Hash }, "From this machine");
        fake.OnRead = () =>
        {
            fake.OnRead = null;
            Assert.IsType<ApplyResult.Applied>(commandLine.ApplyIfCurrent(adoptedThere, mine.Hash, "adopted from nas", adoptedThere.Hash));
        };
        var reconciler = new Reconciler(window, Bindings.Empty);
        await model.RefreshAsync();

        var decision = Assert.IsType<ReconcileOutcome.NeedsDecision>(await reconciler.ReconcileAsync(model));
        Assert.IsType<SetupSharing.Diverged>(decision.Sharing);
        Assert.Equal(adoptedThere.Hash, window.Document!.Hash);
        Assert.Equal(adoptedThere.Bytes, File.ReadAllBytes(home.ConfigPath));
        // An adoption by another process here is no hand edit: it gets no revision of its own.
        Assert.Equal(4, window.Document.Identity.RevisionNumber);
    }

    [Fact]
    public async Task AMachineThatMovesBetweenMetaAndFetchIsDecidedAgain()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, fake) = Build(home, mine);
        var first = Documents.Make("setup-a", 4, new[] { mine.Hash }, "First");
        var second = Documents.Make("setup-a", 5, new[] { first.Hash, mine.Hash }, "Second");
        fake.Held = first;
        fake.OnMeta = () => fake.Held = second;
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();

        Assert.IsType<ReconcileOutcome.Held>(await reconciler.ReconcileAsync(model));
        Assert.Equal(mine.Hash, store.Document!.Hash);

        fake.OnMeta = null;
        await model.RefreshAsync();
        Assert.IsType<ReconcileOutcome.Adopted>(await reconciler.ReconcileAsync(model));
        Assert.Equal(second.Hash, store.Document!.Hash);
    }

    [Fact]
    public async Task TheOutcomesTheWindowReadsAreASnapshotThatNeverChangesUnderIt()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (store, model, _) = Build(home, mine);
        var reconciler = new Reconciler(store, Bindings.Empty);
        await model.RefreshAsync();
        var before = reconciler.Outcomes;
        await reconciler.ReconcileAsync(model);
        Assert.Empty(before);
        Assert.IsType<ReconcileOutcome.Published>(reconciler.Outcomes[model.Id]);
    }

    [Fact]
    public async Task AnIdenticalRePushAfterALostReplyIsANoopAndCountsAsSuccess()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 3, Array.Empty<string>());
        var (_, model, fake) = Build(home, mine);
        fake.Held = mine;
        await model.RefreshAsync();
        var (reply, problem) = await model.PushSetupAsync(mine);
        Assert.Null(problem);
        Assert.Equal("noop", reply!.Action);
        Assert.Equal(PushEffect.Clear, SetupSync.AfterPush(reply.Json, mine.Hash).Effect);
    }

    [Fact]
    public async Task ADocumentWhoseHashDoesNotMatchWhatTheMachineClaimsIsNotAdopted()
    {
        using var home = new TempHome();
        var mine = Documents.Make("setup-a", 1, Array.Empty<string>());
        var (store, model, _) = Build(home, mine);

        var lying = new FakeRunner(call => call.RemoteCommand.Contains(" config")
            ? FakeRunner.Ok("""
                { "ok": true, "contract": 3, "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                  "controller": { "version": 1, "machines": [] } }
                """)
            : FakeRunner.Ok("""{ "ok": true, "contract": 3, "agentVersion": "3.0.0" }"""));
        var model2 = new MachineModel(store.Config!.Machines[0], lying,
            new OperationTracker(Path.Combine(home.Path, "operations2.json")));
        await model2.RefreshAsync();
        var (document, problem) = await model2.FetchSetupAsync();
        Assert.Null(document);
        Assert.Contains("hashes to", problem);
    }
}

/// Readings of one machine that overlap.
public class ReadingTests
{
    /// A runner whose first status waits until it is let go, and whose later ones answer at once.
    private sealed class SlowFirstRunner(string first, string later) : IProcessRunner
    {
        public TaskCompletionSource Release { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        private int _calls;

        public async Task<CommandResult> RunAsync(string executable, IReadOnlyList<string> arguments, TimeSpan timeout,
            byte[]? input = null, CancellationToken cancellationToken = default)
        {
            if (Interlocked.Increment(ref _calls) == 1)
            {
                await Release.Task;
                return FakeRunner.Ok(first);
            }
            return FakeRunner.Ok(later);
        }
    }

    [Fact]
    public async Task ASlowReadingNeverPutsAnOlderPictureOverANewerOne()
    {
        using var home = new TempHome();
        var machine = MachineConfig.From(Value.Parse("""
            { "id": "pi", "endpoints": [ { "id": "lan", "host": "pi.example" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ] }
            """));
        var runner = new SlowFirstRunner(
            """{ "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" }, "hostname": "older" }""",
            """{ "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux" }, "hostname": "newer" }""");
        var model = new MachineModel(machine, runner, new OperationTracker(Path.Combine(home.Path, "operations.json")));

        var slow = model.RefreshAsync();
        await model.RefreshAsync();
        Assert.Equal("3.1.0", model.Status!.AgentVersion);

        runner.Release.SetResult();
        await slow;
        Assert.Equal("3.1.0", model.Status!.AgentVersion);
    }
}

/// Where this device thinks it is, and what that is worth.
public class SiteSenseTests
{
    private static ControllerConfig TwoHousesWithTheSameSubnet => ControllerConfig.From(Value.Parse("""
        {
          "version": 1,
          "sites": [
            { "id": "attic", "lanPrefixes": [ "192.168.50." ] },
            { "id": "flat",  "lanPrefixes": [ "192.168.50." ] }
          ],
          "machines": [ { "id": "legion", "site": "attic", "wake": { "mac": "AA:BB:CC:DD:EE:FF" } } ]
        }
        """));

    [Fact]
    public void TwoSitesWithTheSameSubnetAreAmbiguousRatherThanTheFirstOne()
    {
        // The router default that every second home uses. A prefix match cannot tell them apart,
        // and picking one would be a guess dressed up as a fact.
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet, Bindings.Empty, new[] { "192.168.50.34" });
        Assert.Null(presence.SiteId);
        Assert.True(presence.IsAmbiguous);
        Assert.Contains("cannot be told", presence.Sentence);
        Assert.False(presence.CanReach("attic"));
    }

    [Fact]
    public void APersonSayingWhereTheyAreSettlesIt()
    {
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet,
            new Bindings { CurrentSite = "flat" }, new[] { "192.168.50.34" });
        Assert.Equal("flat", presence.SiteId);
        Assert.True(presence.Confirmed);
        Assert.True(presence.CanReach("flat"));
        Assert.False(presence.CanReach("attic"));
    }

    [Fact]
    public void OneMatchingSiteIsAHintAndSaysSo()
    {
        var config = ControllerConfig.From(Value.Parse("""
            { "version": 1, "sites": [ { "id": "attic", "lanPrefixes": [ "10.0.0." ] } ], "machines": [] }
            """));
        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "10.0.0.7" });
        Assert.Equal("attic", presence.SiteId);
        Assert.False(presence.Confirmed);
        Assert.Contains("Not confirmed", presence.Sentence);
        Assert.True(presence.CanReach("attic"));
    }

    [Fact]
    public void AMachineWithNoSiteFallsBackToItsOwnPrefix()
    {
        var machine = MachineConfig.From(Value.Parse("""
            { "id": "legion", "wake": { "mac": "AA:BB:CC:DD:EE:FF", "lanPrefix": "10.1.1." } }
            """));
        var presence = new SitePresence();
        Assert.True(presence.IsOnNetworkOf(machine, new[] { "10.1.1.9" }));
        Assert.False(presence.IsOnNetworkOf(machine, new[] { "10.2.2.9" }));
    }

    [Fact]
    public void NoAddressesMeansNotOnAnySiteRatherThanOnAllOfThem()
    {
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet, Bindings.Empty, Array.Empty<string>());
        Assert.Null(presence.SiteId);
        Assert.False(presence.IsAmbiguous);
    }
}

/// How a machine gets woken, and what is said when it cannot be.
public class WakeTests
{
    private static ControllerConfig Config => ControllerConfig.From(Value.Parse("""
        {
          "version": 1,
          "sites": [ { "id": "attic", "lanPrefixes": [ "10.0.0." ], "broadcast": [ "10.0.0.255" ] } ],
          "machines": [
            { "id": "pi", "name": "Pi", "site": "attic", "alwaysOn": true,
              "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.5" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ] },
            { "id": "nas", "name": "NAS", "site": "attic", "alwaysOn": true,
              "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.6" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ] },
            { "id": "legion", "name": "Legion", "site": "attic",
              "endpoints": [ { "id": "lan", "kind": "lan", "host": "10.0.0.7" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ],
              "wake": { "mac": "AA:BB:CC:DD:EE:FF", "broadcast": [ "10.0.0.255" ],
                        "helper": { "machine": "pi", "action": "wake-legion" },
                        "helpers": [ { "machine": "pi", "action": "wake-legion" },
                                     { "machine": "nas", "action": "wake-legion" } ] } }
          ]
        }
        """));

    private static Dictionary<string, MachineModel> Models(TempHome home, IProcessRunner runner, ControllerConfig config)
    {
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        return config.Machines.ToDictionary(
            machine => machine.Id,
            machine => new MachineModel(machine, runner, tracker),
            StringComparer.Ordinal);
    }

    [Fact]
    public void OnTheTargetsOwnNetworkThePacketGoesDirectlyAndNoHelperIsAsked()
    {
        using var home = new TempHome();
        var config = Config;
        var models = Models(home, new FakeRunner(_ => FakeRunner.Ok("{\"ok\":true}")), config);
        var presence = SitePresence.Decide(config, new Bindings { CurrentSite = "attic" }, new[] { "10.0.0.34" });
        var plan = WakePlanner.Plan(config.Machine("legion")!, config, presence, models, new[] { "10.0.0.34" });
        Assert.IsType<WakeStep.Direct>(plan.Steps[0]);
    }

    [Fact]
    public async Task OffSiteTheHelpersAreWalkedInOrder()
    {
        using var home = new TempHome();
        var config = Config;
        var models = Models(home, new FakeRunner(_ => FakeRunner.Ok("""
            { "ok": true, "contract": 3, "actions": [ { "id": "wake-legion", "kind": "wol" } ] }
            """)), config);
        foreach (var model in models.Values) await model.RefreshAsync();

        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "203.0.113.9" });
        var plan = WakePlanner.Plan(config.Machine("legion")!, config, presence, models, new[] { "203.0.113.9" });
        Assert.All(plan.Steps, step => Assert.IsType<WakeStep.ViaHelper>(step));
        Assert.Equal("pi", ((WakeStep.ViaHelper)plan.Steps[0]).Helper.Machine);
        Assert.Equal("nas", ((WakeStep.ViaHelper)plan.Steps[1]).Helper.Machine);
    }

    [Fact]
    public async Task AHelperThatIsAsleepIsSkippedAndOfferedAsAManualStep()
    {
        using var home = new TempHome();
        var config = ControllerConfig.From(Value.Parse("""
            {
              "version": 1,
              "machines": [
                { "id": "pi", "name": "Pi",
                  "endpoints": [ { "id": "lan", "host": "10.0.0.5" } ],
                  "wake": { "mac": "11:22:33:44:55:66" },
                  "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ] },
                { "id": "legion", "name": "Legion",
                  "endpoints": [ { "id": "lan", "host": "10.0.0.7" } ],
                  "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ],
                  "wake": { "mac": "AA:BB:CC:DD:EE:FF",
                            "helpers": [ { "machine": "pi", "action": "wake-legion" } ] } }
              ]
            }
            """));
        var models = Models(home, new FakeRunner(_ => FakeRunner.SshFailure("ssh: connect to host 10.0.0.5 port 22: No route to host")), config);
        await models["pi"].RefreshAsync();

        var plan = WakePlanner.Plan(config.Machine("legion")!, config, new SitePresence(), models,
            Array.Empty<string>());
        Assert.Empty(plan.Steps);
        Assert.Contains(plan.Obstacles, obstacle => obstacle.Contains("asleep or off"));
        // The manual step is offered rather than taken: waking a helper to wake something else is
        // how a machine nobody wanted on ends up on.
        Assert.Contains("pi", plan.OfferToWakeFirst);
        Assert.Contains("no way to wake", plan.Sentence("Legion"), StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task AHelperWithoutTheNamedActionIsNotTried()
    {
        using var home = new TempHome();
        var config = Config;
        var models = Models(home, new FakeRunner(_ => FakeRunner.Ok("""
            { "ok": true, "contract": 3, "actions": [ { "id": "something-else" } ] }
            """)), config);
        foreach (var model in models.Values) await model.RefreshAsync();
        var plan = WakePlanner.Plan(config.Machine("legion")!, config, new SitePresence(), models,
            Array.Empty<string>());
        Assert.Empty(plan.Steps);
        Assert.Contains(plan.Obstacles, obstacle => obstacle.Contains("no action called"));
    }

    [Fact]
    public async Task AWakeIsOnlyConfirmedByTheMachineAnswering()
    {
        using var home = new TempHome();
        var config = Config;
        var awake = false;
        var runner = new FakeRunner(call =>
        {
            if (call.Arguments.Any(argument => argument.Contains("10.0.0.7")) && !awake)
            {
                return FakeRunner.SshFailure("ssh: connect to host 10.0.0.7 port 22: No route to host");
            }
            return FakeRunner.Ok("""
                { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" },
                  "actions": [ { "id": "wake-legion", "kind": "wol" } ] }
                """);
        });
        var models = Models(home, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();

        var target = models["legion"];
        var plan = WakePlanner.Plan(config.Machine("legion")!, config, new SitePresence(), models,
            Array.Empty<string>());
        Assert.NotEmpty(plan.Steps);

        awake = true;
        // The target's failed address is only eligible again after the required minute.
        foreach (var route in target.Machine.Routes)
            target.Routes.NoteFailure(route.Id, DateTimeOffset.UtcNow - TimeSpan.FromSeconds(61));
        var report = await new WakeRunner(runner).RunAsync(target, plan, models, TimeSpan.FromSeconds(5));
        Assert.True(report.Confirmed);
        Assert.Contains("answering", report.Sentence);
    }

    [Fact]
    public async Task AHelperActionWhoseOutcomeIsUnknownStopsTheFailover()
    {
        // A general command action could be anything. Asking a second helper to do it while the
        // first may already have done it is exactly the mistake operation ids exist to avoid.
        using var home = new TempHome();
        var config = Config;
        var runner = new FakeRunner(call =>
        {
            if (call.RemoteCommand.Contains("run wake-legion")) return FakeRunner.TimedOutResult();
            if (call.Arguments.Any(argument => argument.Contains("10.0.0.7")))
            {
                return FakeRunner.SshFailure("ssh: connect to host 10.0.0.7 port 22: No route to host");
            }
            return FakeRunner.Ok("""
                { "ok": true, "contract": 3, "agentVersion": "3.0.0",
                  "actions": [ { "id": "wake-legion", "kind": "command" } ] }
                """);
        });
        var models = Models(home, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();

        var plan = WakePlanner.Plan(config.Machine("legion")!, config, new SitePresence(), models,
            Array.Empty<string>());
        var report = await new WakeRunner(runner).RunAsync(models["legion"], plan, models, TimeSpan.FromSeconds(1));
        Assert.False(report.Confirmed);
        Assert.Contains("not a declared wake action", report.Sentence);
        // The second helper was never asked.
        Assert.DoesNotContain(report.Attempts, attempt => attempt.StartsWith("NAS", StringComparison.Ordinal));
    }

    [Fact]
    public void AMagicPacketIsTheStandardHundredAndTwoBytes()
    {
        var packet = WakeOnLan.MagicPacket(new byte[] { 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF });
        Assert.Equal(102, packet.Length);
        Assert.All(packet.Take(6), value => Assert.Equal(0xFF, value));
        Assert.Equal(0xAA, packet[6]);
        Assert.Equal(0xFF, packet[^1]);
    }

    [Fact]
    public void AMachineWithNothingToSendToSaysSoRatherThanShowingADeadButton()
    {
        using var home = new TempHome();
        var config = ControllerConfig.From(Value.Parse("""
            { "version": 1, "machines": [ { "id": "x", "name": "X" } ] }
            """));
        var plan = WakePlanner.Plan(config.Machine("x")!, config, new SitePresence(),
            new Dictionary<string, MachineModel>(), Array.Empty<string>());
        Assert.True(plan.IsEmpty);
        Assert.Contains("no wake configuration", plan.Sentence("X"));
    }
}
