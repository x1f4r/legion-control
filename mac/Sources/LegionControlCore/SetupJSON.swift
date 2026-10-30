import Foundation

/// A setup document as JSON that keeps what a merge or an edit must not change: the order members
/// were written in and the spelling of every number.
///
/// `JSONSerialization` keeps neither. Its objects are dictionaries, so the Mac used to sort every key
/// it wrote, and its numbers are doubles, so `22.0` read from another peer's document was written
/// back as `22`. Each of those turns a document nobody edited into one with another hash, which every
/// other device then reads as a change, and a merge built on it moved entries to the end of their
/// arrays and rewrote values it never touched (C6). This is the tree `contract/tools/setup-sync.mjs`
/// works on, ported: objects are member lists in order, numbers are their lexemes, strings are
/// decoded, and writing a tree back gives `JSON.stringify(value, null, 2)` with every number exactly
/// as it was read.
///
/// The synthesized `==` compares two trees as written, order and spelling included. Whether two
/// trees are the same JSON value is `SetupJSON.equal`, which is what a merge asks.
indirect enum SetupJSON: Sendable, Equatable {
    case object([Member])
    case array([SetupJSON])
    case string(String)
    /// A number as it was written: `22.0` stays `22.0`, `1e2` stays `1e2`.
    case number(String)
    case bool(Bool)
    case null

    struct Member: Sendable, Equatable {
        var name: String
        var value: SetupJSON
    }

    struct ParseError: Error, Sendable, CustomStringConvertible {
        var message: String
        var offset: Int

        var description: String { "\(message) at offset \(offset)" }
    }

    /// What a parse does with an object that repeats a member name.
    enum Repeats: Sendable {
        /// Refuse it. Parsers disagree about which copy counts, so a document that repeats a name
        /// means different things on different devices and is never merged or edited (section 9.7).
        case refused
        /// Keep the last copy where the first one stood, the way `JSON.parse` reads it. Only for
        /// reading a document this device already applies, so its other parts can still be used.
        case lastWins
    }

    // MARK: - Reading

    /// Parse exactly one JSON value from UTF-8 text.
    static func parse(_ data: Data, repeats: Repeats = .refused) throws -> SetupJSON {
        try parseReportingRepeats(data, repeats: repeats).value
    }

    static func parse(_ text: String, repeats: Repeats = .refused) throws -> SetupJSON {
        try parse(Data(text.utf8), repeats: repeats)
    }

    /// Parse, and say where the first repeated member name is (`machines[0].systems[1].agent`), or
    /// nil when there is none. With `.refused` a repeat throws instead.
    static func parseReportingRepeats(_ data: Data, repeats: Repeats = .lastWins) throws -> (value: SetupJSON, repeated: String?) {
        var parser = Parser(bytes: Array(data), repeats: repeats)
        let value = try parser.document()
        return (value, parser.repeated)
    }

    // MARK: - Writing

    /// The document text of this tree: two-space indentation, LF, one final newline. It is the layout
    /// `JSON.stringify(value, null, 2)` gives, with every number written exactly as it was read.
    var documentText: String { printed(indent: "") + "\n" }

    var documentData: Data { Data(documentText.utf8) }

    /// The value on one line, for a message.
    var compactText: String {
        switch self {
        case .object(let members):
            "{" + members.map { JSONText.quote($0.name) + ":" + $0.value.compactText }.joined(separator: ",") + "}"
        case .array(let items):
            "[" + items.map(\.compactText).joined(separator: ",") + "]"
        default:
            printed(indent: "")
        }
    }

    /// The value written out from the given indentation, without a final newline.
    func printed(indent: String = "") -> String {
        let inner = indent + "  "
        switch self {
        case .object(let members):
            guard !members.isEmpty else { return "{}" }
            let lines = members.map { inner + JSONText.quote($0.name) + ": " + $0.value.printed(indent: inner) }
            return "{\n" + lines.joined(separator: ",\n") + "\n" + indent + "}"
        case .array(let items):
            guard !items.isEmpty else { return "[]" }
            let lines = items.map { inner + $0.printed(indent: inner) }
            return "[\n" + lines.joined(separator: ",\n") + "\n" + indent + "]"
        case .string(let value):
            return JSONText.quote(value)
        case .number(let lexeme):
            return lexeme
        case .bool(let value):
            return value ? "true" : "false"
        case .null:
            return "null"
        }
    }

    // MARK: - Looking inside

    var members: [Member]? { if case .object(let members) = self { members } else { nil } }
    var items: [SetupJSON]? { if case .array(let items) = self { items } else { nil } }
    var stringValue: String? { if case .string(let value) = self { value } else { nil } }
    var boolValue: Bool? { if case .bool(let value) = self { value } else { nil } }
    var isObject: Bool { members != nil }

    /// The number as JavaScript reads it, which is what the contract's checks are written against:
    /// `22.0` is the integer 22, `1e400` is infinite and no integer at all.
    var numberValue: Double? { if case .number(let lexeme) = self { Double(lexeme) } else { nil } }

    /// Whether this is a number JavaScript's `Number.isInteger` says is an integer.
    var isInteger: Bool { numberValue.map { $0.isFinite && $0.rounded() == $0 } ?? false }

    /// The value of a member, or nil when this is not an object or has no member of that name.
    ///
    /// Names are compared code point by code point. Swift's own `==` on strings calls a composed and
    /// a decomposed `é` the same, and JSON calls them two names.
    subscript(name: String) -> SetupJSON? {
        get { members?.first { Self.same($0.name, name) }?.value }
        set {
            guard case .object(var members) = self else { return }
            self = .null
            let index = members.firstIndex { Self.same($0.name, name) }
            switch (index, newValue) {
            case (let index?, let value?): members[index].value = value
            case (let index?, nil): members.remove(at: index)
            case (nil, let value?): members.append(Member(name: name, value: value))
            case (nil, nil): break
            }
            self = .object(members)
        }
    }

    /// The string `id` of an object, or nil.
    var id: String? { self["id"]?.stringValue }

    /// The objects of an array, or none.
    var objects: [SetupJSON] { (items ?? []).filter(\.isObject) }

    /// Change the array held in a member, in place. Nothing happens when there is no such array.
    mutating func modifyItems(of name: String, _ change: (inout [SetupJSON]) throws -> Void) rethrows {
        guard case .array(var items)? = self[name] else { return }
        self[name] = .null
        try change(&items)
        self[name] = .array(items)
    }

    /// Change the element of a member's array that has this id, in place.
    mutating func modifyItem(in name: String, id: String, _ change: (inout SetupJSON) throws -> Void) rethrows {
        try modifyItems(of: name) { items in
            guard let index = items.firstIndex(where: { $0.id == id }) else { return }
            try change(&items[index])
        }
    }

    /// Two strings as JSON compares them: the same code points in the same order.
    static func same(_ a: String, _ b: String) -> Bool { a.unicodeScalars.elementsEqual(b.unicodeScalars) }

    /// Code point order. Swift compares strings by their canonical form and JavaScript by UTF-16
    /// code units, and the contract lists differences in neither: UTF-8 byte order is code point
    /// order.
    static func precedes(_ a: String, _ b: String) -> Bool { a.utf8.lexicographicallyPrecedes(b.utf8) }

    // MARK: - Equality by value

    /// Whether two values are the same JSON value: objects with the same names in any order and equal
    /// values, arrays element by element, strings by code point, numbers by exact decimal value, and
    /// absent only equal to absent. `22`, `22.0` and `2.2e1` are equal; no floating point decides it.
    static func equal(_ a: SetupJSON?, _ b: SetupJSON?) -> Bool {
        switch (a, b) {
        case (nil, nil): return true
        case (nil, _), (_, nil): return false
        case (.object(let left)?, .object(let right)?):
            guard left.count == right.count else { return false }
            let other = SetupJSON.object(right)
            return left.allSatisfy { member in equal(member.value, other[member.name]) }
        case (.array(let left)?, .array(let right)?):
            return left.count == right.count && zip(left, right).allSatisfy { equal($0, $1) }
        case (.string(let left)?, .string(let right)?):
            return same(left, right)
        case (.number(let left)?, .number(let right)?):
            return decimalKey(left) == decimalKey(right)
        case (.bool(let left)?, .bool(let right)?):
            return left == right
        case (.null?, .null?):
            return true
        default:
            return false
        }
    }

    /// A number lexeme as its exact decimal value, `<sign><digits>e<exponent>` with no leading or
    /// trailing zeros in the digits: `22`, `22.0`, `2.2e1` and `220e-1` are all `22e0`, and `0` and
    /// `-0` are both `0`. Two lexemes are the same number exactly when their keys are equal, at any
    /// size and precision, so `0.1` and `0.10000000000000001` are different numbers here although a
    /// double cannot tell them apart. Nil for text that is not a JSON number.
    static func decimalKey(_ lexeme: String) -> String? {
        guard let parts = NumberParts(lexeme) else { return nil }
        let digits = String((parts.whole + parts.fraction).drop { $0 == "0" })
        guard !digits.isEmpty else { return "0" }
        var significant = Substring(digits)
        while significant.last == "0" { significant.removeLast() }
        let shift = digits.count - significant.count - parts.fraction.count
        return parts.sign + significant + "e" + BigDecimal.adding(shift, to: parts.exponent)
    }

    /// The integer a number holds, or nil when it holds none or one too large for an Int.
    var integerValue: Int? {
        guard case .number(let lexeme) = self, let key = Self.decimalKey(lexeme) else { return nil }
        if key == "0" { return 0 }
        let pieces = key.split(separator: "e")
        guard pieces.count == 2, let scale = Int(pieces[1]), scale >= 0, scale <= 18 else { return nil }
        return Int(String(pieces[0]) + String(repeating: "0", count: scale))
    }

    // MARK: - Foundation values

    struct Unrepresentable: Error, Sendable {
        var message: String
    }

    /// The value as `JSONSerialization` would have handed it over, for code that edits parsed JSON:
    /// objects are dictionaries, arrays are arrays, numbers are `NSNumber`, and a bool stays a bool.
    var foundationValue: Any {
        switch self {
        case .object(let members):
            var object: [String: Any] = [:]
            for member in members { object[member.name] = member.value.foundationValue }
            return object
        case .array(let items):
            return items.map(\.foundationValue)
        case .string(let value):
            return value
        case .number(let lexeme):
            if let integer = Int(lexeme) { return NSNumber(value: integer) }
            return NSNumber(value: Double(lexeme) ?? .nan)
        case .bool(let value):
            return value
        case .null:
            return NSNull()
        }
    }

    /// A Foundation value as a tree. New object keys are written in code point order, because a
    /// dictionary has no order of its own to keep.
    init(foundation value: Any) throws {
        self = try Self.rebased(value, onto: nil)
    }

    /// A Foundation value as a tree that keeps the original's spelling wherever the value did not
    /// change.
    ///
    /// This is how an edit made to a parsed dictionary is written back without rewriting what it did
    /// not touch. A value equal to the original's is the original, exactly as written. An object keeps
    /// the original's member order and appends what is new; an array matches each element to the
    /// original one with the same `id`, then to an equal one, then to the one at the same place, so a
    /// machine moved up the list keeps its own spelling rather than borrowing its neighbour's.
    static func rebased(_ value: Any, onto original: SetupJSON?) throws -> SetupJSON {
        switch value {
        case is NSNull:
            return .null
        case let number as NSNumber:
            if CFGetTypeID(number) == CFBooleanGetTypeID() { return .bool(number.boolValue) }
            if case .number(let written)? = original, carries(number, written) { return .number(written) }
            let lexeme = JSONText.format(number)
            guard decimalKey(lexeme) != nil else { throw Unrepresentable(message: "\(number) is not a JSON number.") }
            return .number(lexeme)
        case let text as String:
            if case .string(let written)? = original, same(written, text) { return .string(written) }
            return .string(text)
        case let array as [Any]:
            let before = original?.items ?? []
            var used = Set<Int>()
            var items: [SetupJSON] = []
            for (index, element) in array.enumerated() {
                let id = (element as? [String: Any])?["id"] as? String
                let match = before.indices.first { !used.contains($0) && id != nil && before[$0].id == id }
                    ?? before.indices.first { !used.contains($0) && holds(element, before[$0]) }
                    ?? (index < before.count && !used.contains(index) && before[index].id == id ? index : nil)
                if let match { used.insert(match) }
                items.append(try Self.rebased(element, onto: match.map { before[$0] }))
            }
            return .array(items)
        case let object as [String: Any]:
            var members: [Member] = []
            for member in original?.members ?? [] {
                guard let new = object.first(where: { same($0.key, member.name) })?.value else { continue }
                members.append(Member(name: member.name, value: try Self.rebased(new, onto: member.value)))
            }
            let added = object.keys
                .filter { key in !members.contains { same($0.name, key) } }
                .sorted(by: precedes)
            for key in added {
                members.append(Member(name: key, value: try Self.rebased(object[key] as Any, onto: nil)))
            }
            let rebuilt = SetupJSON.object(members)
            if let original, equal(original, rebuilt) { return original }
            return rebuilt
        default:
            throw Unrepresentable(message: "\(type(of: value)) is not a JSON value.")
        }
    }

    /// Whether a number an edit handed back is the one written as `lexeme`.
    ///
    /// An integer is compared exactly. Anything else is compared with the double `foundationValue`
    /// read the lexeme as, which is all an edit ever had of it: 12345678901234567890 and
    /// 0.10000000000000000001 are not doubles, and 1e400 is not even finite, so formatting the double
    /// again would write another number, or none, and an edit anywhere else would change a value
    /// nobody touched.
    private static func carries(_ number: NSNumber, _ lexeme: String) -> Bool {
        if CFNumberIsFloatType(number as CFNumber) { return Double(lexeme) == number.doubleValue }
        return decimalKey(JSONText.format(number)) == decimalKey(lexeme)
    }

    /// Whether a Foundation value is this tree's value, numbers compared as `carries` compares them,
    /// so that an array element nobody changed finds the original it came from. Asked of the value
    /// itself rather than of a tree written from it, which a number no double holds would not survive.
    private static func holds(_ value: Any, _ tree: SetupJSON) -> Bool {
        switch (value, tree) {
        case (is NSNull, .null):
            return true
        case (let number as NSNumber, _):
            if CFGetTypeID(number) == CFBooleanGetTypeID() { return tree == .bool(number.boolValue) }
            guard case .number(let written) = tree else { return false }
            return carries(number, written)
        case (let text as String, .string(let written)):
            return same(written, text)
        case (let array as [Any], .array(let items)):
            return array.count == items.count && zip(array, items).allSatisfy { holds($0, $1) }
        case (let object as [String: Any], .object(let members)):
            return object.count == members.count && members.allSatisfy { member in
                object.first { same($0.key, member.name) }.map { holds($0.value, member.value) } ?? false
            }
        default:
            return false
        }
    }
}

