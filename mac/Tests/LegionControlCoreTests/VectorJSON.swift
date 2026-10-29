import Foundation
@testable import LegionControlCore

/// A JSON value as the conformance vectors compare it (`comparison` in `contract/vectors/index.json`).
///
/// Objects compare by key set in any order, arrays element by element, strings by code point,
/// booleans and null by identity, and numbers, which the vectors keep to integers, by value. A field
/// whose name ends in `Text` holds a JSON document of its own and is compared parsed, object key
/// order ignored. Swift's `==` on strings would not do: it treats a composed and a decomposed `é`
/// as the same string, and the vectors are there to catch exactly that kind of difference.
indirect enum VectorJSON: Decodable, Equatable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([VectorJSON])
    case object([String: VectorJSON])

    init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        // Bool before Double, so `true` is never read as 1.
        if container.decodeNil() { self = .null }
        else if let value = try? container.decode(Bool.self) { self = .bool(value) }
        else if let value = try? container.decode(Double.self) { self = .number(value) }
        else if let value = try? container.decode(String.self) { self = .string(value) }
        else if let value = try? container.decode([VectorJSON].self) { self = .array(value) }
        else { self = .object(try container.decode([String: VectorJSON].self)) }
    }

    static func parse(_ data: Data) throws -> VectorJSON {
        try JSONDecoder().decode(VectorJSON.self, from: data)
    }

    subscript(key: String) -> VectorJSON? {
        guard case .object(let members) = self else { return nil }
        return members[key]
    }

    var string: String? { if case .string(let value) = self { value } else { nil } }
    var bool: Bool? { if case .bool(let value) = self { value } else { nil } }
    var number: Double? { if case .number(let value) = self { value } else { nil } }
    var array: [VectorJSON]? { if case .array(let value) = self { value } else { nil } }
    var object: [String: VectorJSON]? { if case .object(let value) = self { value } else { nil } }
    var isNull: Bool { self == .null }

    /// The value as `JSONSerialization` would have handed it over, for code that works on parsed
    /// JSON rather than on a typed model: a bool stays a bool and never turns into a number.
    var foundation: Any {
        switch self {
        case .null: NSNull()
        case .bool(let value): value
        case .number(let value): value.rounded() == value && abs(value) < 9_007_199_254_740_992 ? Int(value) : value
        case .string(let value): value
        case .array(let values): values.map(\.foundation)
        case .object(let members): members.mapValues(\.foundation)
        }
    }

    /// The value as an integer, when it is a number with no fraction.
    var int: Int? {
        guard let number, number.rounded() == number, abs(number) < 9_007_199_254_740_992 else { return nil }
        return Int(number)
    }

    /// Where `actual` differs from this expected value, by path. Empty when they agree.
    func differences(from actual: VectorJSON, at path: String = "$") -> [String] {
        switch (self, actual) {
        case (.object(let expected), .object(let got)):
            let missing = Set(expected.keys).subtracting(got.keys).sorted().map { "\(path).\($0) is missing" }
            let extra = Set(got.keys).subtracting(expected.keys).sorted().map { "\(path).\($0) is not expected" }
            let common = expected.keys.filter { got[$0] != nil }.sorted().flatMap { key -> [String] in
                let child = "\(path).\(key)"
                if key.hasSuffix("Text"), case .string(let text) = expected[key]!, case .string(let other) = got[key]! {
                    return Self.documentDifferences(text, other, at: child)
                }
                return expected[key]!.differences(from: got[key]!, at: child)
            }
            return missing + extra + common
        case (.array(let expected), .array(let got)):
            guard expected.count == got.count else { return ["\(path) has \(got.count) elements, not \(expected.count)"] }
            return zip(expected, got).enumerated().flatMap { index, pair in pair.0.differences(from: pair.1, at: "\(path)[\(index)]") }
        case (.string(let expected), .string(let got)):
            return Array(expected.unicodeScalars) == Array(got.unicodeScalars) ? [] : ["\(path) is \(Self.show(actual)), not \(Self.show(self))"]
        case (.number(let expected), .number(let got)):
            return expected == got ? [] : ["\(path) is \(Self.show(actual)), not \(Self.show(self))"]
        case (.bool(let expected), .bool(let got)):
            return expected == got ? [] : ["\(path) is \(got), not \(expected)"]
        case (.null, .null):
            return []
        default:
            return ["\(path) is \(Self.show(actual)), not \(Self.show(self))"]
        }
    }

    /// Two JSON documents held in strings, compared parsed: object key order ignored, array order
    /// exact, number lexemes exact. They are read with `SetupJSON`, because a double would call `22`
    /// and `22.0` the same, and keeping `22.0` as it was written is what a merge is held to.
    private static func documentDifferences(_ expected: String, _ actual: String, at path: String) -> [String] {
        guard let left = try? SetupJSON.parse(expected) else { return ["\(path) in the vector is not a JSON document"] }
        guard let right = try? SetupJSON.parse(actual) else { return ["\(path) is not a JSON document"] }
        return lexicalDifferences(left, right, at: path)
    }

    private static func lexicalDifferences(_ expected: SetupJSON, _ actual: SetupJSON, at path: String) -> [String] {
        switch (expected, actual) {
        case (.object(let left), .object(let right)):
            let names = Set(left.map(\.name)), others = Set(right.map(\.name))
            let missing = names.subtracting(others).sorted().map { "\(path).\($0) is missing" }
            let extra = others.subtracting(names).sorted().map { "\(path).\($0) is not expected" }
            let common = left.filter { others.contains($0.name) }.flatMap { member in
                lexicalDifferences(member.value, actual[member.name]!, at: "\(path).\(member.name)")
            }
            return missing + extra + common
        case (.array(let left), .array(let right)):
            guard left.count == right.count else { return ["\(path) has \(right.count) elements, not \(left.count)"] }
            return zip(left, right).enumerated().flatMap { index, pair in lexicalDifferences(pair.0, pair.1, at: "\(path)[\(index)]") }
        case (.string(let left), .string(let right)):
            return SetupJSON.same(left, right) ? [] : ["\(path) is \(actual.compactText), not \(expected.compactText)"]
        default:
            return expected == actual ? [] : ["\(path) is \(actual.compactText), not \(expected.compactText)"]
        }
    }

    private static func show(_ value: VectorJSON) -> String {
        switch value {
        case .null: "null"
        case .bool(let value): String(value)
        case .number(let value): value.rounded() == value ? String(Int(value)) : String(value)
        case .string(let value): "\"\(value.unicodeScalars.map { $0.isASCII && $0.value >= 0x20 ? String($0) : "\\u{\(String($0.value, radix: 16))}" }.joined())\""
        case .array(let values): "[\(values.map(show).joined(separator: ", "))]"
        case .object(let members): "{\(members.keys.sorted().map { "\($0): \(show(members[$0]!))" }.joined(separator: ", "))}"
        }
    }
}

extension VectorJSON: ExpressibleByStringLiteral, ExpressibleByBooleanLiteral, ExpressibleByIntegerLiteral,
                      ExpressibleByNilLiteral, ExpressibleByArrayLiteral, ExpressibleByDictionaryLiteral {
    init(stringLiteral value: String) { self = .string(value) }
    init(booleanLiteral value: Bool) { self = .bool(value) }
    init(integerLiteral value: Int) { self = .number(Double(value)) }
    init(nilLiteral: ()) { self = .null }
    init(arrayLiteral elements: VectorJSON...) { self = .array(elements) }
    init(dictionaryLiteral elements: (String, VectorJSON)...) {
        self = .object(Dictionary(elements, uniquingKeysWith: { _, last in last }))
    }

    /// A string, or null when there is none.
    init(_ value: String?) { self = value.map(VectorJSON.string) ?? .null }
    /// A number, or null when there is none.
    init(_ value: Int?) { self = value.map { .number(Double($0)) } ?? .null }
}
