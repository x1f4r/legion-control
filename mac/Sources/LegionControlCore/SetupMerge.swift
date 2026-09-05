import Foundation

/// Putting two diverged setups back together.
///
/// When both sides changed the document since they last agreed, neither may simply overwrite the
/// other. What this does is compare them entry by entry — one machine, one site, one endpoint, one
/// system, the app update settings — and, where a common ancestor is available, work out which side
/// actually changed each entry. Entries only one side touched are taken from that side without
/// asking. Entries both sides touched are the only ones a person has to decide about, and they are
/// shown with both versions.
///
/// Everything works on the raw JSON rather than on the typed model, and that is deliberate: a
/// document may legitimately carry keys this build has never heard of — a newer client wrote them,
/// or only the phone reads them — and round-tripping through `ControllerConfig` would silently
/// delete every one of them.
enum SetupMerge {

    /// One thing the two documents disagree about.
    struct Difference: Sendable, Equatable, Identifiable {
        /// Where it is, as a path a person can read: `machines.legion.wake`, `sites.home-a`.
        var path: [String]
        var kind: Kind
        /// What is on this device, as pretty JSON, or nil when this side does not have it.
        var mine: String?
        /// What the other side has, or nil when it does not have it.
        var theirs: String?
        /// What the common ancestor had, when there is one.
        var base: String?

        var id: String { path.joined(separator: ".") }
        var label: String { path.joined(separator: " › ") }

        enum Kind: Sendable, Equatable {
            /// Only this device changed it. Taken from here, no question asked.
            case onlyMineChanged
            /// Only the other side changed it. Taken from there, no question asked.
            case onlyTheirsChanged
            /// Both changed it, differently. The one case a person has to settle.
            case bothChanged
            /// No common ancestor was available, so which side changed it cannot be worked out.
            case unknownBase
        }

        /// What a merge does with it unless the user says otherwise.
        var defaultChoice: Choice {
            switch kind {
            case .onlyMineChanged: .mine
            case .onlyTheirsChanged: .theirs
            // With no evidence about who moved, the copy the machines are already carrying wins.
            // Taking "mine" by default would quietly undo an edit made on another device.
            case .bothChanged, .unknownBase: .theirs
            }
        }

        /// Whether it needs a person. Everything else is settled by the evidence.
        var needsDecision: Bool { kind == .bothChanged || kind == .unknownBase }
    }

    enum Choice: String, Sendable, Equatable, CaseIterable {
        case mine
        case theirs
    }

    struct MergeFailure: Error, Sendable, Equatable {
        var message: String
        init(_ message: String) { self.message = message }
    }

    /// Compare two documents, with the common ancestor when one is to hand.
    static func differences(mine: Data, theirs: Data, base: Data?) throws -> [Difference] {
        let a = try object(mine, what: "this device's setup")
        let b = try object(theirs, what: "the machine's setup")
        let c = base.flatMap { try? object($0, what: "the common ancestor") }

        var out: [Difference] = []
        for path in entryPaths(a: a, b: b, base: c) {
            let mineValue = value(at: path, in: a)
            let theirsValue = value(at: path, in: b)
            if equal(mineValue, theirsValue) { continue }
            let baseValue = c.flatMap { value(at: path, in: $0) }

            let kind: Difference.Kind
            if c == nil {
                kind = .unknownBase
            } else if equal(baseValue, mineValue) {
                kind = .onlyTheirsChanged
            } else if equal(baseValue, theirsValue) {
                kind = .onlyMineChanged
            } else {
                kind = .bothChanged
            }

            out.append(Difference(
                path: path,
                kind: kind,
                mine: mineValue.map(pretty),
                theirs: theirsValue.map(pretty),
                base: baseValue.map(pretty)
            ))
        }
        return out.sorted { $0.id < $1.id }
    }