// MARK: - The parser

extension SetupJSON {
    /// A JSON number lexeme split into its parts, or nil for text that is not one.
    fileprivate struct NumberParts {
        var sign: String
        var whole: String
        var fraction: String
        var exponent: String

        init?(_ lexeme: String) {
            let bytes = Array(lexeme.utf8)
            var at = 0
            func digits() -> String {
                let start = at
                while at < bytes.count, (0x30...0x39).contains(bytes[at]) { at += 1 }
                return String(decoding: bytes[start..<at], as: UTF8.self)
            }
            sign = bytes.first == UInt8(ascii: "-") ? "-" : ""
            at = sign.count
            whole = digits()
            guard !whole.isEmpty, whole == "0" || !whole.hasPrefix("0") else { return nil }
            fraction = ""
            if at < bytes.count, bytes[at] == UInt8(ascii: ".") {
                at += 1
                fraction = digits()
                guard !fraction.isEmpty else { return nil }
            }
            exponent = "0"
            if at < bytes.count, bytes[at] == UInt8(ascii: "e") || bytes[at] == UInt8(ascii: "E") {
                at += 1
                var exponentSign = ""
                if at < bytes.count, bytes[at] == UInt8(ascii: "+") || bytes[at] == UInt8(ascii: "-") {
                    exponentSign = bytes[at] == UInt8(ascii: "-") ? "-" : ""
                    at += 1
                }
                let value = digits()
                guard !value.isEmpty else { return nil }
                exponent = exponentSign + value
            }
            guard at == bytes.count else { return nil }
        }
    }

