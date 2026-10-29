using LegionControl.Desktop.Config;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// How setup documents are read and printed: exactly the JSON every other client reads, and every
/// number spelled as it was written.
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
    public void KeepsEveryNumberAsItWasWritten()
    {
        var text = "{\n  \"a\": 22.0,\n  \"b\": 1E+2,\n  \"c\": -0,\n  \"d\": 123456789012345678901234567890\n}\n";
        Assert.Equal(text, DocumentJson.Print(DocumentJson.Parse(text)));
    }

    [Fact]
    public void PrintsTheSharedFixtureExactlyAsTheAgentStoredIt()
    {
        // The stored document of the shared config.read fixture is written the way JSON.stringify
        // writes it; printing it parsed gives back the same bytes, so its hash.
        var raw = Value(Repo.ReadFixture("config.read.json"), "raw");
        Assert.Equal(raw, DocumentJson.Print(DocumentJson.Parse(raw)));
    }

    private static string Value(string json, string key) => ((DocumentString)((DocumentObject)DocumentJson.Parse(json))[key]!).Value;
}
