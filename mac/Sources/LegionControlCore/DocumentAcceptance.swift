import Foundation

/// Which setup documents this app accepts: section 9.7 of the contract.
///
/// Before this there were five answers to that question, one per client, the validator and the
/// agent, and a setup one device wrote could be refused by the next. A client that refuses its
/// peer's document stops replicating without anybody noticing (C3). So acceptance is one function,
/// ported from `contract/tools/document-rules.mjs` and held to `contract/vectors/setup-acceptance.json`:
/// a document is refused for a broken schema constraint or one of the relationship rules, and for
/// nothing else. Everything this app cannot use is a warning with a code, so it can say why a machine
/// is not driven without refusing the setup it is in.
///
/// Where a document came from decides what a failure means. A peer's copy, a kept revision or a
/// merge result (`peer`), and one a person typed or opened (`local`), is refused. The document this
/// device already applies (`applied`) is kept as long as section 12 accepts its bytes: its errors are
/// shown, it is not published, and a machine an error is inside is not driven, but it still decides
/// and merges, because refusing it would leave this device with no setup at all.
///
/// The checks follow the reference's JavaScript exactly, numbers included: a port of `22.0` is the
/// integer 22 there, so it is here.
enum DocumentAcceptance {

    enum Intake: String, Sendable, Equatable {
        case peer
        case local
        case applied
    }

    /// One thing wrong with a document, or one thing worth saying about it.
    struct Problem: Sendable, Equatable, Hashable {
        var code: String
        /// Where it is, written the reference's way: `machines[2].wake.helper`.
        var path: String
        var message: String

        /// The index of the machine this problem is inside, when it is inside one.
        var machineIndex: Int? { index(in: "machines") }

        private func index(in list: String) -> Int? {
            guard path.hasPrefix("\(list)[") else { return nil }
            return Int(path.dropFirst(list.count + 1).prefix { $0 != "]" })
        }

        /// What the problem is and where, with the machine or site it is inside named by its id
        /// rather than its place in the list: the same fault in a document an edit reordered or
        /// renamed is still the same fault.
        func identity(in document: SetupJSON?) -> String {
            for list in ["machines", "sites"] {
                guard let index = index(in: list), let id = document?[list]?.items?[safe: index]?.id else { continue }
                let rest = path.drop { $0 != "]" }.dropFirst()
                return "\(code) \(list){\(id)}\(rest)"
            }
            return "\(code) \(path)"
        }
    }

    struct Verdict: Sendable, Equatable {
        var valid: Bool
        var errors: [Problem]
        var warnings: [Problem]
        /// Local intake gave the document an identity before checking it.
        var assignsIdentity: Bool
        /// Whether this app goes on using the document: a refused peer or local document is not
        /// used, an applied one is used with its errors shown.
        var keep: Bool
    }

    /// The setup id local intake checks a document under when it has none. The device mints a fresh
    /// one when it actually writes the document; any valid id gives the same verdict.
    static let assignedSetupID = "setup-00000000-0000-4000-8000-000000000000"

    /// The errors a merge may take mine into: how parts that are well formed on their own refer to
    /// each other. Every other error leaves no single answer to what the document's entries are, so a
    /// document with one is fixed in the editor before it is merged (section 9.8).
    static let mergeableErrors: Set<String> = ["unknown-site", "helper-self", "helper-unknown", "helper-alias", "helper-cycle"]

    // MARK: - Checking

