import Foundation

/// The contract corpus every client shares: fixtures and conformance vectors under `contract/`.
///
/// Found from this file's own path, so it is the corpus of the checkout being tested. A missing file
/// is a failure, never a skip: a suite that quietly checks nothing when the corpus is absent is how
/// two clients came to disagree while both suites stayed green.
enum ContractCorpus {
    struct Missing: Error, CustomStringConvertible {
        var path: String
        var description: String { "contract/\(path) is missing; the Mac tests need the shared contract corpus" }
    }

    /// `.../contract`, found by walking up from `mac/Tests/LegionControlCoreTests/ContractCorpus.swift`.
    static var root: URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 { url = url.deletingLastPathComponent() }
        return url.appending(path: "contract")
    }

    /// The bytes of a file in the corpus, by its path relative to `contract/`.
    static func data(_ relative: String) throws -> Data {
        let url = root.appending(path: relative)
        guard let data = try? Data(contentsOf: url) else { throw Missing(path: relative) }
        return data
    }

    static func fixtureIndex() throws -> VectorJSON {
        try VectorJSON.parse(data("fixtures/index.json"))
    }

    static func vectorIndex() throws -> VectorIndex {
        try JSONDecoder().decode(VectorIndex.self, from: data("vectors/index.json"))
    }

    /// A family's file, as the index names it.
    static func family(_ name: String) throws -> VectorJSON {
        let index = try vectorIndex()
        guard let entry = index.families.first(where: { $0.family == name }) else { throw Missing(path: "vectors/\(name).json") }
        return try VectorJSON.parse(data(entry.file))
    }

    /// `contract/vectors/index.json`: which families exist, which groups each has, and how many
    /// cases each consumer runs.
    struct VectorIndex: Decodable {
        var contract: Int
        var families: [Family]

        struct Family: Decodable {
            var family: String
            var file: String
            var layout: String
            var groups: [Group]
        }

        struct Group: Decodable {
            var group: String
            var cases: Int
            var consumers: [String: Int]
        }
    }
}
