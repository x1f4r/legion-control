import Foundation
import Testing
@testable import LegionControlCore

/// The canonical form of a controller document.
///
/// This is the one place where being one byte out means every device disagrees about every document
/// forever. The shared `hash` vectors check it case by case (ContractVectorTests); these name the
/// rules a person would get wrong.
struct CanonicalTests {

    @Test("the rule is the fixed ASCII set, not the language's own trim")
    func trimsOnlyTheFixedSet() throws {
        // U+00A0 is whitespace to Foundation and to JavaScript, and is not in the contract's set.
        // Trimming it here and not there would give the same document two different hashes.
        let withNBSP = Data("\u{00A0}{}\u{00A0}".utf8)
        let canonical = try Canonical.bytes(withNBSP)
        #expect(String(decoding: canonical, as: UTF8.self) == "\u{00A0}{}\u{00A0}\n")

        let withTabs = Data("\t\n {}\r\n \t".utf8)
        #expect(String(decoding: try Canonical.bytes(withTabs), as: UTF8.self) == "{}\n")
    }

    @Test("a leading byte order mark goes, one further in stays")
    func byteOrderMark() throws {
        let leading = Data("\u{FEFF}{\"a\":1}".utf8)
        #expect(String(decoding: try Canonical.bytes(leading), as: UTF8.self) == "{\"a\":1}\n")

        let inside = Data("{\"a\":\"\u{FEFF}\"}".utf8)
        #expect(String(decoding: try Canonical.bytes(inside), as: UTF8.self) == "{\"a\":\"\u{FEFF}\"}\n")
    }

    @Test("CRLF and lone CR both become one LF")
    func lineEndings() throws {
        let crlf = Data("{\r\n  \"a\": 1\r\n}".utf8)
        let cr = Data("{\r  \"a\": 1\r}".utf8)
        let lf = Data("{\n  \"a\": 1\n}".utf8)
        #expect(try Canonical.hash(crlf) == (try Canonical.hash(lf)))
        #expect(try Canonical.hash(cr) == (try Canonical.hash(lf)))
    }

    @Test("invalid UTF-8 is refused rather than hashed")
    func invalidUTF8() {
        let bad = Data([0x7B, 0xFF, 0xFE, 0x7D])
        #expect(throws: Canonical.NotUTF8.self) { _ = try Canonical.bytes(bad) }
    }

    @Test("a document without a final newline hashes the same as one with it")
    func trailingNewlineDoesNotMatter() throws {
        // The exact defect from the review: the Mac hashed the raw file and the agent hashed what it
        // stored, so a file saved without a final newline never agreed with anybody and the Mac
        // pushed the same document every ten minutes forever.
        let without = Data("{\"version\":1}".utf8)
        let with = Data("{\"version\":1}\n".utf8)
        let many = Data("{\"version\":1}\n\n\n\n".utf8)
        let hash = try Canonical.hash(without)
        #expect(try Canonical.hash(with) == hash)
        #expect(try Canonical.hash(many) == hash)
    }

    @Test("two byte order marks: one is stripped, the second is content, and the document is refused")
    func doubleByteOrderMark() throws {
        // C13: Foundation's UTF-8 decoder drops a leading mark without saying so, and so does
        // JSONSerialization, so a document behind two of them lost one in each and was accepted here
        // while every other implementation refuses it.
        let bom: [UInt8] = [0xEF, 0xBB, 0xBF]
        let document = Data(#"{"version":1,"machines":[]}"#.utf8)
        let twice = Data(bom + bom) + document
        #expect(try Canonical.bytes(twice) == Data(bom) + document + Data("\n".utf8))
        #expect(Canonical.rejection(twice) == .notAJSONObject)
        #expect(Canonical.rejection(Data(bom) + document) == nil)
    }

    @Test("what section 12 refuses, in its order")
    func rejectionOrder() {
        #expect(Canonical.rejection(Data(repeating: 0x20, count: Canonical.maxDocumentBytes + 1)) == .tooLarge)
        #expect(Canonical.rejection(Data([0x7B, 0xFF, 0x7D])) == .invalidUTF8)
        #expect(Canonical.rejection(Data(" \r\n\t".utf8)) == .empty)
        #expect(Canonical.rejection(Data("[1]".utf8)) == .notAJSONObject)
        #expect(Canonical.rejection(Data(#"{"version":"1","machines":[]}"#.utf8)) == .notAJSONObject)
        // A repeated name is section 9.7's to refuse, not section 12's.
        #expect(Canonical.rejection(Data(#"{"version":1,"version":1,"machines":[]}"#.utf8)) == nil)
    }
}