    /// Build the merged document.
    ///
    /// `choices` is keyed by difference id and only has to cover the ones that need a decision;
    /// anything missing takes the default, which is the side that changed it.
    static func merge(
        mine: Data,
        theirs: Data,
        base: Data?,
        differences: [Difference],
        choices: [String: Choice],
        identity: ControllerIdentity
    ) throws -> Data {
        var result = try object(mine, what: "this device's setup")
        let other = try object(theirs, what: "the machine's setup")

        for difference in differences {
            let choice = choices[difference.id] ?? difference.defaultChoice
            guard choice == .theirs else { continue }
            let theirsValue = value(at: difference.path, in: other)
            result = setting(difference.path, to: theirsValue, in: result)
        }

        // The identity is always this device's own: it descends from both sides, so both machines
        // accept it as a fast-forward and neither branch is lost.
        result["controller"] = encode(identity)

        // Written the way every device writes a document, so the merge has the same bytes and the
        // same hash wherever it was made.
        return try JSONText.canonicalDocument(result)
    }

    /// The identity block as a plain dictionary, so unknown keys elsewhere in the document survive.
    static func encode(_ identity: ControllerIdentity) -> [String: Any] {
        var block: [String: Any] = [:]
        if let id = identity.id { block["id"] = id }
        if let name = identity.name { block["name"] = name }
        block["revision"] = identity.revisionNumber
        if let updatedAt = identity.updatedAt { block["updatedAt"] = updatedAt }
        if let source = identity.source { block["source"] = source }
        if let device = identity.device { block["device"] = device }
        block["lineage"] = identity.ancestors
        return block
    }

    // MARK: - Walking the document

    /// The paths a merge works at.
    ///
    /// Not every key and not the whole document: one machine, one site, one endpoint, one system,
    /// the wake block, the app update settings. Fine enough that renaming a machine on one device
    /// and adding an endpoint to another one elsewhere merges without a question, coarse enough that
    /// a person is never asked about a port number in isolation.
    static func entryPaths(a: [String: Any], b: [String: Any], base: [String: Any]?) -> [[String]] {
        var paths: [[String]] = []

        for key in union(a, b, base, at: "sites").sorted() {
            paths.append(["sites", key])
        }

        for machineId in union(a, b, base, at: "machines").sorted() {
            let inA = element("machines", machineId, a)
            let inB = element("machines", machineId, b)
            let inBase = base.flatMap { element("machines", machineId, $0) }
            // A machine that exists on only one side is one decision, not a dozen.
            guard inA != nil, inB != nil else {
                paths.append(["machines", machineId])
                continue
            }
            // The machine's own scalar fields, minus the collections handled below.
            paths.append(["machines", machineId, "identity"])
            paths.append(["machines", machineId, "wake"])
            for endpoint in union(inA, inB, inBase, at: "endpoints").sorted() {
                paths.append(["machines", machineId, "endpoints", endpoint])
            }
            for system in union(inA, inB, inBase, at: "systems").sorted() {
                paths.append(["machines", machineId, "systems", system])
            }
        }

        paths.append(["appUpdates"])
        return paths
    }

    /// The ids present in an array-of-objects field, across all three documents.
    private static func union(_ a: [String: Any]?, _ b: [String: Any]?, _ base: [String: Any]?,
                              at key: String) -> Set<String> {
        var ids = Set<String>()
        for document in [a, b, base] {
            guard let list = document?[key] as? [[String: Any]] else { continue }
            for entry in list {
                if let id = entry["id"] as? String { ids.insert(id) }
            }
        }
        return ids
    }

    private static func element(_ key: String, _ id: String, _ document: [String: Any]) -> [String: Any]? {
        (document[key] as? [[String: Any]])?.first { $0["id"] as? String == id }
    }

