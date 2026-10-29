import Foundation

/// Putting two diverged setups back together: section 9.8 of the contract.
///
/// When both sides changed the document since they last agreed, neither may simply overwrite the
/// other. This compares them entry by entry — one machine, one site, one endpoint, one system, a
/// wake block, an unknown key — and, where a common ancestor is available, works out which side
/// changed each entry. Entries only one side touched are taken from that side without asking.
/// Entries both sides touched, a deletion against an edit included, are the ones a person decides,
/// and without an ancestor every difference is.
///
/// The three clients used to merge three ways, and each lost something different: a deletion beat
/// an edit without a question, endpoints without an id vanished, unknown keys were dropped, merged
/// entries moved to the end of their arrays and `22.0` became `22` (C6). This is a port of
/// `contract/tools/setup-sync.mjs`, held to `contract/vectors/setup-merge.json`, and it works on
/// `SetupJSON`, which keeps member order and number spelling, so what a merge does not replace it
/// writes back exactly as it found it.
enum SetupMerge {

    /// Which side changed an entry, as far as the evidence says.
    enum Kind: String, Sendable, Equatable {
        /// Only this device changed it. Taken from here, no question asked.
        case mine
        /// Only the other side changed it. Taken from there, no question asked.
        case theirs
        /// Both changed it, differently, or one deleted what the other edited.
        case both
        /// No common ancestor was available, so which side changed it cannot be worked out.
        case noBase
    }

    enum Choice: String, Sendable, Equatable, CaseIterable {
        case mine
        case theirs
    }

    /// One entry the two documents disagree about.
    struct Difference: Sendable, Equatable, Identifiable {
        /// The entry's key in the contract's spelling: `machines[pi].fields`, `sites[home]`.
        var entry: String
        var kind: Kind
        /// What is on this device, written out, or nil when this side does not have it.
        var mine: String?
        /// What the other side has, or nil when it does not have it.
        var theirs: String?
        /// What the common ancestor had, when there is one and it had the entry.
        var base: String?

        var id: String { entry }

        /// The key as a person reads it: `machines[pi] › fields`.
        var label: String {
            entry.hasPrefix("\"") ? entry : entry
                .replacingOccurrences(of: "].", with: "] › ")
                .replacingOccurrences(of: "controller.", with: "controller › ")
        }

        /// What a merge does with it unless the person says otherwise. With no evidence about who
        /// moved, the copy the machines are already carrying wins: taking this device's by default
        /// would quietly undo an edit made on another one.
        var defaultChoice: Choice { kind == .mine ? .mine : .theirs }

        /// Whether it needs a person. Everything else is settled by the evidence.
        var needsDecision: Bool { kind == .both || kind == .noBase }
    }

    /// Who writes the merged revision.
    struct Writer: Sendable, Equatable {
        var source: String
        var device: String
        /// The ISO 8601 time it is written.
        var now: String
    }

    struct Outcome: Sendable, Equatable {
        var differences: [Difference]
        /// The merged document, or nil when section 9.7 refuses it.
        var result: Data?
        /// The first error code the refused result breaks, in code point order.
        var error: String?
    }

    /// A side that cannot be merged at all, saying why.
    struct MergeFailure: Error, Sendable, Equatable {
        var message: String
        init(_ message: String) { self.message = message }
    }

    // MARK: - Merging

    /// The entries where the two documents differ, in code point order of their keys.
    static func differences(base: Data?, mine: Data, theirs: Data) throws -> [Difference] {
        let (mineTree, theirsTree, baseTree) = try sides(base: base, mine: mine, theirs: theirs)
        return differing(mine: mineTree, theirs: theirsTree, base: baseTree).map(\.difference)
    }

