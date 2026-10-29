using System.Diagnostics;
using System.Globalization;
using System.Numerics;
using System.Text;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// How setup documents are read and written, so that every client writes the same bytes for the
/// same document and keeps what it does not understand exactly as it found it.
public class DocumentJsonTests
{
    [Fact]
    public void EscapesExactlyWhatJavaScriptEscapes()
    {
        // The quote, the backslash, controls below U+0020 and a surrogate without its partner; not
        // an apostrophe, a plus, a line separator, anything outside ASCII or an emoji.
        Assert.Equal("\"a\\\"b\\\\c\\n\\t\\u0001\"", DocumentJson.Quote("a\"b\\c\n\t\u0001"));
        Assert.Equal("\"Büro's +1 \u2028 🎉 <x> &\"", DocumentJson.Quote("Büro's +1 \u2028 🎉 <x> &"));
        Assert.Equal("\"\\ud800x\"", DocumentJson.Quote("\ud800x"));
    }

    [Fact]
    public void ReadsJsonAndNothingLooser()
    {
        foreach (var text in new[] { "{\"a\": 1,}", "{\"a\": 1 /* note */}", "\uFEFF{}", "{'a': 1}", "{\"a\": \"x\ny\"}", "{\"a\": NaN}", "{\"a\": 01}" })
            Assert.Throws<DocumentJson.Malformed>(() => DocumentJson.Parse(text));
        var value = (DocumentObject)DocumentJson.Parse("{\"a\": \"\\u00e9\\n\\/\", \"b\": [true, false, null]}");
        Assert.Equal("é\n/", ((DocumentString)value["a"]!).Value);
    }

    [Theory]
    [InlineData("{\"a\": 1, \"a\": {\"x\": 1, \"x\": 2}}", "a")]
    [InlineData("{\"a\": {\"x\": 1, \"x\": 2}, \"a\": 1}", "a.x")]
    [InlineData("{\"m\": [{\"id\": 1}, {\"id\": 1, \"\\u0069d\": 2}]}", "m[1].id")]
    public void ReportsTheFirstRepeatedMemberInTheOrderOfTheText(string text, string path)
    {
        DocumentJson.Read(text, out var repeated);
        Assert.Equal(path, repeated);
        Assert.Throws<DocumentJson.Malformed>(() => DocumentJson.Parse(text));
    }

    [Fact]
    public void ARepeatedNameKeepsItsFirstPlaceAndTakesItsLastValue()
    {
        // As JSON.parse reads it, which is what section 9.7 holds an applied document to.
        var value = (DocumentObject)DocumentJson.Read("{\"a\": 1, \"b\": 2, \"a\": 3}", out var repeated);
        Assert.Equal("a", repeated);
        Assert.Equal(new[] { "a", "b" }, value.Members.Select(member => member.Key));
        Assert.Equal("3", ((DocumentNumber)value["a"]!).Lexeme);
    }

    [Fact]
    public void ReadsAMegabyteOfNamesInTimeThatGrowsWithTheText()
    {
        // Section 9.7 allows any number of names it has never heard of, so a peer can send a
        // megabyte of them and the agent stores it. The reader looked for a repeated name through
        // every member read so far: one check of this document took about half a minute, and the
        // store, the editor and a merge each check a document more than once.
        var text = Names(115_000);
        Assert.True(Encoding.UTF8.GetByteCount(text) <= Canonical.MaxDocumentBytes);
        var watch = Stopwatch.StartNew();
        var verdict = DocumentAcceptance.Check(text, Intake.Peer);
        Assert.True(watch.Elapsed < TimeSpan.FromSeconds(2), $"one check took {watch.Elapsed}");
        Assert.True(verdict.Valid, verdict.Sentence);

        // The repeat is still found, and reported where it is, however far into the text.
        var repeated = text[..^1] + ",\"" + Name(114_999) + "\": 1}";
        DocumentJson.Read(repeated, out var path);
        Assert.Equal(Name(114_999), path);
    }

    /// A valid document with [count] members of its own besides `version` and `machines`, each a
    /// short name nobody knows with the value 0.
    internal static string Names(int count)
    {
        var text = new StringBuilder("{\"version\":1,\"machines\":[]");
        for (var index = 0; index < count; index += 1) text.Append(",\"").Append(Name(index)).Append("\":0");
        return text.Append('}').ToString();
    }

    /// The [index]th of the shortest names that begin with an underscore, which no name a document
    /// knows does.
    internal static string Name(int index)
    {
        const string digits = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        var name = new StringBuilder("_");
        do
        {
            name.Insert(1, digits[index % digits.Length]);
            index /= digits.Length;
        } while (index > 0);
        return name.ToString();
    }

