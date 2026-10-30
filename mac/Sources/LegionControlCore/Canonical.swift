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
        guard isUTF8(raw) else { throw NotUTF8() }

        // 1. One leading byte order mark, and only a leading one. A second one, or one further in,
        //    is a zero width no-break space inside the document and stays where it is.
        let bom: [UInt8] = [0xEF, 0xBB, 0xBF]
        let body = raw.starts(with: bom) ? raw.dropFirst(bom.count) : raw[...]
        var text = String(decoding: body, as: UTF8.self)

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

    /// Whether these bytes are valid UTF-8, by the reference's test: decoding them and encoding the
    /// result again gives the same bytes, which fails exactly when a byte had to be replaced.
    ///
    /// Foundation is never asked. `String(data:encoding:)` and `JSONSerialization` both drop a
    /// leading byte order mark without saying so, which is how a document starting with two of them
    /// lost one there, lost the other here, and was accepted as a plain document that every other
    /// implementation refuses (C13).
    static func isUTF8(_ raw: Data) -> Bool {
        Data(String(decoding: raw, as: UTF8.self).utf8) == raw
    }

    /// Lowercase hex sha256 of the canonical bytes.
    static func hash(_ raw: Data) throws -> String {
        sha256(try bytes(raw))
    }

    static func sha256(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    // MARK: - What section 12 refuses

    /// The most a setup document may be, counted on the bytes as given.
    static let maxDocumentBytes = 1_048_576

    /// Why section 12 refuses a document before anything is stored, sent or adopted.
    enum Rejection: String, Sendable, Equatable {
        case tooLarge = "too-large"
        case invalidUTF8 = "invalid-utf8"
        case empty
        case notAJSONObject = "not-a-json-object"

        /// The refusal as the end of a sentence about the document.
        var sentence: String {
            switch self {
            case .tooLarge: "it is larger than the 1 MiB a setup may be."
            case .invalidUTF8: "it is not valid UTF-8."
            case .empty: "it is empty."
            case .notAJSONObject: "it is not a JSON object with a version and a machines list."
            }
        }
    }

    /// Why these bytes are no setup document at all, or nil when they are one, in the reference's
    /// order: too large, not UTF-8, empty, not a JSON object with a numeric `version` and a
    /// `machines` array.
    ///
    /// Size is judged on the bytes as given, because that is what was read; the rest on the
    /// canonical bytes, so a byte order mark or blank lines around a document never make it
    /// unreadable. The text is parsed with `SetupJSON` rather than `JSONSerialization`, which skips
    /// a leading U+FEFF: after the one byte order mark the rule strips, a second one is content, and
    /// the document is refused, as it is everywhere else. A repeated member name is not refused
    /// here, any more than `JSON.parse` refuses it; section 9.7 does that.
    static func rejection(_ raw: Data) -> Rejection? {
        if raw.count > maxDocumentBytes { return .tooLarge }
        guard let canonical = try? bytes(raw) else { return .invalidUTF8 }
        if canonical.count == 1 { return .empty }
        guard let parsed = try? SetupJSON.parse(canonical, repeats: .lastWins),
              parsed.isObject, parsed["version"]?.numberValue != nil, parsed["machines"]?.items != nil
        else { return .notAJSONObject }
        return nil
    }
}