    /// Merge two documents.
    ///
    /// `choices` maps an entry key to a side. An entry it does not name takes the side its kind says:
    /// mine for a `mine` difference and theirs for every other, so `both` and `noBase` are preselected
    /// theirs. Keep mine is every entry answered mine.
    ///
    /// The result starts from mine. Every entry taken from theirs is removed where theirs lacks it and
    /// replaced in place where both have it; then theirs' arrays are walked to insert what theirs
    /// adds, each after its predecessor in theirs. Member order is mine's with new members appended,
    /// and nothing that is not replaced is written differently. The identity is mine's id, one more
    /// than the higher revision, both heads and both histories in the lineage, and the writer's
    /// `updatedAt`, `source` and `device`; every other `controller` key is an entry like any other.
    ///
    /// The result is held to section 9.7 under peer intake, and one it refuses is not returned: the
    /// person is told the rule instead, the one their choices broke or the one mine broke already
    /// and the choices kept.
    static func merge(base: Data?, mine: Data, theirs: Data, mineHash: String, theirsHash: String,
                      writer: Writer, choices: [String: Choice] = [:]) throws -> Outcome {
        let (mineTree, theirsTree, baseTree) = try sides(base: base, mine: mine, theirs: theirs)
        guard mineTree["controller"]?["id"]?.stringValue != nil else {
            throw MergeFailure("This device's setup has no identity yet, so it cannot be merged. Give it one first.")
        }

        let found = differing(mine: mineTree, theirs: theirsTree, base: baseTree)
        let taken = Set(found.filter { (choices[$0.key] ?? $0.difference.defaultChoice) == .theirs }.map(\.key))

        var result = mineTree
        for entry in found where taken.contains(entry.key) { takeTheirs(entry, into: &result) }

        walkTheirs(&result, "sites", theirsTree["sites"], taken: taken) { "sites[\($0 ?? "null")]" }
        walkTheirs(&result, "machines", theirsTree["machines"], taken: taken) { "machines[\($0 ?? "null")]" }
        let paired = pairedMachines(mineTree, theirsTree)
        for machine in theirsTree["machines"]?.objects ?? [] {
            guard let id = machine.id, paired.contains(id) else { continue }
            let at = "machines[\(id)]"
            result.modifyItem(in: "machines", id: id) { holder in
                walkTheirs(&holder, "systems", machine["systems"], taken: taken) { "\(at).systems[\($0 ?? "null")]" }
                walkTheirs(&holder, "endpoints", machine["endpoints"], taken: taken) { id in
                    id.map { "\(at).endpoints[\($0)]" } ?? "\(at).endpoints[]"
                }
            }
        }

        // The identity. Mine's id stays where it is; the rest is written in place, or appended in
        // this order when mine's block lacks it.
        func revision(_ tree: SetupJSON) -> Int { tree["controller"]?["revision"]?.integerValue ?? 0 }
        let lineage = mergedLineage(mineHash: mineHash, theirsHash: theirsHash,
                                    mineLineage: lineageOf(mineTree), theirsLineage: lineageOf(theirsTree))
        if var block = result["controller"] {
            block["revision"] = .number(String(max(revision(mineTree), revision(theirsTree)) + 1))
            block["updatedAt"] = .string(writer.now)
            block["source"] = .string(writer.source)
            block["device"] = .string(writer.device)
            block["lineage"] = .array(lineage.map(SetupJSON.string))
            result["controller"] = block
        }

        let text = result.documentData
        let verdict = DocumentAcceptance.check(text, intake: .peer)
        let codes = Set(verdict.errors.map(\.code)).sorted(by: SetupJSON.precedes)
        return Outcome(differences: found.map(\.difference), result: verdict.valid ? text : nil,
                       error: verdict.valid ? nil : codes.first)
    }

    /// Why this device's document cannot be merged, or nil when it can.
    ///
    /// Mine is the applied document, which applied intake keeps with its errors. One that breaks only
    /// how its parts refer to each other still merges: the result is checked like any other, and
    /// taking theirs' side of the entry at fault repairs it. Any other error leaves no single answer
    /// to what its entries are, so Merge and Keep mine wait until it is fixed in the editor. Take
    /// theirs does not.
    static func blocker(mine: Data) -> String? {
        let verdict = DocumentAcceptance.check(mine, intake: .applied)
        return verdict.errors.first { !DocumentAcceptance.mergeableErrors.contains($0.code) }?.message
    }

    // MARK: - Bases and lineage

