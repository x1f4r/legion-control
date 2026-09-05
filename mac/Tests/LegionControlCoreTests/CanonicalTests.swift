import Foundation
import Testing
@testable import LegionControlCore

/// The canonical form of a controller document, checked against the vectors the whole fleet agrees
/// on.
///
/// This is the one place where being one byte out means every device disagrees about every document
/// forever, so it is checked against `contract/hash-vectors.json` rather than against this app's own
/// idea of the rule.
struct CanonicalTests {

    struct Vectors: Decodable {
        var vectors: [Vector]

        struct Vector: Decodable {
            var name: String
            var purpose: String
            var inputBase64: String
            var canonicalBase64: String?
            var sha256: String?
            var accepted: Bool?
            var reject: String?
        }
    }

    static func loadVectors() throws -> Vectors? {
        let url = repositoryRoot.appending(path: "contract/hash-vectors.json")
        guard let data = try? Data(contentsOf: url) else { return nil }
        return try JSONDecoder().decode(Vectors.self, from: data)
    }

    /// The repository this test is running out of, found by walking up from the source file.
    static var repositoryRoot: URL {
        var url = URL(fileURLWithPath: #filePath)
        // .../mac/Tests/LegionControlCoreTests/CanonicalTests.swift
        for _ in 0..<4 { url = url.deletingLastPathComponent() }
        return url
    }

    @Test("every shared hash vector produces the same canonical bytes and hash")
    func sharedVectors() throws {
        guard let vectors = try Self.loadVectors() else {
            // This shared file may be absent from a source archive. A silently skipped check would
            // be worse than either outcome, so the suite says so.
            Issue.record("contract/hash-vectors.json is missing, so the shared canonicalisation was not checked")
            return
        }
        #expect(!vectors.vectors.isEmpty)

        for vector in vectors.vectors {
            let input = try #require(Data(base64Encoded: vector.inputBase64), "\(vector.name): bad base64")

            guard let expectedHash = vector.sha256 else {
                // A vector with no hash is one that has no canonical form: invalid UTF-8. It has to
                // be refused rather than hashed, because a stable hash for something nobody can read
                // would make two sides agree forever about rubbish.
                #expect(throws: Canonical.NotUTF8.self, "\(vector.name) should have no canonical form") {
                    _ = try Canonical.bytes(input)
                }
                continue
            }

            let bytes = try Canonical.bytes(input)
            if let expectedCanonical = vector.canonicalBase64 {
                let expected = try #require(Data(base64Encoded: expectedCanonical))
                #expect(bytes == expected, "\(vector.name): \(vector.purpose)")
            }
            #expect(Canonical.sha256(bytes) == expectedHash, "\(vector.name): \(vector.purpose)")
        }
    }

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
}