    /// Section 9.7 for a document as bytes under one intake.
    ///
    /// The order is fixed. Section 12 comes first: a text it refuses is no document at all, so it is
    /// a `schema` error under every intake and not even applied intake keeps it. Then a repeated
    /// member name, which when it applies is the only thing reported. Then local intake's identity,
    /// then the schema and the rules, and the warnings only for a document with no error.
    static func check(_ text: Data, intake: Intake) -> Verdict {
        if let refusal = Canonical.rejection(text) {
            let error = Problem(code: "schema", path: "",
                                message: "This is not a setup document: \(refusal.sentence)")
            return Verdict(valid: false, errors: [error], warnings: [], assignsIdentity: false, keep: false)
        }
        guard let canonical = try? Canonical.bytes(text),
              let reading = try? SetupJSON.parseReportingRepeats(canonical) else {
            // Section 12 parsed it a moment ago; this cannot happen with the same bytes.
            let error = Problem(code: "schema", path: "", message: "This is not a setup document.")
            return Verdict(valid: false, errors: [error], warnings: [], assignsIdentity: false, keep: false)
        }
        if let repeated = reading.repeated {
            let error = Problem(code: "duplicate-member", path: repeated,
                                message: "\(repeated) is written twice in one object, and every program reading it may take a different one.")
            return Verdict(valid: false, errors: [error], warnings: [], assignsIdentity: false, keep: intake == .applied)
        }
        var document = reading.value
        let assignsIdentity = intake == .local && needsIdentity(document)
        if assignsIdentity { document = withAssignedIdentity(document) }
        let (errors, warnings) = check(document: document)
        return Verdict(valid: errors.isEmpty, errors: errors, warnings: warnings, assignsIdentity: assignsIdentity,
                       keep: intake == .applied ? true : errors.isEmpty)
    }

    /// The errors and warnings of a parsed document under peer intake. Warnings are only worked out
    /// for a document with no error: a person is shown the errors of a broken document and the
    /// warnings of a working one.
    static func check(document: SetupJSON) -> (errors: [Problem], warnings: [Problem]) {
        let errors = unique(schemaProblems(document) + ruleProblems(document))
        let warnings = errors.isEmpty ? unique(warningProblems(document)) : []
        return (errors, warnings)
    }

    /// Whether local intake gives this document an identity before checking it: when it has no
    /// `controller` block, or a block whose id is absent or null.
    static func needsIdentity(_ document: SetupJSON) -> Bool {
        guard let block = document["controller"] else { return true }
        guard block.isObject else { return false }
        return block["id"] == nil || block["id"] == .null
    }

    private static func withAssignedIdentity(_ document: SetupJSON) -> SetupJSON {
        var copy = document
        var block = document["controller"].flatMap { $0.isObject ? $0 : nil } ?? .object([])
        block["id"] = .string(assignedSetupID)
        block["revision"] = .number("1")
        block["lineage"] = .array([])
        copy["controller"] = block
        return copy
    }

    /// Each problem once, in the order found. Two rules can describe the same fault from two ends.
    private static func unique(_ problems: [Problem]) -> [Problem] {
        var seen = Set<Problem>()
        return problems.filter { seen.insert($0).inserted }
    }

    // MARK: - The string grammars

    /// The setup document schema's token, `^[A-Za-z0-9][A-Za-z0-9._:@/\~=+-]*$`.
    private static func isToken(_ value: SetupJSON) -> Bool {
        value.stringValue.map(AgentToken.isValid) ?? false
    }

    private static func isText(_ value: SetupJSON) -> Bool {
        guard let text = value.stringValue else { return false }
        return !text.isEmpty
    }

    private static func isPort(_ value: SetupJSON) -> Bool {
        guard value.isInteger, let number = value.numberValue else { return false }
        return number >= 1 && number <= 65535
    }

    /// `^[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}$`. Either separator, mixed if it must be.
    private static func isMAC(_ value: SetupJSON) -> Bool {
        guard let text = value.stringValue else { return false }
        let scalars = Array(text.unicodeScalars)
        guard scalars.count == 17 else { return false }
        for (index, scalar) in scalars.enumerated() {
            if index % 3 == 2 {
                guard scalar == ":" || scalar == "-" else { return false }
            } else {
                guard scalar.isASCII, scalar.properties.isASCIIHexDigit else { return false }
            }
        }
        return true
    }

    private static func guarded(_ check: @escaping (String) -> Bool) -> (SetupJSON) -> Bool {
        { value in value.stringValue.map(check) ?? false }
    }

    private static let sources = ["mac", "desktop", "phone", "cli", "legacy"]
    private static let platforms = ["linux", "windows", "mac"]
    private static let shells = ["posix", "powershell", "cmd"]
    private static let kinds = ["lan", "remote"]

    private static func join(_ base: String, _ key: String) -> String { base.isEmpty ? key : "\(base).\(key)" }

