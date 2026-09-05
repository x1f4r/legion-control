using System.Security.Cryptography;
using System.Text;

namespace LegionControl.Desktop.Config;

/// The canonical form of a controller document, and its hash.
///
/// The reference implementation lives at contract/tools/canonical.mjs, and contract/hash-vectors.json
/// is what proves the four implementations agree. The steps are ordered and the order matters:
/// every one of them exists because some editor, some shell redirect or some phone keyboard
/// produces bytes a person would call the same document and a hash would not.
public static class Canonical
{
    /// The exact set of code points stripped from the two ends: tab, line feed, vertical tab, form
    /// feed, carriage return, space.
    ///
    /// Spelled out rather than delegated to string.Trim, because the runtimes do not agree. .NET's
    /// Trim strips every Unicode whitespace, including U+00A0 and U+FEFF; JavaScript's String.trim
    /// strips a different set again. A document that begins with a non-breaking space has to hash
    /// the same on all four sides, so the set is fixed here and nowhere else.
    public static readonly char[] TrimCodePoints =
    {
        '\u0009', '\u000A', '\u000B', '\u000C', '\u000D', '\u0020',
    };

    private static bool IsTrimmable(char character) =>
        character is '\u0009' or '\u000A' or '\u000B' or '\u000C' or '\u000D' or '\u0020';

    /// Thrown rather than papered over: hashing replacement characters would hand back a stable
    /// hash for a document nobody can read, and the two sides would then agree forever about
    /// rubbish.
    public sealed class NotUtf8() : Exception("the document is not valid UTF-8");

    /// Whether these bytes decode as UTF-8 without loss.
    public static bool IsValidUtf8(byte[] bytes)
    {
        try
        {
            _ = new UTF8Encoding(false, true).GetString(bytes);
            return true;
        }
        catch (DecoderFallbackException)
        {
            return false;
        }
    }

    /// The canonical bytes of a controller document. Throws [NotUtf8] when the input is not UTF-8.
    public static byte[] Bytes(byte[] raw)
    {
        string text;
        try
        {
            text = new UTF8Encoding(false, true).GetString(raw);
        }
        catch (DecoderFallbackException)
        {
            throw new NotUtf8();
        }

        // 1. One leading byte order mark, and only a leading one. A BOM further in is a zero width
        //    no-break space inside the document and stays where it is.
        if (text.Length > 0 && text[0] == '\uFEFF') text = text[1..];

        // 2. Line endings. CRLF first, then any remaining lone CR, so a CRLF file does not turn
        //    into a blank line between every pair of lines.
        text = text.Replace("\r\n", "\n").Replace("\r", "\n");

        // 3. Both ends, using the fixed set above and nothing else.
        var start = 0;
        var end = text.Length;
        while (start < end && IsTrimmable(text[start])) start += 1;
        while (end > start && IsTrimmable(text[end - 1])) end -= 1;
        text = text[start..end];

        // 4. Exactly one trailing newline, whether the editor left none or twelve.
        return new UTF8Encoding(false).GetBytes(text + "\n");
    }

    public static string Hash(byte[] raw) => Sha256Hex(Bytes(raw));

    public static string Sha256Hex(byte[] bytes) =>
        Convert.ToHexString(SHA256.HashData(bytes)).ToLowerInvariant();
}
