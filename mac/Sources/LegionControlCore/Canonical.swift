import CryptoKit
import Foundation

/// The canonical form of a controller document, and its hash.
///
/// This is the rule from `contract/v3.md`, implemented again here because the agent, this app, the
/// phone and the desktop client each hash the same document independently and have to agree byte for
/// byte. `contract/hash-vectors.json` is what proves they do, and the Mac test suite asserts every
/// vector in it.
///
/// The steps are ordered and the order matters. Every one of them exists because some editor, some
/// shell redirect or some phone keyboard produces bytes a person would call the same document and a
/// hash would not.
enum Canonical {

    /// The exact set of code points stripped from the two ends: tab, line feed, vertical tab, form
    /// feed, carriage return, space.
    ///
    /// Spelled out rather than delegated to Foundation. `.whitespacesAndNewlines` also strips
    /// U+00A0, U+2028 and the rest of the Unicode whitespace table, JavaScript's `trim` strips
    /// U+00A0 and U+FEFF, and .NET's `Trim` uses a third table. A document that begins with a
    /// non-breaking space has to hash the same on all four sides, so the set is fixed here.
    static let trimmed: Set<Unicode.Scalar> = ["\u{0009}", "\u{000A}", "\u{000B}", "\u{000C}", "\u{000D}", "\u{0020}"]

    struct NotUTF8: Error, Sendable {}

    /// The canonical bytes, or a throw for input that is not valid UTF-8.
    ///
    /// Refusing rather than substituting replacement characters is deliberate: hashing U+FFFD would
    /// hand back a stable hash for a document nobody can read, and the two sides would then agree
    /// forever about rubbish.
    static func bytes(_ raw: Data) throws -> Data {
        guard var text = String(data: raw, encoding: .utf8) else { throw NotUTF8() }

        // 1. One leading byte order mark, and only a leading one. A BOM further in is a zero width
        //    no-break space inside the document and stays where it is.
        if text.unicodeScalars.first == "\u{FEFF}" {
            text = String(String.UnicodeScalarView(text.unicodeScalars.dropFirst()))
        }

        // 2. Line endings. CRLF first, then any remaining lone CR, so a CRLF file does not turn into
        //    a blank line between every pair of lines.
        text = text.replacingOccurrences(of: "\r\n", with: "\n")
        text = text.replacingOccurrences(of: "\r", with: "\n")

        // 3. Both ends, using the fixed set above.
        var scalars = Array(text.unicodeScalars)
        var start = 0
        var end = scalars.count
        while start < end, trimmed.contains(scalars[start]) { start += 1 }
        while end > start, trimmed.contains(scalars[end - 1]) { end -= 1 }
        scalars = Array(scalars[start..<end])

        // 4. Exactly one trailing newline, whether the editor left none or twelve.
        scalars.append("\u{000A}")
        var view = String.UnicodeScalarView()
        view.append(contentsOf: scalars)
        return Data(String(view).utf8)
    }

    /// Lowercase hex sha256 of the canonical bytes.
    static func hash(_ raw: Data) throws -> String {
        sha256(try bytes(raw))
    }

    static func sha256(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
}