    private static func show(_ value: SetupJSON) -> String { value.compactText }

    // MARK: - The schema, written out (code `schema`)

    private static func schemaProblems(_ document: SetupJSON) -> [Problem] {
        var problems: [Problem] = []
        func add(_ path: String, _ message: String) { problems.append(Problem(code: "schema", path: path, message: message)) }

        /// An optional key held to a rule.
        func string(_ object: SetupJSON, _ key: String, _ path: String, _ rule: (SetupJSON) -> Bool, _ what: String) {
            if let value = object[key], !rule(value) { add(join(path, key), "\(join(path, key)) is \(show(value)), which is not \(what).") }
        }
        /// An optional array key: its type and limits, then `each` on every item.
        func list(_ object: SetupJSON, _ key: String, _ path: String, min: Int = 0, max: Int,
                  _ each: (SetupJSON, String) -> Void) {
            guard let value = object[key] else { return }
            let at = join(path, key)
            guard let items = value.items else {
                add(at, "\(at) is not a list.")
                return
            }
            if items.count < min { add(at, "\(at) has \(items.count) entries and needs at least \(min).") }
            if items.count > max { add(at, "\(at) has \(items.count) entries, and at most \(max) are allowed.") }
            for (index, item) in items.enumerated() { each(item, "\(at)[\(index)]") }
        }
        func required(_ object: SetupJSON, _ keys: [String], _ path: String) {
            for key in keys where object[key] == nil { add(join(path, key), "\(join(path, key)) is missing, and it is required.") }
        }
        func object(_ value: SetupJSON, _ path: String, _ what: String) -> Bool {
            if value.isObject { return true }
            add(path, "\(path.isEmpty ? "The document" : path) is not \(what).")
            return false
        }
        func port(_ object: SetupJSON, _ key: String, _ path: String) {
            if let value = object[key], !isPort(value) {
                add(join(path, key), "\(join(path, key)) is \(show(value)), which is not a port: a whole number from 1 to 65535.")
            }
        }
        func nonEmpty(_ value: SetupJSON, _ path: String, _ what: String) {
            if !isText(value) { add(path, "\(path) is \(show(value)), which is not \(what).") }
        }
        func oneOf(_ object: SetupJSON, _ key: String, _ path: String, _ allowed: [String]) {
            if let value = object[key], !(value.stringValue.map(allowed.contains) ?? false) {
                add(join(path, key), "\(join(path, key)) is \(show(value)), which is not one of \(allowed.joined(separator: ", ")).")
            }
        }
        func boolean(_ object: SetupJSON, _ key: String, _ path: String) {
            if let value = object[key], value.boolValue == nil { add(join(path, key), "\(join(path, key)) is not true or false.") }
        }

        guard document.isObject else {
            add("", "The document is not a JSON object.")
            return problems
        }

        if let version = document["version"] {
            if version.numberValue != 1 { add("version", "version is \(show(version)); it stays 1.") }
        } else {
            add("version", "version is missing. It is required, and it is 1.")
        }
        if document["machines"] == nil { add("machines", "machines is missing, and it is required.") }

        if let block = document["controller"], object(block, "controller", "an object") {
            required(block, ["id", "revision"], "controller")
            string(block, "id", "controller", guarded(AgentToken.isValidSetupID),
                   "a setup id: a letter or digit, then up to 63 letters, digits, . _ : or -")
            string(block, "name", "controller", isText, "a name")
            if let revision = block["revision"], !(revision.isInteger && (revision.numberValue ?? -1) >= 0) {
                add("controller.revision", "controller.revision is \(show(revision)), which is not a revision: a whole number from 0.")
            }
            if let updatedAt = block["updatedAt"], updatedAt.stringValue == nil { add("controller.updatedAt", "controller.updatedAt is not a string.") }
            if let source = block["source"], !(source.stringValue.map(sources.contains) ?? false) {
                add("controller.source", "controller.source is \(show(source)), which is not one of \(sources.joined(separator: ", ")); the device's own name goes in device.")
            }
            string(block, "device", "controller", isText, "a device name")
            list(block, "lineage", "controller", max: ControllerIdentity.lineageLimit) { hash, path in
                if !(hash.stringValue.map(AgentToken.isValidHash) ?? false) { add(path, "\(path) is \(show(hash)), which is not a lower case sha256.") }
            }
            if let lineage = block["lineage"]?.items {
                var seen = Set<String>()
                for (index, hash) in lineage.enumerated() where !seen.insert(hash.equalityKey).inserted {
                    add("controller.lineage[\(index)]", "controller.lineage lists \(show(hash)) twice; a document cannot descend from one ancestor twice.")
                }
            }
        }

        list(document, "sites", "", max: 20) { site, path in
            guard object(site, path, "a site") else { return }
            required(site, ["id"], path)
            string(site, "id", path, isToken, "a site id")
            string(site, "name", path, isText, "a name")
            list(site, "lanPrefixes", path, min: 1, max: 10) { prefix, at in nonEmpty(prefix, at, "a prefix") }
            list(site, "broadcast", path, max: 10) { address, at in nonEmpty(address, at, "a broadcast address") }
        }

        func helper(_ value: SetupJSON, _ path: String) {
            guard object(value, path, "a wake helper") else { return }
            required(value, ["machine", "action"], path)
            string(value, "machine", path, isToken, "a machine id")
            string(value, "action", path, guarded(AgentToken.isValidID), "an action id: a letter or digit, then letters, digits, . _ or -")
        }

        list(document, "machines", "", max: 50) { machine, path in
            guard object(machine, path, "a machine") else { return }
            required(machine, ["id"], path)
            string(machine, "id", path, isToken, "a machine id")
            string(machine, "name", path, isText, "a name")
            string(machine, "site", path, isToken, "a site id")
            boolean(machine, "alwaysOn", path)

            list(machine, "endpoints", path, max: 20) { endpoint, at in
                guard object(endpoint, at, "an endpoint") else { return }
                required(endpoint, ["host"], at)
                string(endpoint, "id", at, isToken, "an endpoint id")
                oneOf(endpoint, "kind", at, kinds)
                string(endpoint, "host", at, guarded(SetupGuard.isHost), "a host name or address")
                port(endpoint, "port", at)
                string(endpoint, "user", at, guarded(SetupGuard.isUser), "a login name ssh can be given safely")
                string(endpoint, "system", at, isToken, "a system id")
                string(endpoint, "label", at, isText, "a label")
            }

            if let wake = machine["wake"], object(wake, join(path, "wake"), "an object") {
                let at = join(path, "wake")
                string(wake, "mac", at, isMAC, "a hardware address of six two-digit hex bytes")
                list(wake, "broadcast", at, max: 10) { address, where_ in nonEmpty(address, where_, "a broadcast address") }
                list(wake, "ports", at, max: 5) { value, where_ in
                    if !isPort(value) { add(where_, "\(where_) is \(show(value)), which is not a port: a whole number from 1 to 65535.") }
                }
                if let probe = wake["probe"], object(probe, join(at, "probe"), "an object") {
                    required(probe, ["host"], join(at, "probe"))
                    string(probe, "host", join(at, "probe"), guarded(SetupGuard.isHost), "a host name or address")
                    port(probe, "port", join(at, "probe"))
                }
                string(wake, "lanPrefix", at, isText, "a prefix")
                if let single = wake["helper"] { helper(single, join(at, "helper")) }
                list(wake, "helpers", at, max: 5) { value, where_ in helper(value, where_) }
            }

            list(machine, "systems", path, max: 10) { system, at in
                guard object(system, at, "a system") else { return }
                required(system, ["id"], at)
                string(system, "id", at, isToken, "a system id")
                string(system, "name", at, isText, "a name")
                oneOf(system, "platform", at, platforms)
                oneOf(system, "shell", at, shells)
                boolean(system, "restricted", at)
                list(system, "agent", at, max: 20) { item, where_ in
                    if !(item.stringValue.map(SetupGuard.isAgentArg) ?? false) {
                        add(where_, "\(where_) is \(show(item)), which is not a word of a command: it is empty, longer than 4096 bytes or holds a control character.")
                    }
                }
            }

            if let ssh = machine["ssh"], object(ssh, join(path, "ssh"), "an object") {
                let at = join(path, "ssh")
                string(ssh, "host", at, guarded(SetupGuard.isHost), "a host name or address")
                string(ssh, "user", at, guarded(SetupGuard.isUser), "a login name ssh can be given safely")
                port(ssh, "port", at)
                string(ssh, "identityFile", at, isText, "a path")
            }
        }

        if let updates = document["appUpdates"], object(updates, "appUpdates", "an object") {
            required(updates, ["githubRepo"], "appUpdates")
            string(updates, "githubRepo", "appUpdates", guarded(SetupGuard.isRepository), "a GitHub owner/name")
        }
        for key in ["local", "ssh"] {
            if let value = document[key], value != .null, !value.isObject { add(key, "\(key) is neither an object nor null.") }
        }
        return problems
    }