    /// The base of a merge: the first hash of mine's lineage, newest first, that is also in theirs'
    /// lineage and whose kept bytes still hash to it, or nil. A head is never a base: a head is not
    /// in its own lineage, and a merge whose base was one side would only ever take the other side.
    static func commonBase(mineHash: String, theirsHash: String, mineLineage: [String], theirsLineage: [String],
                           kept: (String) -> Data?) -> String? {
        for hash in mineLineage where hash != mineHash && hash != theirsHash && theirsLineage.contains(hash) {
            if let bytes = kept(hash), (try? Canonical.hash(bytes)) == hash { return hash }
        }
        return nil
    }

    /// The lineage of a merge: mine's hash, theirs' hash, then the two lineages interleaved, mine
    /// first, each hash once, cut at 32. Both heads are in it, so both branches fast-forward onto
    /// the result.
    static func mergedLineage(mineHash: String, theirsHash: String, mineLineage: [String], theirsLineage: [String]) -> [String] {
        var lineage: [String] = []
        func add(_ hash: String) { if !lineage.contains(hash) { lineage.append(hash) } }
        add(mineHash)
        add(theirsHash)
        for index in 0..<max(mineLineage.count, theirsLineage.count) {
            if index < mineLineage.count { add(mineLineage[index]) }
            if index < theirsLineage.count { add(theirsLineage[index]) }
        }
        return Array(lineage.prefix(ControllerIdentity.lineageLimit))
    }

    private static func lineageOf(_ tree: SetupJSON) -> [String] {
        (tree["controller"]?["lineage"]?.items ?? []).compactMap(\.stringValue)
    }

    // MARK: - Entries

    /// The controller keys that are identity, which a merge writes itself.
    private static let identityKeys: Set<String> = ["id", "revision", "updatedAt", "source", "device", "lineage"]
    /// The top-level keys whose contents are entries of their own.
    private static let structureKeys: Set<String> = ["controller", "machines", "sites"]
    /// The members of a machine that are entries of their own when both sides have the machine.
    private static let machineParts: Set<String> = ["endpoints", "systems", "wake"]

    /// The entry key of a top-level member: its name, or the name in double quotes, nothing escaped,
    /// when it begins the way another entry's key does. Without the quotes, a member called
    /// `sites[home]` beside a site `home` would give one key to two entries. Ids are tokens, which
    /// hold no bracket, so the other keys never collide with each other.
    static func topLevelKey(_ name: String) -> String {
        ["\"", "controller.", "sites[", "machines["].contains(where: name.hasPrefix) ? "\"\(name)\"" : name
    }

    /// Where an entry lives in a document.
    private enum Place: Sendable, Equatable {
        case top(String)
        case controller(String)
        case site(String)
        case machine(String)
        case fields(machine: String)
        case wake(machine: String)
        case system(machine: String, id: String)
        case endpoint(machine: String, id: String)
        case idless(machine: String)
    }

    private struct Entry {
        var key: String
        var place: Place
        var mine: SetupJSON?
        var theirs: SetupJSON?
        var base: SetupJSON?
        var kind: Kind

        var difference: Difference {
            Difference(entry: key, kind: kind, mine: mine.map { $0.printed() }, theirs: theirs.map { $0.printed() },
                       base: base.map { $0.printed() })
        }
    }