    /// Where the parser is inside the document, so a repeated member can be named by its path.
    private enum Step {
        case member(String)
        case item(Int)
    }

    private struct Parser {
        let bytes: [UInt8]
        let repeats: Repeats
        var at = 0
        var steps: [Step] = []
        var repeated: String?

        /// Deeper than any setup document, and shallow enough that a hostile one cannot exhaust the
        /// stack.
        static let maximumDepth = 512

        init(bytes: [UInt8], repeats: Repeats) {
            self.bytes = bytes
            self.repeats = repeats
        }

        mutating func document() throws -> SetupJSON {
            let value = try parseValue(depth: 0)
            skipSpace()
            guard at == bytes.count else { throw fail("text after the JSON value") }
            return value
        }

        func fail(_ message: String) -> ParseError { ParseError(message: message, offset: at) }

        mutating func skipSpace() {
            while at < bytes.count, [0x20, 0x09, 0x0A, 0x0D].contains(bytes[at]) { at += 1 }
        }

        var path: String {
            var text = ""
            for step in steps {
                switch step {
                case .member(let name): text += text.isEmpty ? name : ".\(name)"
                case .item(let index): text += "[\(index)]"
                }
            }
            return text
        }

        mutating func parseValue(depth: Int) throws -> SetupJSON {
            guard depth < Self.maximumDepth else { throw fail("the document is nested too deeply") }
            skipSpace()
            guard at < bytes.count else { throw fail("a JSON value was expected") }
            switch bytes[at] {
            case UInt8(ascii: "{"): return try parseObject(depth: depth)
            case UInt8(ascii: "["): return try parseArray(depth: depth)
            case UInt8(ascii: "\""): return .string(try parseString())
            default: break
            }
            for (word, literal) in [("true", SetupJSON.bool(true)), ("false", .bool(false)), ("null", .null)] {
                let spelled = Array(word.utf8)
                if bytes[at...].starts(with: spelled) {
                    at += spelled.count
                    return literal
                }
            }
            return .number(try parseNumber())
        }

