import Foundation

/// Writing a JSON document the way every other device writes it.
///
/// This exists because of one hard requirement: the hash the whole fleet compares is a hash of the
/// document's bytes, so two devices that serialise the same object differently hold two different
/// documents as far as everybody else is concerned. `JSONSerialization.prettyPrinted` is not the
/// same as `JSON.stringify(value, null, 2)` — Foundation writes `"a" : 1` and JavaScript writes
/// `"a": 1` — and the stored form in `contract/fixtures/config.read.json` is the JavaScript one.
///
/// So the writer is spelled out here rather than delegated, exactly as the canonicaliser is.
///
/// Keys are sorted. JavaScript preserves insertion order and a Swift dictionary has none to
/// preserve, so sorting is the only rule two languages can both follow; see
/// integration-notes-mac.txt for the open question about documents written by a client that does
/// not sort.
enum JSONText {

    struct Unrepresentable: Error, Sendable {
        var message: String
    }

    /// `JSON.stringify(value, null, 2)` with sorted keys, then one trailing newline, then the
    /// canonical rule applied so the result is exactly what gets hashed and stored.
    static func canonicalDocument(_ value: Any) throws -> Data {
        var out = ""
        try write(value, into: &out, indent: 0)
        return try Canonical.bytes(Data((out + "\n").utf8))
    }

    /// The same, without the canonical pass, for anything that is not a controller document.
    static func stringify(_ value: Any) throws -> String {
        var out = ""
        try write(value, into: &out, indent: 0)
        return out
    }

    private static func write(_ value: Any, into out: inout String, indent: Int) throws {
        let pad = String(repeating: " ", count: indent * 2)
        let innerPad = String(repeating: " ", count: (indent + 1) * 2)

        switch value {
        case is NSNull:
            out += "null"

        case let number as NSNumber:
            out += format(number)

        case let text as String:
            out += quote(text)

        case let array as [Any]:
            if array.isEmpty {
                out += "[]"
                return
            }
            out += "[\n"
            for (index, element) in array.enumerated() {
                out += innerPad
                try write(element, into: &out, indent: indent + 1)
                out += index == array.count - 1 ? "\n" : ",\n"
            }
            out += pad + "]"

        case let object as [String: Any]:
            if object.isEmpty {
                out += "{}"
                return
            }
            out += "{\n"
            let keys = object.keys.sorted()
            for (index, key) in keys.enumerated() {
                out += innerPad + quote(key) + ": "
                try write(object[key] ?? NSNull(), into: &out, indent: indent + 1)
                out += index == keys.count - 1 ? "\n" : ",\n"
            }
            out += pad + "}"

        default:
            throw Unrepresentable(message: "\(type(of: value)) is not a JSON value.")
        }
    }

    /// Numbers as JavaScript writes them: an integer with no decimal point, a double with the
    /// shortest representation that round-trips, and a bool as a bool.
    private static func format(_ number: NSNumber) -> String {
        // CFNumber does not distinguish a Bool from a 0 or 1 by type alone.
        if CFGetTypeID(number) == CFBooleanGetTypeID() {
            return number.boolValue ? "true" : "false"
        }
        let type = String(cString: number.objCType)
        if type == "d" || type == "f" {
            let double = number.doubleValue
            if double == double.rounded(), abs(double) < 1e15 {
                return String(Int64(double))
            }
            return "\(double)"
        }
        return number.stringValue
    }

    /// A JSON string literal, escaping the same set JavaScript escapes and no more. In particular a
    /// forward slash is left alone, which is what `withoutEscapingSlashes` means in Foundation and
    /// what JavaScript does by default.
    static func quote(_ text: String) -> String {
        var out = "\""
        for scalar in text.unicodeScalars {
            switch scalar {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\u{08}": out += "\\b"
            case "\u{0C}": out += "\\f"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            default:
                if scalar.value < 0x20 {
                    out += String(format: "\\u%04x", scalar.value)
                } else {
                    out.unicodeScalars.append(scalar)
                }
            }
        }
        return out + "\""
    }
}