    /// One document's entries by key. Keys are unique in every document a merge takes, because
    /// section 9.7 refuses two parts with one id and topLevelKey keeps unknown names apart, so a
    /// second entry under one key is a bug and stops the merge rather than lose one of the two.
    private static func entryMap(_ document: SetupJSON, paired: Set<String>) throws -> [String: (place: Place, value: SetupJSON)] {
        var map: [String: (place: Place, value: SetupJSON)] = [:]
        func add(_ key: String, _ place: Place, _ value: SetupJSON) throws {
            guard map[key] == nil else { throw MergeFailure("Two entries of one document are both called \(key).") }
            map[key] = (place, value)
        }
        for member in document.members ?? [] where !structureKeys.contains(member.name) {
            try add(topLevelKey(member.name), .top(member.name), member.value)
        }
        for member in document["controller"]?.members ?? [] where !identityKeys.contains(member.name) {
            try add("controller.\(member.name)", .controller(member.name), member.value)
        }
        for site in document["sites"]?.objects ?? [] {
            if let id = site.id { try add("sites[\(id)]", .site(id), site) }
        }
        for machine in document["machines"]?.objects ?? [] {
            guard let id = machine.id else { continue }
            let at = "machines[\(id)]"
            guard paired.contains(id) else {
                try add(at, .machine(id), machine)
                continue
            }
            try add("\(at).fields", .fields(machine: id),
                    .object((machine.members ?? []).filter { !machineParts.contains($0.name) }))
            if let wake = machine["wake"] { try add("\(at).wake", .wake(machine: id), wake) }
            for system in machine["systems"]?.objects ?? [] {
                if let systemId = system.id { try add("\(at).systems[\(systemId)]", .system(machine: id, id: systemId), system) }
            }
            // Endpoints without an id cannot be told apart, so together, in order, they are one
            // entry. None at all is absent.
            var idless: [SetupJSON] = []
            for endpoint in machine["endpoints"]?.objects ?? [] {
                if let endpointId = endpoint.id {
                    try add("\(at).endpoints[\(endpointId)]", .endpoint(machine: id, id: endpointId), endpoint)
                } else {
                    idless.append(endpoint)
                }
            }
            if !idless.isEmpty { try add("\(at).endpoints[]", .idless(machine: id), .array(idless)) }
        }
        return map
    }

    private static func machineIds(_ document: SetupJSON) -> Set<String> {
        Set((document["machines"]?.objects ?? []).compactMap(\.id))
    }

    /// The ids of the machines both documents have, which merge part by part.
    private static func pairedMachines(_ mine: SetupJSON, _ theirs: SetupJSON) -> Set<String> {
        machineIds(mine).intersection(machineIds(theirs))
    }

    /// Every entry where mine and theirs differ, in code point order of the key, with its kind.
    private static func differing(mine: SetupJSON, theirs: SetupJSON, base: SetupJSON?) -> [Entry] {
        let paired = pairedMachines(mine, theirs)
        // entryMap only throws for a document section 9.7 refuses, and sides() never lets one in.
        let ofMine = (try? entryMap(mine, paired: paired)) ?? [:]
        let ofTheirs = (try? entryMap(theirs, paired: paired)) ?? [:]
        let ofBase = base.flatMap { try? entryMap($0, paired: paired) } ?? [:]
        let keys = Set(ofMine.keys).union(ofTheirs.keys).sorted(by: SetupJSON.precedes)
        return keys.compactMap { key in
            let mineValue = ofMine[key]?.value
            let theirsValue = ofTheirs[key]?.value
            guard !SetupJSON.equal(mineValue, theirsValue), let place = (ofMine[key] ?? ofTheirs[key])?.place else { return nil }
            let baseValue = ofBase[key]?.value
            let kind: Kind
            if base == nil {
                kind = .noBase
            } else if SetupJSON.equal(theirsValue, baseValue) {
                kind = .mine
            } else if SetupJSON.equal(mineValue, baseValue) {
                kind = .theirs
            } else {
                kind = .both
            }
            return Entry(key: key, place: place, mine: mineValue, theirs: theirsValue, base: baseValue, kind: kind)
        }
    }

    /// Parse the three sides, each held to the intake section 9.7 gives it: theirs and the base to
    /// peer intake, as everything read from a machine or kept is, and mine to applied intake with
    /// only the errors a merge can carry.
    private static func sides(base: Data?, mine: Data, theirs: Data) throws -> (SetupJSON, SetupJSON, SetupJSON?) {
        if let problem = blocker(mine: mine) {
            throw MergeFailure("This device's setup has to be fixed in the editor before it can be merged: \(problem)")
        }
        let theirsVerdict = DocumentAcceptance.check(theirs, intake: .peer)
        if let problem = theirsVerdict.errors.first {
            throw MergeFailure("The other setup breaks a rule every device holds a setup to, so it cannot be merged: \(problem.message)")
        }
        if let base, let problem = DocumentAcceptance.check(base, intake: .peer).errors.first {
            throw MergeFailure("The common ancestor kept on this device breaks a rule, so it cannot be used: \(problem.message)")
        }
        func tree(_ data: Data) throws -> SetupJSON { try SetupJSON.parse(try Canonical.bytes(data)) }
        return (try tree(mine), try tree(theirs), try base.map(tree))
    }