    // MARK: - The relationship rules (errors)

    /// Every wake helper of a machine: the entries of `wake.helpers` and the alias `wake.helper`.
    /// Both count, because clients disagree about which one does when `helpers` is empty, and a
    /// helper some client would run must name a machine that exists and must not lead back to the
    /// one it wakes.
    private static func helpers(of machine: SetupJSON?) -> [(machine: String, action: String?)] {
        guard let wake = machine?["wake"], wake.isObject else { return [] }
        var list = wake["helpers"]?.items ?? []
        if let single = wake["helper"] { list.append(single) }
        return list.compactMap { helper in
            guard helper.isObject, let id = helper["machine"]?.stringValue else { return nil }
            return (id, helper["action"]?.stringValue)
        }
    }

    private static func label(_ machine: SetupJSON) -> String {
        machine["name"]?.stringValue.flatMap { $0.isEmpty ? nil : $0 } ?? machine.id ?? "A machine"
    }

    private static func ruleProblems(_ document: SetupJSON) -> [Problem] {
        var problems: [Problem] = []
        func add(_ code: String, _ path: String, _ message: String) { problems.append(Problem(code: code, path: path, message: message)) }
        guard document.isObject else { return problems }
        let machines = document["machines"]?.items ?? []
        let sites = document["sites"]?.items ?? []

        var machineIds = Set<String>()
        for (index, machine) in machines.enumerated() {
            guard machine.isObject, let id = machine.id else { continue }
            if !machineIds.insert(id).inserted {
                add("duplicate-machine", "machines[\(index)].id", "Two machines have the id \"\(id)\". Ids have to be unique.")
            }
            // Endpoints without an id are never duplicates: they cannot be remembered as a route,
            // and that is a warning, not a reason to refuse the setup.
            var endpointIds = Set<String>()
            for (k, endpoint) in (machine["endpoints"]?.items ?? []).enumerated() {
                guard endpoint.isObject, let endpointId = endpoint.id else { continue }
                if !endpointIds.insert(endpointId).inserted {
                    add("duplicate-endpoint", "machines[\(index)].endpoints[\(k)].id",
                        "\(label(machine)) has two addresses with the id \"\(endpointId)\", so a remembered route could be either.")
                }
            }
            var systemIds = Set<String>()
            for (k, system) in (machine["systems"]?.items ?? []).enumerated() {
                guard system.isObject, let systemId = system.id else { continue }
                if !systemIds.insert(systemId).inserted {
                    add("duplicate-system", "machines[\(index)].systems[\(k)].id", "\(label(machine)) has two systems with the id \"\(systemId)\".")
                }
            }
        }

        var siteIds = Set<String>()
        for (index, site) in sites.enumerated() {
            guard site.isObject, let id = site.id else { continue }
            if !siteIds.insert(id).inserted {
                add("duplicate-site", "sites[\(index)].id", "Two sites have the id \"\(id)\". Ids have to be unique.")
            }
        }

        for (index, machine) in machines.enumerated() {
            guard machine.isObject else { continue }
            // A setup without sites places nobody, so a site name is only checked against a list
            // that exists.
            if let site = machine["site"]?.stringValue, !sites.isEmpty, !siteIds.contains(site) {
                add("unknown-site", "machines[\(index)].site",
                    "\(label(machine)) names the site \"\(site)\", which is not in the sites list, so it drops out of every decision about where it is.")
            }
            for helper in helpers(of: machine) {
                if helper.machine == machine.id {
                    add("helper-self", "machines[\(index)].wake",
                        "\(label(machine)) lists itself as its own wake helper. A machine that is asleep cannot wake itself.")
                } else if !machineIds.contains(helper.machine) {
                    add("helper-unknown", "machines[\(index)].wake",
                        "The wake helper \"\(helper.machine)\" of \(label(machine)) is not a machine in this setup.")
                }
            }
            // The singular key is what a 1.2 client reads. If the two disagree, an old and a new
            // client try different machines from the same document.
            if let wake = machine["wake"], let single = wake["helper"], single.isObject,
               let first = wake["helpers"]?.items?.first, first.isObject,
               !SetupJSON.equal(first["machine"], single["machine"]) || !SetupJSON.equal(first["action"], single["action"]) {
                add("helper-alias", "machines[\(index)].wake.helper",
                    "\(label(machine)) lists \(first["machine"]?.stringValue ?? "?") as its first wake helper but \(single["machine"]?.stringValue ?? "?") in the compatibility \"helper\" key. They have to be the same machine and action.")
            }
        }

        // A ring of helpers is the failure that looks fine one machine at a time: every machine in
        // it waits for another to be awake first.
        var byId: [String: SetupJSON] = [:]
        for machine in machines where machine.isObject {
            if let id = machine.id, byId[id] == nil { byId[id] = machine }
        }
        for (index, machine) in machines.enumerated() {
            guard machine.isObject, let start = machine.id else { continue }
            var seen: Set<String> = [start]
            func walk(_ id: String, _ trail: [String]) {
                for helper in helpers(of: byId[id]) {
                    if seen.contains(helper.machine) {
                        if helper.machine == start, !trail.isEmpty {
                            add("helper-cycle", "machines[\(index)].wake",
                                "The wake helpers form a loop: \((trail + [id, helper.machine]).joined(separator: " needs ")). Every machine in it waits for another to be awake first.")
                        }
                        continue
                    }
                    guard byId[helper.machine] != nil else { continue }
                    seen.insert(helper.machine)
                    walk(helper.machine, trail + [id])
                }
            }
            walk(start, [])
        }
        return problems
    }