        mutating func parseObject(depth: Int) throws -> SetupJSON {
            at += 1
            var members: [Member] = []
            var names: [Data: Int] = [:]
            skipSpace()
            if at < bytes.count, bytes[at] == UInt8(ascii: "}") {
                at += 1
                return .object([])
            }
            while true {
                skipSpace()
                guard at < bytes.count, bytes[at] == UInt8(ascii: "\"") else { throw fail("a member name was expected") }
                let name = try parseString()
                skipSpace()
                guard at < bytes.count, bytes[at] == UInt8(ascii: ":") else { throw fail("a colon was expected") }
                at += 1
                steps.append(.member(name))
                let key = Data(name.utf8)
                if let first = names[key] {
                    guard repeats == .lastWins else { throw fail("the member name \(JSONText.quote(name)) is repeated") }
                    if repeated == nil { repeated = path }
                    members[first].value = try parseValue(depth: depth + 1)
                } else {
                    names[key] = members.count
                    members.append(Member(name: name, value: try parseValue(depth: depth + 1)))
                }
                steps.removeLast()
                skipSpace()
                guard at < bytes.count else { throw fail("a comma or a closing brace was expected") }
                if bytes[at] == UInt8(ascii: ",") {
                    at += 1
                    continue
                }
                if bytes[at] == UInt8(ascii: "}") {
                    at += 1
                    return .object(members)
                }
                throw fail("a comma or a closing brace was expected")
            }
        }