    // MARK: - Building the result

    /// Take one entry from theirs: remove what theirs lacks, replace in place what both have.
    /// Insertions come later, in walkTheirs.
    private static func takeTheirs(_ entry: Entry, into result: inout SetupJSON) {
        let theirs = entry.theirs
        func replace(in name: String, id: String, of holder: inout SetupJSON) {
            holder.modifyItems(of: name) { items in
                guard let index = items.firstIndex(where: { $0.id == id }) else { return }
                if let theirs { items[index] = theirs } else { items.remove(at: index) }
            }
        }
        switch entry.place {
        case .top(let name):
            result[name] = theirs
        case .controller(let name):
            if var block = result["controller"] {
                block[name] = theirs
                result["controller"] = block
            }
        case .site(let id):
            replace(in: "sites", id: id, of: &result)
        case .machine(let id):
            replace(in: "machines", id: id, of: &result)
        case .fields(let machineId):
            // The machine's own members, other than its parts, become theirs: each where mine has
            // it, gone where theirs lacks it, appended where it is new.
            result.modifyItem(in: "machines", id: machineId) { machine in
                guard let members = machine.members else { return }
                var kept: [SetupJSON.Member] = []
                for member in members {
                    if machineParts.contains(member.name) {
                        kept.append(member)
                    } else if let value = theirs?[member.name] {
                        kept.append(SetupJSON.Member(name: member.name, value: value))
                    }
                }
                for member in theirs?.members ?? [] where !kept.contains(where: { SetupJSON.same($0.name, member.name) }) {
                    kept.append(member)
                }
                machine = .object(kept)
            }
        case .wake(let machineId):
            result.modifyItem(in: "machines", id: machineId) { $0["wake"] = theirs }
        case .system(let machineId, let id):
            result.modifyItem(in: "machines", id: machineId) { replace(in: "systems", id: id, of: &$0) }
        case .endpoint(let machineId, let id):
            result.modifyItem(in: "machines", id: machineId) { replace(in: "endpoints", id: id, of: &$0) }
        case .idless(let machineId):
            result.modifyItem(in: "machines", id: machineId) { machine in
                machine.modifyItems(of: "endpoints") { $0.removeAll { $0.id == nil } }
            }
        }
    }

    /// Walk one of theirs' arrays in order with a cursor. An element whose key is in the result moves
    /// the cursor to it; an element taken from theirs that is not in the result is inserted after the
    /// cursor, first when there is none, and becomes the cursor. Anything else is passed over. So an
    /// element theirs added lands after its predecessor in theirs, and everything else keeps mine's
    /// order: a reorder alone is never a difference.
    ///
    /// An endpoint without an id has no key of its own. When the id-less entry is taken from theirs,
    /// mine's id-less endpoints are already gone and each of theirs' is inserted; otherwise theirs'
    /// n-th id-less endpoint stands for the result's n-th and only moves the cursor.
    private static func walkTheirs(_ holder: inout SetupJSON, _ name: String, _ theirsArray: SetupJSON?,
                                   taken: Set<String>, entry: (String?) -> String) {
        var cursor = -1
        var idless = 0
        for element in theirsArray?.objects ?? [] {
            let id = element.id
            let key = entry(id)
            let items = holder[name]?.items ?? []
            let index: Int?
            if let id {
                index = items.firstIndex { $0.id == id }
            } else if taken.contains(key) {
                index = nil
            } else {
                let positions = items.indices.filter { items[$0].isObject && items[$0].id == nil }
                index = idless < positions.count ? positions[idless] : nil
                idless += 1
            }
            if let index {
                cursor = index
                continue
            }
            guard taken.contains(key) else { continue }
            // An array mine never had is made only for something that goes in it.
            if holder[name]?.items == nil { holder[name] = .array([]) }
            cursor += 1
            let at = cursor
            holder.modifyItems(of: name) { $0.insert(element, at: at) }
        }
    }
}