    // MARK: - What this app cannot use (warnings)

    /// Only read for a document with no error, so every shape here is one the schema allows.
    private static func warningProblems(_ document: SetupJSON) -> [Problem] {
        var warnings: [Problem] = []
        func add(_ code: String, _ path: String, _ message: String) { warnings.append(Problem(code: code, path: path, message: message)) }
        let machines = document["machines"]?.items ?? []
        let siteIds = Set((document["sites"]?.items ?? []).compactMap(\.id))
        var byId: [String: SetupJSON] = [:]
        for machine in machines { if let id = machine.id, byId[id] == nil { byId[id] = machine } }

        if machines.isEmpty { add("no-machines", "machines", "The setup has no machines yet, so there is nothing to drive.") }

        /// Section 15.6's dial grammar, on a value the guard already passed.
        func dialable(_ holder: SetupJSON, _ path: String, _ owner: String) {
            if let host = holder["host"]?.stringValue, Addresses.hostForm(host) == nil {
                add("endpoint-undialable", join(path, "host"),
                    "\(owner) lists \(SetupGuard.describe(host)), which this app never dials, scans or pins: it is not a name or address it accepts.")
            }
            if let user = holder["user"]?.stringValue, !Addresses.isUser(user) {
                add("endpoint-undialable", join(path, "user"),
                    "\(owner) has the login name \(SetupGuard.describe(user)), which this app never dials with.")
            }
            if let port = holder["port"], !isPort(port) {
                add("endpoint-undialable", join(path, "port"), "\(owner) has the port \(show(port)), which this app never dials.")
            }
        }

        for (index, machine) in machines.enumerated() {
            let path = "machines[\(index)]"
            let name = label(machine)
            let systems = machine["systems"]?.items ?? []
            let endpoints = machine["endpoints"]?.items ?? []
            let systemIds = Set(systems.compactMap(\.id))
            let legacyHost = machine["ssh"]?["host"]?.stringValue != nil

            if systems.isEmpty {
                add("machine-no-systems", path, "\(name) lists no systems, so nothing can be run on it yet. Add one with the agent command for that machine.")
            }
            if endpoints.isEmpty, !legacyHost {
                add("machine-no-endpoints", path, "\(name) has no address and no ssh host, so there is no way to reach it.")
            }
            for (k, system) in systems.enumerated() where (system["agent"]?.items ?? []).isEmpty {
                let systemName = system["name"]?.stringValue ?? system.id ?? "A system"
                add("system-no-agent", "\(path).systems[\(k)]", "\(systemName) on \(name) has no agent command, so it is not driven.")
            }
            for (k, endpoint) in endpoints.enumerated() {
                let at = "\(path).endpoints[\(k)]"
                if endpoint["id"] == nil {
                    add("endpoint-no-id", at, "An address of \(name) has no id, so it cannot be remembered as the route that worked, and it merges with the other addresses without one.")
                }
                if endpoint["user"] == nil {
                    add("endpoint-no-user", at, "An address of \(name) names no user, so ssh logs in as whatever this device's own ssh configuration says.")
                }
                if let hint = endpoint["system"]?.stringValue, !systemIds.contains(hint) {
                    add("endpoint-unknown-system", join(at, "system"), "An address of \(name) is marked for the system \"\(hint)\", which \(name) does not have, so the mark is ignored.")
                }
                dialable(endpoint, at, name)
            }
            if let ssh = machine["ssh"], ssh.isObject { dialable(ssh, "\(path).ssh", name) }

            guard let wake = machine["wake"], wake.isObject else { continue }
            let helpers = helpers(of: machine)
            if wake["mac"] == nil {
                add("wake-no-mac", "\(path).wake", "The wake block of \(name) has no hardware address, so no packet can wake it directly. Its helpers still can.")
            }
            let namesSite = machine["site"]?.stringValue.map(siteIds.contains) ?? false
            if helpers.isEmpty, !namesSite, wake["lanPrefix"]?.stringValue.flatMap(LanPrefix.init) == nil {
                add("wake-unplaced", "\(path).wake", "\(name) can only be woken by a device that is already on its network: it names no site, no valid LAN prefix and no helper.")
            }
            if let single = wake["helper"], single.isObject, wake["helpers"]?.items?.isEmpty == true {
                add("helper-alias-unused", "\(path).wake.helper", "\(name) has a \"helper\" beside an empty \"helpers\" list, which some apps read as no helper at all and others as that one. Write it into helpers as well.")
            }
            for helper in helpers {
                guard let target = byId[helper.machine] else { continue }
                if let site = machine["site"]?.stringValue, let helperSite = target["site"]?.stringValue, helperSite != site {
                    add("helper-other-site", "\(path).wake", "\(label(target)) is listed as a wake helper for \(name) but they are at different sites. That only works if the action reaches \(name)'s network another way, through a router or a tunnel.")
                }
                if target["alwaysOn"] != .bool(true) {
                    add("helper-not-always-on", "\(path).wake", "\(label(target)) is a wake helper for \(name) but is not marked as always on, so waking \(name) may need \(label(target)) to be woken first.")
                }
            }
        }
        return warnings + prefixWarnings(document)
    }