    /// What is at a path, or nil when nothing is.
    static func value(at path: [String], in document: [String: Any]) -> Any? {
        switch path.count {
        case 1:
            return document[path[0]]
        case 2:
            return element(path[0], path[1], document)
        case 3 where path[2] == "identity":
            // Everything about the machine except the collections, which have their own paths.
            guard var machine = element(path[0], path[1], document) else { return nil }
            for key in ["endpoints", "systems", "wake"] { machine.removeValue(forKey: key) }
            return machine
        case 3:
            return element(path[0], path[1], document)?[path[2]]
        case 4:
            guard let machine = element(path[0], path[1], document) else { return nil }
            return (machine[path[2]] as? [[String: Any]])?.first { $0["id"] as? String == path[3] }
        default:
            return nil
        }
    }

    /// The document with one path replaced. Removing is what a nil value means.
    static func setting(_ path: [String], to newValue: Any?, in document: [String: Any]) -> [String: Any] {
        var result = document
        switch path.count {
        case 1:
            if let newValue { result[path[0]] = newValue } else { result.removeValue(forKey: path[0]) }
        case 2:
            var list = (result[path[0]] as? [[String: Any]]) ?? []
            list.removeAll { $0["id"] as? String == path[1] }
            if let entry = newValue as? [String: Any] { list.append(entry) }
            result[path[0]] = list
        case 3 where path[2] == "identity":
            var list = (result[path[0]] as? [[String: Any]]) ?? []
            guard let index = list.firstIndex(where: { $0["id"] as? String == path[1] }) else { return result }
            let keep = list[index]
            var merged = (newValue as? [String: Any]) ?? [:]
            for key in ["endpoints", "systems", "wake"] where keep[key] != nil { merged[key] = keep[key] }
            list[index] = merged
            result[path[0]] = list
        case 3:
            var list = (result[path[0]] as? [[String: Any]]) ?? []
            guard let index = list.firstIndex(where: { $0["id"] as? String == path[1] }) else { return result }
            var machine = list[index]
            if let newValue { machine[path[2]] = newValue } else { machine.removeValue(forKey: path[2]) }
            list[index] = machine
            result[path[0]] = list
        case 4:
            var list = (result[path[0]] as? [[String: Any]]) ?? []
            guard let index = list.firstIndex(where: { $0["id"] as? String == path[1] }) else { return result }
            var machine = list[index]
            var inner = (machine[path[2]] as? [[String: Any]]) ?? []
            inner.removeAll { $0["id"] as? String == path[3] }
            if let entry = newValue as? [String: Any] { inner.append(entry) }
            machine[path[2]] = inner
            list[index] = machine
            result[path[0]] = list
        default:
            break
        }
        return result
    }

    // MARK: - Comparing and printing

    /// Two JSON values, compared by their canonical serialisation. Key order and number formatting
    /// therefore cannot make two identical entries look different.
    static func equal(_ a: Any?, _ b: Any?) -> Bool {
        switch (a, b) {
        case (nil, nil): return true
        case (nil, _), (_, nil): return false
        default: return canonicalString(a!) == canonicalString(b!)
        }
    }

    private static func canonicalString(_ value: Any) -> String {
        let wrapped = ["v": value]
        guard let data = try? JSONSerialization.data(withJSONObject: wrapped, options: [.sortedKeys]) else {
            return String(describing: value)
        }
        return String(decoding: data, as: UTF8.self)
    }

    static func pretty(_ value: Any) -> String {
        let wrapped = ["v": value]
        guard let data = try? JSONSerialization.data(withJSONObject: wrapped,
                                                     options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]),
              let text = String(data: data, encoding: .utf8)
        else { return String(describing: value) }
        // Unwrap the one-key object the serialiser needed to accept a bare value.
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        guard lines.count > 2 else { return text }
        return lines.dropFirst().dropLast()
            .map { line -> String in
                var trimmed = String(line)
                if trimmed.hasPrefix("  ") { trimmed.removeFirst(2) }
                return trimmed
            }
            .joined(separator: "\n")
            .replacingOccurrences(of: "\"v\" : ", with: "")
    }

    private static func object(_ data: Data, what: String) throws -> [String: Any] {
        guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
            throw MergeFailure("\(what) is not a JSON object, so the two cannot be compared.")
        }
        return object
    }
}