    [Fact]
    public void ReadsAndEditsADocumentAsDeepAsItAccepts()
    {
        // Section 9.7 lets a key nobody knows hold anything, nested as deep as its writer liked.
        // This client refused a document 65 levels deep, which validate.mjs and the agent read, and
        // the readers it edits documents with stopped at 64. A document accepted here has to open
        // in every one of them.
        var deepest = Deep(DocumentJson.MaxDepth);
        Assert.True(DocumentAcceptance.Check(deepest, Intake.Peer).Valid, DocumentAcceptance.Check(deepest, Intake.Peer).Sentence);
        Assert.Equal(deepest, DocumentJson.Print(DocumentJson.Parse(deepest)));
        var document = ControllerDocument.FromText(deepest);
        Assert.Equal("setup-a", document.Identity.Id);
        Assert.Equal("pi", Assert.Single(document.Decoded.Machines).Id);

        var plan = ControllerEditor.Plan(deepest, document.Decoded, "rename", root => root["machines"]![0]!["name"] = "Renamed");
        Assert.True(plan.IsApplicable, string.Join(" ", plan.Problems));
        Assert.Equal(deepest.Replace("\"Beacon\"", "\"Renamed\""), plan.Text);

        // One level deeper is no document at all, under every intake.
        Assert.Equal("not-a-json-object", Canonical.Rejection(Encoding.UTF8.GetBytes(Deep(DocumentJson.MaxDepth + 1))));
    }

    /// [of], or a valid document of its own, with an array [levels] down, counting the document
    /// itself, under a key nobody knows. Written the way JSON.stringify(document, null, 2) writes it.
    internal static string Deep(int levels, ControllerDocument? of = null)
    {
        var root = (DocumentObject)DocumentJson.Parse((of ?? Documents.Make("setup-a", 1, Array.Empty<string>())).Text);
        DocumentValue nested = new DocumentArray();
        for (var level = 2; level < levels; level += 1) nested = new DocumentArray(new[] { nested });
        root.Set("deep", nested);
        return DocumentJson.Print(root);
    }

    [Fact]
    public void KeepsEveryNumberAsItWasWritten()
    {
        var text = "{\n  \"a\": 22.0,\n  \"b\": 1E+2,\n  \"c\": -0,\n  \"d\": 123456789012345678901234567890\n}\n";
        Assert.Equal(text, DocumentJson.Print(DocumentJson.Parse(text)));
        Assert.True(JsonEquality.Equal(new DocumentNumber("22.0"), new DocumentNumber("2.2e1")));
        Assert.False(JsonEquality.Equal(new DocumentNumber("0.1"), new DocumentNumber("0.10000000000000001")));
    }

    [Fact]
    public void ComparesNumbersAlikeWhateverACultureWritesAMinusSignAs()
    {
        // Some cultures write the minus sign U+2212, some with a bidirectional mark in front. A
        // parse in the culture of the person's desktop took the `-` of 1e-5 for no sign at all and
        // threw in the middle of a merge.
        var before = CultureInfo.CurrentCulture;
        var culture = (CultureInfo)CultureInfo.InvariantCulture.Clone();
        culture.NumberFormat.NegativeSign = "\u200F\u2212";
        CultureInfo.CurrentCulture = culture;
        try
        {
            Assert.Equal("1e-5", JsonEquality.DecimalKey("0.00001"));
            Assert.True(JsonEquality.Equal(new DocumentNumber("1e-5"), new DocumentNumber("0.00001")));
            Assert.Equal(new BigInteger(-25), JsonEquality.Integer("-2.5e1"));
            Assert.Null(JsonEquality.Integer("25e-1"));
            Assert.Equal(8, LanPrefix.Parse("10.0.0.0/8")?.Length);
        }
        finally
        {
            CultureInfo.CurrentCulture = before;
        }
    }

    [Fact]
    public void PrintsTheSharedFixtureExactlyAsTheAgentStoredIt()
    {
        // The stored document of the shared config.read fixture is written the way JSON.stringify
        // writes it; printing it parsed gives back the same bytes, so its hash.
        var raw = Value(Repo.ReadFixture("config.read.json"), "raw");
        Assert.Equal(raw, DocumentJson.Print(DocumentJson.Parse(raw)));
    }

    [Fact]
    public void WhatThisDeviceWritesEveryOtherClientCanRebuild()
    {
        // The desktop wrote documents with .NET's default encoder, which escapes ', + and every
        // character outside ASCII. A client rebuilding the document from an agent before 3.1.0
        // writes those as themselves, so no other client could ever reproduce a document this one
        // wrote.
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 2, Array.Empty<string>()).Bytes);
        using var store = new ConfigStore(home.ConfigPath, watch: false);
        var edited = JsonNode.Parse(store.Document!.Text)!.AsObject();
        edited["machines"]![0]!["name"] = "Büro's +1 tower 🎉";
        var (document, problem) = store.ApplyEdit(edited.ToJsonString());
        Assert.Null(problem);

        var written = File.ReadAllText(home.ConfigPath);
        Assert.Contains("\"Büro's +1 tower 🎉\"", written);
        Assert.Equal(written, DocumentJson.Print(DocumentJson.Parse(written)));
        Assert.Equal(document!.Hash, Canonical.Hash(Encoding.UTF8.GetBytes(DocumentJson.Print(DocumentJson.Parse(written)))));
    }

    private static string Value(string json, string key) => ((DocumentString)((DocumentObject)DocumentJson.Parse(json))[key]!).Value;
}