    /// The warnings that come from addresses: `shared-prefix`, `prefix-invalid`,
    /// `prefix-without-dot`, `broadcast-invalid` and `wake-broadcast-defaulted`, in document order.
    private static func prefixWarnings(_ document: SetupJSON) -> [Problem] {
        var warnings: [Problem] = []
        func add(_ code: String, _ path: String, _ message: String) { warnings.append(Problem(code: code, path: path, message: message)) }
        let sites = document["sites"]?.items ?? []
        let machines = document["machines"]?.items ?? []

        func checkPrefix(_ value: SetupJSON, _ path: String) -> LanPrefix? {
            let text = value.stringValue ?? ""
            guard let parsed = value.stringValue.flatMap(LanPrefix.init) else {
                add("prefix-invalid", path, "\(show(value)) is not a dotted or CIDR IPv4 prefix, so it never matches any address.")
                return nil
            }
            if parsed.form == .dotted, !text.hasSuffix(".") {
                add("prefix-without-dot", path, "\(show(value)) has no trailing dot, so apps from before 1.4 also take addresses whose next number merely begins with its last one. Write \(text). instead.")
            }
            return parsed
        }
        func checkBroadcasts(_ list: SetupJSON?, _ path: String) {
            for (index, entry) in (list?.items ?? []).enumerated() where entry.stringValue.flatMap(LanPrefix.ipv4) == nil {
                add("broadcast-invalid", "\(path)[\(index)]", "\(show(entry)) is not a dotted IPv4 address, so no packet is sent to it and it is never looked up as a name.")
            }
        }
        func validBroadcasts(_ list: SetupJSON?) -> Int {
            (list?.items ?? []).filter { $0.stringValue.flatMap(LanPrefix.ipv4) != nil }.count
        }

        // Each site's prefixes and broadcasts, keeping the valid prefixes for the overlap check.
        let parsedBySite: [[(text: String, prefix: LanPrefix)]] = sites.enumerated().map { index, site in
            guard site.isObject else { return [] }
            var parsed: [(text: String, prefix: LanPrefix)] = []
            for (k, text) in (site["lanPrefixes"]?.items ?? []).enumerated() {
                if let prefix = checkPrefix(text, "sites[\(index)].lanPrefixes[\(k)]") { parsed.append((text.stringValue ?? "", prefix)) }
            }
            checkBroadcasts(site["broadcast"], "sites[\(index)].broadcast")
            return parsed
        }

        // Overlapping prefixes on two sites are legal and expected: home routers ship the same
        // default range. Matching one then no longer says which site a device is on.
        for j in sites.indices.dropFirst() {
            for i in 0..<j {
                let pair = parsedBySite[i].lazy.flatMap { a in parsedBySite[j].lazy.map { b in (a, b) } }.first { $0.0.prefix.overlaps($0.1.prefix) }
                guard let pair else { continue }
                let (a, b) = pair
                let what = a.text == b.text ? "both use the prefix \(a.text)" : "have the overlapping prefixes \(a.text) and \(b.text)"
                add("shared-prefix", "sites[\(j)].lanPrefixes",
                    "The sites \(sites[i].id ?? "?") and \(sites[j].id ?? "?") \(what), so being on it does not say which of them this device is at. Choose the site here when it matters.")
            }
        }

        var siteById: [String: SetupJSON] = [:]
        for site in sites where site.isObject {
            if let id = site.id, siteById[id] == nil { siteById[id] = site }
        }

        for (index, machine) in machines.enumerated() {
            guard machine.isObject, let wake = machine["wake"], wake.isObject else { continue }
            let path = "machines[\(index)].wake"
            let site = machine["site"]?.stringValue.flatMap { siteById[$0] }
            let lanPrefix = wake["lanPrefix"].flatMap { checkPrefix($0, "\(path).lanPrefix") }
            checkBroadcasts(wake["broadcast"], "\(path).broadcast")

            // A machine that can be woken directly has a hardware address and a place: a site in
            // the list or, only when it names none, a valid lanPrefix.
            guard wake["mac"]?.stringValue != nil, site != nil || lanPrefix != nil else { continue }
            if validBroadcasts(wake["broadcast"]) == 0, site.map({ validBroadcasts($0["broadcast"]) == 0 }) ?? true {
                add("wake-broadcast-defaulted", path,
                    "\(label(machine)) can be woken directly but names no dotted broadcast address\(site == nil ? "" : ", and neither does its site"), so the packet goes to the broadcast address of this device's own network, or to 255.255.255.255.")
            }
        }
        return warnings
    }
}

private extension SetupJSON {
    /// What tells two lineage entries apart, the way `JSON.stringify` would print them.
    var equalityKey: String {
        switch self {
        case .string(let value): "s" + value.unicodeScalars.map { String($0.value, radix: 16) }.joined(separator: " ")
        case .number(let lexeme): "n" + (SetupJSON.decimalKey(lexeme) ?? lexeme)
        default: "v" + compactText
        }
    }
}

private extension Array {
    subscript(safe index: Int) -> Element? { indices.contains(index) ? self[index] : nil }
}
