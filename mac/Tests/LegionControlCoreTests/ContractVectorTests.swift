import Foundation
import Testing
@testable import LegionControlCore

/// The shared conformance vectors, run through this app's own code.
///
/// Each group the index lists the Mac as a consumer of goes through exactly one adapter, which turns
/// a case's input into what this app does with it, and every case has to agree with the reference's
/// `expect`. The number of cases run has to be the number the index gives the Mac, so a group that
/// shrank, grew or was skipped fails here rather than passing quietly.
struct ContractVectorTests {
    static let consumer = "mac"

    /// The families this suite runs. A family joins when this app has a port of its reference to
    /// run, and the contract has nine.
    static let families = ["identifiers"]

    typealias Adapter = @Sendable (VectorJSON) throws -> VectorJSON

    struct BadInput: Error, CustomStringConvertible {
        var description: String
    }

    static func string(_ input: VectorJSON, _ key: String) throws -> String {
        guard let value = input[key]?.string else { throw BadInput(description: "\(key) is not a string") }
        return value
    }

    static func int(_ input: VectorJSON, _ key: String) throws -> Int {
        guard let value = input[key]?.int else { throw BadInput(description: "\(key) is not an integer") }
        return value
    }

    static func valid(_ check: @escaping @Sendable (String) -> Bool) -> Adapter {
        { input in ["valid": .bool(check(try string(input, "value")))] }
    }

    static let adapters: [String: Adapter] = [
        // Section 1, rule 5: the identifier grammars, whole input, ASCII only.
        "identifiers/token": valid(AgentToken.isValid),
        "identifiers/configuredId": valid(AgentToken.isValidID),
        "identifiers/setupId": valid(AgentToken.isValidSetupID),
        "identifiers/opId": valid(AgentToken.isValidOperationId),
        "identifiers/duration": valid(AgentToken.isValidDuration),
        "identifiers/clockTime": valid(AgentToken.isValidClockTime),
        "identifiers/sha256": valid(AgentToken.isValidHash),
    ]

    @Test("every case of a family the Mac consumes agrees with the reference", arguments: families)
    func family(_ name: String) throws {
        let index = try ContractCorpus.vectorIndex()
        #expect(index.contract == 3)
        let entry = try #require(index.families.first { $0.family == name }, "the index has no family \(name)")
        let file = try ContractCorpus.family(name)

        for group in entry.groups {
            guard let expected = group.consumers[Self.consumer] else { continue }
            let key = "\(name)/\(group.group)"
            let adapter = try #require(Self.adapters[key], "no adapter for \(key)")
            let cases = try #require(file["groups"]?[group.group]?.array, "\(key) has no cases in its file")
            var ran = 0
            for vector in cases where vector["consumers"]?.array?.contains(.string(Self.consumer)) == true {
                let label = "\(key)/\(vector["name"]?.string ?? "?")"
                let input = try #require(vector["input"], "\(label) has no input")
                let expect = try #require(vector["expect"], "\(label) has no expect")
                do {
                    let differences = expect.differences(from: try adapter(input))
                    #expect(differences.isEmpty, "\(label): \(differences.joined(separator: "; "))")
                } catch {
                    Issue.record("\(label): \(error)")
                }
                ran += 1
            }
            #expect(ran == expected, "\(key): ran \(ran) cases, the index gives the Mac \(expected)")
        }
    }

    @Test("every group the Mac consumes in these families has exactly one adapter")
    func everyConsumedGroupHasOneAdapter() throws {
        let index = try ContractCorpus.vectorIndex()
        let consumed = Set(index.families.filter { Self.families.contains($0.family) }.flatMap { family in
            family.groups.filter { $0.consumers[Self.consumer] != nil }.map { "\(family.family)/\($0.group)" }
        })
        #expect(Set(Self.families).isSubset(of: index.families.map(\.family)))
        #expect(Set(Self.adapters.keys) == consumed)
    }

    @Test("the comparison is the index's: key order free, strings by code point, Text fields parsed")
    func comparison() {
        let expected: VectorJSON = ["a": 1, "b": ["x", nil, true], "docText": #"{"p":1,"q":[2]}"#]
        #expect(expected.differences(from: ["b": ["x", nil, true], "a": 1, "docText": #"{"q":[2],"p":1}"#]).isEmpty)
        // A composed and a decomposed é are one string to Swift and two to the contract.
        #expect(!VectorJSON.string("\u{E9}").differences(from: .string("e\u{301}")).isEmpty)
        #expect(!expected.differences(from: ["a": 1, "b": ["x", nil, false], "docText": #"{"p":1,"q":[2]}"#]).isEmpty)
        #expect(!expected.differences(from: ["a": 1, "b": ["x", nil, true]]).isEmpty)
        #expect(!VectorJSON.bool(true).differences(from: .number(1)).isEmpty)
    }
}
