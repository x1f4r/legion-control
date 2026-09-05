import Foundation

/// A reply this app passes through rather than decodes, for the agent's diagnostic bundle.
struct RawJSON: Decodable, Sendable {
    var text: String

    init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        let value = try container.decode(JSONAny.self)
        let data = try JSONSerialization.data(withJSONObject: value.value,
                                              options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
        text = String(decoding: data, as: UTF8.self)
    }
}

/// Just enough of a dynamic JSON value to re-encode a document whose keys this build has never seen.
///
/// `@unchecked Sendable` because `value` is `Any`, and the compiler cannot see that everything ever
/// put in it is an immutable JSON value: a dictionary, an array, a string, a number, a bool or null.
/// Nothing writes to it after `init`.
struct JSONAny: Decodable, @unchecked Sendable {
    let value: Any

    init(from decoder: any Decoder) throws {
        if let container = try? decoder.container(keyedBy: Key.self) {
            var object: [String: Any] = [:]
            for key in container.allKeys {
                object[key.stringValue] = try container.decode(JSONAny.self, forKey: key).value
            }
            value = object
            return
        }
        if var container = try? decoder.unkeyedContainer() {
            var array: [Any] = []
            while !container.isAtEnd {
                array.append(try container.decode(JSONAny.self).value)
            }
            value = array
            return
        }
        let single = try decoder.singleValueContainer()
        if single.decodeNil() { value = NSNull() }
        else if let bool = try? single.decode(Bool.self) { value = bool }
        else if let int = try? single.decode(Int.self) { value = int }
        else if let double = try? single.decode(Double.self) { value = double }
        else { value = try single.decode(String.self) }
    }

    private struct Key: CodingKey {
        var stringValue: String
        var intValue: Int?
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { self.intValue = intValue; stringValue = String(intValue) }
    }
}