        mutating func parseArray(depth: Int) throws -> SetupJSON {
            at += 1
            var items: [SetupJSON] = []
            skipSpace()
            if at < bytes.count, bytes[at] == UInt8(ascii: "]") {
                at += 1
                return .array([])
            }
            while true {
                steps.append(.item(items.count))
                items.append(try parseValue(depth: depth + 1))
                steps.removeLast()
                skipSpace()
                guard at < bytes.count else { throw fail("a comma or a closing bracket was expected") }
                if bytes[at] == UInt8(ascii: ",") {
                    at += 1
                    continue
                }
                if bytes[at] == UInt8(ascii: "]") {
                    at += 1
                    return .array(items)
                }
                throw fail("a comma or a closing bracket was expected")
            }
        }

        /// A string, decoded: exactly the escapes JSON has, and no raw control character inside the
        /// quotes. A lone surrogate, which a Swift string cannot hold, becomes U+FFFD.
        mutating func parseString() throws -> String {
            at += 1
            var out: [UInt8] = []
            while true {
                guard at < bytes.count else { throw fail("an unterminated string") }
                let byte = bytes[at]
                if byte == UInt8(ascii: "\"") {
                    at += 1
                    return String(decoding: out, as: UTF8.self)
                }
                if byte < 0x20 { throw fail("a control character inside a string") }
                guard byte == UInt8(ascii: "\\") else {
                    out.append(byte)
                    at += 1
                    continue
                }
                guard at + 1 < bytes.count else { throw fail("an unterminated string") }
                let escape = bytes[at + 1]
                at += 2
                switch escape {
                case UInt8(ascii: "\""): out.append(0x22)
                case UInt8(ascii: "\\"): out.append(0x5C)
                case UInt8(ascii: "/"): out.append(0x2F)
                case UInt8(ascii: "b"): out.append(0x08)
                case UInt8(ascii: "f"): out.append(0x0C)
                case UInt8(ascii: "n"): out.append(0x0A)
                case UInt8(ascii: "r"): out.append(0x0D)
                case UInt8(ascii: "t"): out.append(0x09)
                case UInt8(ascii: "u"):
                    var code = try hex4()
                    if (0xD800...0xDBFF).contains(code), at + 1 < bytes.count,
                       bytes[at] == UInt8(ascii: "\\"), bytes[at + 1] == UInt8(ascii: "u") {
                        let saved = at
                        at += 2
                        let low = try hex4()
                        if (0xDC00...0xDFFF).contains(low) {
                            code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
                        } else {
                            at = saved
                        }
                    }
                    let scalar = Unicode.Scalar(code) ?? "\u{FFFD}"
                    out.append(contentsOf: Array(String(Character(scalar)).utf8))
                default:
                    at -= 1
                    throw fail("an escape JSON does not have")
                }
            }
        }

