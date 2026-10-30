import Foundation

/// Writing JSON the way JavaScript writes it.
///
/// `JSONSerialization.prettyPrinted` is not `JSON.stringify(value, null, 2)`: Foundation writes
/// `"a" : 1` and JavaScript writes `"a": 1`, and Foundation escapes a forward slash. The setup
/// document itself is written by `SetupJSON`, which keeps the order and the number spelling it read;
/// what is here is the part both share, how a string is quoted and how a number is spelled, and a
/// writer with sorted keys for this device's own files, which no other device reads.
enum JSONText {

    struct Unrepresentable: Error, Sendable {
        var message: String
    }

    /// `JSON.stringify(value, null, 2)` with sorted keys, for a file only this device writes.
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
    static func format(_ number: NSNumber) -> String {
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