        mutating func hex4() throws -> UInt32 {
            guard at + 4 <= bytes.count else { throw fail("a \\u escape needs four hex digits") }
            var value: UInt32 = 0
            for byte in bytes[at..<(at + 4)] {
                let digit: UInt32
                switch byte {
                case UInt8(ascii: "0")...UInt8(ascii: "9"): digit = UInt32(byte - UInt8(ascii: "0"))
                case UInt8(ascii: "a")...UInt8(ascii: "f"): digit = UInt32(byte - UInt8(ascii: "a") + 10)
                case UInt8(ascii: "A")...UInt8(ascii: "F"): digit = UInt32(byte - UInt8(ascii: "A") + 10)
                default: throw fail("a \\u escape needs four hex digits")
                }
                value = value * 16 + digit
            }
            at += 4
            return value
        }

        /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, kept as the text it is.
        mutating func parseNumber() throws -> String {
            let start = at
            func isDigit(_ index: Int) -> Bool { index < bytes.count && (0x30...0x39).contains(bytes[index]) }
            if at < bytes.count, bytes[at] == UInt8(ascii: "-") { at += 1 }
            guard isDigit(at) else {
                at = start
                throw fail("a JSON value was expected")
            }
            if bytes[at] == UInt8(ascii: "0") { at += 1 } else { while isDigit(at) { at += 1 } }
            if at < bytes.count, bytes[at] == UInt8(ascii: "."), isDigit(at + 1) {
                at += 1
                while isDigit(at) { at += 1 }
            }
            if at < bytes.count, bytes[at] == UInt8(ascii: "e") || bytes[at] == UInt8(ascii: "E") {
                var next = at + 1
                if next < bytes.count, bytes[next] == UInt8(ascii: "+") || bytes[next] == UInt8(ascii: "-") { next += 1 }
                if isDigit(next) {
                    at = next
                    while isDigit(at) { at += 1 }
                }
            }
            return String(decoding: bytes[start..<at], as: UTF8.self)
        }
    }
}

/// Adding a small shift to an exponent of any size, so a number key stays exact however many digits
/// the exponent was written with.
private enum BigDecimal {
    static func adding(_ shift: Int, to exponent: String) -> String {
        let negative = exponent.hasPrefix("-")
        var digits = Array(exponent.drop { $0 == "-" || $0 == "+" }.drop { $0 == "0" }.utf8).map { Int($0) - 48 }
        if digits.count < 18 {
            let magnitude = digits.reduce(0) { $0 * 10 + $1 }
            return String((negative ? -magnitude : magnitude) + shift)
        }
        // The exponent is at least 10^17, and a shift is bounded by the length of a document, so the
        // sum keeps the exponent's sign and only its magnitude moves.
        let grows = (shift >= 0) != negative
        var carry = shift.magnitude
        var index = digits.count - 1
        while carry > 0, index >= 0 {
            let digit = UInt(digits[index])
            if grows {
                let sum = digit + carry % 10
                digits[index] = Int(sum % 10)
                carry = carry / 10 + sum / 10
            } else {
                let take = carry % 10
                carry /= 10
                if digit >= take {
                    digits[index] = Int(digit - take)
                } else {
                    digits[index] = Int(digit + 10 - take)
                    carry += 1
                }
            }
            index -= 1
        }
        if carry > 0 { digits.insert(contentsOf: String(carry).utf8.map { Int($0) - 48 }, at: 0) }
        let text = String(digits.drop { $0 == 0 }.map { Character(String($0)) })
        return (negative ? "-" : "") + text
    }
}
