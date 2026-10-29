import SwiftUI

/// Editing the shared setup with forms rather than by hand.
///
/// The file stays hand-editable and stays the source of truth; this is a way of getting the fields
/// right without remembering the schema. Every save is validated before it reaches the disk, the
/// previous document is kept, and the revision and lineage are stamped on for you — which is the
/// part a hand edit cannot do, and the part that lets every other device tell your change from
/// somebody else's.
struct SetupEditorView: View {
    let model: AppModel

    @State private var problem: String?
    @State private var selection: Selection = .sites

    enum Selection: Hashable {
        case sites
        case bindings
        case machine(String)
        case newMachine
    }

    private var config: ControllerConfig? { model.config.config }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PageHeading(title: "Setup", note: model.config.config?.identity.name)

            if LaunchWaitingNotice.isNeeded(model) {
                LaunchWaitingNotice(model: model)
                    .padding(.bottom, 16)
            }

            Picker("Part of the setup", selection: $selection) {
                Text("Sites").tag(Selection.sites)
                Text("This device").tag(Selection.bindings)
                ForEach(config?.machines ?? []) { machine in
                    Text(machine.name).tag(Selection.machine(machine.id))
                }
                Text("Add a machine").tag(Selection.newMachine)
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .frame(maxWidth: 320, alignment: .leading)
            .padding(.bottom, 16)

            switch selection {
            case .bindings:
                BindingsEditor(model: model)
            case .sites:
                SitesEditor(model: model, problem: $problem)
            case .machine(let id):
                if let machine = config?.machine(id: id) {
                    MachineEditor(model: model, machine: machine, problem: $problem)
                } else {
                    Text("That machine is no longer in the setup.").foregroundStyle(.secondary)
                }
            case .newMachine:
                NewMachineEditor(model: model, problem: $problem) { id in
                    selection = .machine(id)
                }
            }

            if let problem {
                Text(problem)
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 14)
            }

            if !model.setupErrors.isEmpty {
                SetupErrorsNote(errors: model.setupErrors).padding(.top, 12)
            }
            ForEach(model.setupWarnings, id: \.self) { warning in
                QuietNote(text: warning).padding(.top, 12)
            }
        }
    }
}

/// What the applied setup breaks, said once, with the reason it matters.
struct SetupErrorsNote: View {
    let errors: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            StatusText(symbol: "exclamationmark.triangle",
                       text: "This setup breaks a rule every device holds it to, so it is not shared until it is fixed, and a machine with a problem is not driven.",
                       tint: .orange)
            ForEach(errors, id: \.self) { error in
                Text(error)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

// MARK: - Sites

private struct SitesEditor: View {
    let model: AppModel
    @Binding var problem: String?

    @State private var draftId = ""
    @State private var draftName = ""
    @State private var draftPrefixes = ""
    @State private var draftBroadcast = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DisclosureGroup("About sites") {
                Text("A site groups machines on one network and chooses local wake routes. Address prefixes are location hints; SSH host keys establish identity.")
                    .font(.callout).foregroundStyle(.secondary)
            }.padding(.bottom, 14)

            ForEach(model.sites) { site in
                VStack(alignment: .leading, spacing: 6) {
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text(site.name).font(.headline)
                        Text(site.id).font(.system(.caption, design: .monospaced)).foregroundStyle(.secondary)
                        Spacer(minLength: 8)
                        Button("Remove") {
                            problem = model.editSetup("removed the site \(site.name)") { root in
                                try ControllerEditor.removeSite(id: site.id, in: &root)
                            }
                        }
                        .controlSize(.small)
                    }
                    Text("addresses in \(site.lanPrefixes.isEmpty ? "—" : site.lanPrefixes.joined(separator: ", "))")
                        .font(.callout).foregroundStyle(.secondary)
                    Text("broadcast \(site.broadcast.isEmpty ? "—" : site.broadcast.joined(separator: ", "))")
                        .font(.callout).foregroundStyle(.secondary)
                }
                .padding(.bottom, 12)
            }

            SectionHeading(title: "Add a site")

            VStack(alignment: .leading, spacing: 10) {
                LabelledField("Id", text: $draftId, placeholder: "home-a")
                LabelledField("Name", text: $draftName, placeholder: "Attic house")
                LabelledField("Address prefixes", text: $draftPrefixes, placeholder: "192.168.50., 10.0.0.0/20")
                LabelledField("Broadcast", text: $draftBroadcast, placeholder: "10.0.0.255")

                Button("Add the site") {
                    problem = model.editSetup("added the site \(draftName.isEmpty ? draftId : draftName)") { root in
                        try ControllerEditor.upsertSite(ControllerEditor.siteEntry(
                            id: draftId.trimmingCharacters(in: .whitespaces),
                            name: draftName.isEmpty ? draftId : draftName,
                            prefixes: splitList(draftPrefixes),
                            broadcast: splitList(draftBroadcast)
                        ), in: &root)
                    }
                    if problem == nil {
                        draftId = ""; draftName = ""; draftPrefixes = ""; draftBroadcast = ""
                    }
                }
                .buttonStyle(.borderedProminent)
                .disabled(draftId.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }
}

// MARK: - One machine

private struct MachineEditor: View {
    let model: AppModel
    let machine: Machine
    @Binding var problem: String?

    @State private var helperMachine = ""
    @State private var helperAction = ""

    private var config: ControllerConfig? { model.config.config }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 12) {
                DetailRow("Id", machine.id)
                DetailRow(label: "Site") {
                    Picker("Site", selection: Binding(
                        get: { machine.site ?? "" },
                        set: { site in
                            problem = model.editSetup("moved \(machine.name) to a different site") { root in
                                try ControllerEditor.setSite(site.isEmpty ? nil : site,
                                                             onMachine: machine.id, in: &root)
                            }
                        }
                    )) {
                        Text("Not placed").tag("")
                        ForEach(model.sites) { site in
                            Text(site.name).tag(site.id)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .fixedSize()
                }
                DetailRow(label: "Normally on") {
                    Toggle(isOn: Binding(
                        get: { machine.alwaysOn ?? false },
                        set: { on in
                            problem = model.editSetup("marked \(machine.name) as \(on ? "always on" : "not always on")") { root in
                                try ControllerEditor.setAlwaysOn(on, onMachine: machine.id, in: &root)
                            }
                        }
                    )) {
                        Text("This machine is normally left on, so it can wake others")
                    }
                    .toggleStyle(.switch)
                }
            }

            endpointsSection
            systemsSection
            wakeSection
        }
    }

    private var endpointsSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "Addresses")
            DisclosureGroup("Route details") {
                Text("Addresses are tried in order, preferring LAN routes at this site and remote routes elsewhere.")
                    .font(.callout).foregroundStyle(.secondary)
            }.padding(.bottom, 12)

            ForEach(machine.endpoints) { endpoint in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(endpoint.label ?? endpoint.host).font(.callout)
                    Text([endpoint.kind, endpoint.system].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer(minLength: 8)
                    Button("Remove") {
                        problem = model.editSetup("removed an address from \(machine.name)") { root in
                            try ControllerEditor.removeEndpoint(id: endpoint.id, fromMachine: machine.id, in: &root)
                        }
                    }
                    .controlSize(.small)
                }
                .padding(.bottom, 6)
            }

            EndpointForm(machineName: machine.name, systems: machine.systems) { entry in
                problem = model.editSetup("added an address to \(machine.name)") { root in
                    try ControllerEditor.upsertEndpoint(entry, onMachine: machine.id, in: &root)
                }
                return problem == nil
            }
        }
    }

    /// The grammar a system's lines are written in, or that it is not driven at all.
    private static func grammarLabel(_ system: SystemConfig) -> String {
        switch system.grammar {
        case .success(let grammar): grammar.rawValue
        case .failure(let refusal): refusal.code == "unknown-shell" ? "unknown shell, not driven" : "not driven"
        }
    }

    private var systemsSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            SectionHeading(title: "Systems")
            DisclosureGroup("Command details") {
                Text("Add one entry per operating system. Arguments are quoted for the selected shell; spaces inside paths are preserved.")
                    .font(.callout).foregroundStyle(.secondary)
            }.padding(.bottom, 12)

            ForEach(machine.systems) { system in
                VStack(alignment: .leading, spacing: 4) {
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text(system.name).font(.headline)
                        // The document's own field, because this is the document as it is written:
                        // a restricted system the dispatcher could not take has no grammar, and the
                        // hint is what says why it is not driven.
                        Text("\(system.platform.rawValue) · \(Self.grammarLabel(system))\(system.restricted == true ? " · restricted key" : "")")
                            .font(.caption).foregroundStyle(.secondary)
                        Spacer(minLength: 8)
                        if machine.systems.count > 1 {
                            Button("Remove") {
                                problem = model.editSetup("removed \(system.name) from \(machine.name)") { root in
                                    try ControllerEditor.removeSystem(id: system.id, fromMachine: machine.id, in: &root)
                                }
                            }
                            .controlSize(.small)
                        }
                    }
                    Text(LaunchValues.visible(system.agent.joined(separator: " ")))
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                    if let waiting = waiting(system) {
                        waitingRow(waiting)
                    }
                }
                .padding(.bottom, 10)
            }

            SystemForm(waiting: Set(machine.systems.filter { waiting($0) != nil }.map(\.id))) { entry in
                problem = model.editSetup("added a system to \(machine.name)") { root in
                    try ControllerEditor.upsertSystem(entry, onMachine: machine.id, in: &root)
                }
                return problem == nil
            }
        }
    }

    /// The review entry for a system whose command is waiting for approval on this device.
    private func waiting(_ system: SystemConfig) -> LaunchReviewEntry? {
        model.launchReview.first { $0.location == .system(machine: machine.id, system: system.id) }
    }

    /// A waiting command is read-only here, with its two answers beside it. Editing one field of it
    /// would otherwise make the rest of what the shared setup says about that system this device's
    /// own edit, approved without ever being shown whole.
    private func waitingRow(_ entry: LaunchReviewEntry) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            StatusText(symbol: "exclamationmark.shield",
                       text: entry.approvable ? "Waiting for approval on this device" : "Cannot be approved on this device",
                       tint: .orange)
                .font(.caption)
            Spacer(minLength: 8)
            if entry.approvable {
                Button("Approve") { model.approveLaunch([entry]) }
                    .controlSize(.small)
            }
            if entry.approved != nil {
                Button("Revert") { problem = model.revertLaunch(entry) }
                    .controlSize(.small)
            }
        }
    }

    @ViewBuilder
    private var wakeSection: some View {
        if let wake = machine.wake {
            VStack(alignment: .leading, spacing: 0) {
                SectionHeading(title: "Waking it")
                DisclosureGroup("Wake details") {
                    Text("Helpers send wake packets on the target network in the listed order. A sleeping helper requires a separate explicit wake request.")
                        .font(.callout).foregroundStyle(.secondary)
                }.padding(.bottom, 12)

                VStack(alignment: .leading, spacing: 10) {
                    ForEach(Array(wake.orderedHelpers.enumerated()), id: \.element.id) { index, helper in
                        HStack(alignment: .firstTextBaseline, spacing: 10) {
                            Text("\(index + 1).")
                                .foregroundStyle(.secondary)
                            Text(config?.machine(id: helper.machine)?.name ?? helper.machine)
                            Text(helper.action)
                                .font(.system(.callout, design: .monospaced))
                                .foregroundStyle(.secondary)
                            Spacer(minLength: 8)
                            Button("Remove") { setHelpers(wake.orderedHelpers.filter { $0.id != helper.id }) }
                                .controlSize(.small)
                            if index > 0 {
                                Button("Move up") {
                                    var helpers = wake.orderedHelpers
                                    helpers.swapAt(index, index - 1)
                                    setHelpers(helpers)
                                }
                                .controlSize(.small)
                            }
                        }
                    }
                }
                .padding(.bottom, 12)

                HStack(spacing: 10) {
                    Picker("Helper machine", selection: $helperMachine) {
                        Text("Choose a machine").tag("")
                        ForEach((config?.machines ?? []).filter { $0.id != machine.id }) { candidate in
                            Text(candidate.name).tag(candidate.id)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .fixedSize()

                    TextField("action id", text: $helperAction)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 180)

                    Button("Add the helper") {
                        setHelpers(wake.orderedHelpers + [WakeHelper(machine: helperMachine, action: helperAction)])
                        if problem == nil { helperMachine = ""; helperAction = "" }
                    }
                    .disabled(helperMachine.isEmpty || !AgentToken.isValid(helperAction))
                }
            }
        }
    }

    private func setHelpers(_ helpers: [WakeHelper]) {
        problem = model.editSetup("changed the wake helpers for \(machine.name)") { root in
            try ControllerEditor.setWakeHelpers(helpers, onMachine: machine.id, in: &root)
        }
    }
}

// MARK: - Adding a machine

private struct NewMachineEditor: View {
    let model: AppModel
    @Binding var problem: String?
    var onAdded: (String) -> Void

    @State private var id = ""
    @State private var name = ""
    @State private var host = ""
    @State private var user = ""
    @State private var agentPath = CommandArguments.text(CommandArguments.installedAgent(platform: .linux, user: ""))
    @State private var platform = Platform.linux
    @State private var windowsShell = "cmd"

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            QuietNote(text: "The smallest machine that works: an id, one address and one system. Everything else can be added afterwards.")
                .padding(.bottom, 6)

            LabelledField("Id", text: $id, placeholder: "tower")
            LabelledField("Name", text: $name, placeholder: "Tower")
            LabelledField("Address", text: $host, placeholder: "100.64.0.10 or an ssh alias")
            LabelledField("User", text: $user, placeholder: "me")

            HStack(alignment: .firstTextBaseline, spacing: 18) {
                Text("Platform").font(.subheadline).foregroundStyle(.secondary).frame(width: 140, alignment: .leading)
                Picker("Platform", selection: $platform) {
                    ForEach(Platform.allCases, id: \.self) { value in
                        Text(value.defaultName).tag(value)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                .fixedSize()
            }

            if platform == .windows {
                HStack(alignment: .firstTextBaseline, spacing: 18) {
                    Text("Shell").font(.subheadline).foregroundStyle(.secondary).frame(width: 140, alignment: .leading)
                    Picker("Shell", selection: $windowsShell) {
                        Text("cmd").tag("cmd")
                        Text("powershell").tag("powershell")
                    }
                    .labelsHidden().pickerStyle(.segmented).fixedSize()
                }
            }

            LabelledField("Agent arguments", text: $agentPath, placeholder: "[\"/absolute/base/bin/legionctl\"]")

            QuietNote(text: platform == .windows
                      ? "cmd is what a stock Windows OpenSSH runs. Choose powershell only if this account's OpenSSH DefaultShell points at it."
                      : "Enter a JSON array of arguments. Spaces inside each quoted path are preserved.")

            Button("Add the machine") { add() }
                .buttonStyle(.borderedProminent)
                .disabled(!canAdd)

            if let undialable = undialablePart(host: host, user: user) {
                QuietNote(text: "This app never dials \(undialable).")
            }
        }
        .onChange(of: user) { oldUser, newUser in
            if agentArgv == CommandArguments.installedAgent(platform: platform, user: oldUser) {
                agentPath = CommandArguments.text(CommandArguments.installedAgent(platform: platform, user: newUser))
            }
        }
        .onChange(of: platform) { oldPlatform, newPlatform in
            if agentArgv == CommandArguments.installedAgent(platform: oldPlatform, user: user) {
                agentPath = CommandArguments.text(CommandArguments.installedAgent(platform: newPlatform, user: user))
            }
        }
    }

    private var canAdd: Bool {
        AgentToken.isValid(id.trimmingCharacters(in: .whitespaces))
            && !host.trimmingCharacters(in: .whitespaces).isEmpty
            && undialablePart(host: host, user: user) == nil
            && !agentArgv.isEmpty
    }

    private var agentArgv: [String] {
        CommandArguments.parse(agentPath) ?? []
    }

    /// A Windows system always says which shell it has, so no client is left to guess.
    private var systemEntry: [String: Any] {
        var system: [String: Any] = [
            "id": platform.rawValue,
            "name": platform.defaultName,
            "platform": platform.rawValue,
            "agent": agentArgv
        ]
        if platform == .windows { system["shell"] = windowsShell }
        return system
    }

    private func add() {
        let machineId = id.trimmingCharacters(in: .whitespaces)
        var entry: [String: Any] = [
            "id": machineId,
            "name": name.isEmpty ? machineId : name,
            "endpoints": [[
                "id": "primary",
                "kind": "remote",
                "host": host.trimmingCharacters(in: .whitespaces)
            ] as [String: Any]],
            "systems": [systemEntry]
        ]
        if !user.trimmingCharacters(in: .whitespaces).isEmpty,
           var endpoints = entry["endpoints"] as? [[String: Any]] {
            endpoints[0]["user"] = user.trimmingCharacters(in: .whitespaces)
            entry["endpoints"] = endpoints
        }
        problem = model.editSetup("added the machine \(name.isEmpty ? machineId : name)") { root in
            try ControllerEditor.upsertMachine(entry, in: &root)
        }
        if problem == nil { onAdded(machineId) }
    }
}

// MARK: - Small forms

private struct EndpointForm: View {
    var machineName: String
    var systems: [SystemConfig]
    var save: ([String: Any]) -> Bool

    @State private var id = ""
    @State private var host = ""
    @State private var user = ""
    @State private var port = ""
    @State private var kind = "remote"
    @State private var system = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            LabelledField("Id", text: $id, placeholder: "lan")
            LabelledField("Host", text: $host, placeholder: "10.0.0.40")
            LabelledField("User", text: $user, placeholder: "me")
            LabelledField("Port", text: $port, placeholder: "22")

            HStack(alignment: .firstTextBaseline, spacing: 18) {
                Text("Kind").font(.subheadline).foregroundStyle(.secondary).frame(width: 140, alignment: .leading)
                Picker("Kind", selection: $kind) {
                    Text("On the LAN").tag("lan")
                    Text("Remote").tag("remote")
                }
                .labelsHidden().pickerStyle(.segmented).fixedSize()
            }

            if !systems.isEmpty {
                HStack(alignment: .firstTextBaseline, spacing: 18) {
                    Text("Only when running").font(.subheadline).foregroundStyle(.secondary)
                        .frame(width: 140, alignment: .leading)
                    Picker("Only when running", selection: $system) {
                        Text("Any system").tag("")
                        ForEach(systems) { entry in Text(entry.name).tag(entry.id) }
                    }
                    .labelsHidden().pickerStyle(.menu).fixedSize()
                }
            }

            Button("Add the address") {
                var entry: [String: Any] = [
                    "id": id.trimmingCharacters(in: .whitespaces),
                    "host": host.trimmingCharacters(in: .whitespaces),
                    "kind": kind
                ]
                if !user.isEmpty { entry["user"] = user.trimmingCharacters(in: .whitespaces) }
                if let number = Int(port.trimmingCharacters(in: .whitespaces)) { entry["port"] = number }
                if !system.isEmpty { entry["system"] = system }
                if save(entry) {
                    id = ""; host = ""; user = ""; port = ""; system = ""
                }
            }
            .disabled(!AgentToken.isValid(id.trimmingCharacters(in: .whitespaces))
                      || host.trimmingCharacters(in: .whitespaces).isEmpty || undialable != nil)

            if let undialable { QuietNote(text: "This app never dials \(undialable).") }
        }
    }

    private var undialable: String? { undialablePart(host: host, user: user, port: port) }
}

/// The part of an address typed into a form that this app would never dial, scan or pin (section
/// 15.6 of the contract), or nil when there is none or nothing has been typed yet. Saving one would
/// only add an address every call skips, so the forms refuse it.
private func undialablePart(host: String, user: String, port: String = "") -> String? {
    let host = host.trimmingCharacters(in: .whitespaces)
    let user = user.trimmingCharacters(in: .whitespaces)
    let port = port.trimmingCharacters(in: .whitespaces)
    guard !host.isEmpty else { return nil }
    if !port.isEmpty, Int(port) == nil { return "the port \"\(port)\"" }
    return Addresses.undialablePart(of: SSHTarget(host: host, user: user.isEmpty ? nil : user,
                                                  port: Int(port), identityFile: nil))
}

private struct SystemForm: View {
    /// The ids of systems whose command is waiting for approval here, which this form will not
    /// write over.
    var waiting: Set<String> = []
    var save: ([String: Any]) -> Bool

    @State private var id = ""
    @State private var name = ""
    @State private var platform = Platform.linux
    @State private var shell = ""
    @State private var restricted = false
    @State private var argv = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            LabelledField("Id", text: $id, placeholder: "windows")
            LabelledField("Name", text: $name, placeholder: "Windows 11")

            HStack(alignment: .firstTextBaseline, spacing: 18) {
                Text("Platform").font(.subheadline).foregroundStyle(.secondary).frame(width: 140, alignment: .leading)
                Picker("Platform", selection: $platform) {
                    ForEach(Platform.allCases, id: \.self) { value in Text(value.defaultName).tag(value) }
                }
                .labelsHidden().pickerStyle(.segmented).fixedSize()
            }

            HStack(alignment: .firstTextBaseline, spacing: 18) {
                Text("Shell").font(.subheadline).foregroundStyle(.secondary).frame(width: 140, alignment: .leading)
                Picker("Shell", selection: $shell) {
                    // A Windows system always names its shell: no client guesses one for it.
                    if platform != .windows { Text("What the platform implies").tag("") }
                    ForEach(["posix", "cmd", "powershell"], id: \.self) { value in Text(value).tag(value) }
                }
                .labelsHidden().pickerStyle(.menu).fixedSize()
            }

            Toggle(isOn: $restricted) {
                Text("The key for this system is restricted to a forced command")
            }
            .toggleStyle(.switch)

            LabelledField("Agent arguments", text: $argv, placeholder: "JSON array: one string per argument")

            if restricted {
                QuietNote(text: "A forced command never reaches a shell, so the command is written in the dispatcher's own grammar whatever the platform is. The arguments have to be exactly the interpreter and the agent's entry.")
            }

            Button("Add the system") {
                guard let words = CommandArguments.parse(argv) else { return }
                var entry: [String: Any] = [
                    "id": id.trimmingCharacters(in: .whitespaces),
                    "name": name.isEmpty ? platform.defaultName : name,
                    "platform": platform.rawValue,
                    "agent": words
                ]
                let chosen = platform == .windows && shell.isEmpty ? "cmd" : shell
                if !chosen.isEmpty { entry["shell"] = chosen }
                if restricted { entry["restricted"] = true }
                if save(entry) { id = ""; name = ""; shell = platform == .windows ? "cmd" : ""; argv = ""; restricted = false }
            }
            .disabled(!AgentToken.isValid(id.trimmingCharacters(in: .whitespaces)) || !argumentsFit || isWaiting)

            if isWaiting {
                QuietNote(text: "The command for \(id.trimmingCharacters(in: .whitespaces)) is waiting for approval on this device. Approve or revert it before changing it.")
            }
        }
        .onChange(of: platform) { _, newPlatform in
            // cmd is what a stock Windows OpenSSH runs, so it is what a new Windows system starts with.
            if newPlatform == .windows, shell.isEmpty { shell = "cmd" }
        }
    }

    private var isWaiting: Bool { waiting.contains(id.trimmingCharacters(in: .whitespaces)) }

    /// A restricted system the dispatcher could not take would be saved only to be refused on every
    /// command.
    private var argumentsFit: Bool {
        guard let words = CommandArguments.parse(argv) else { return false }
        return !restricted || words.count == 2
    }
}

/// One labelled field on the same left edge as every detail row in the app.
struct LabelledField: View {
    var label: String
    @Binding var text: String
    var placeholder: String

    init(_ label: String, text: Binding<String>, placeholder: String) {
        self.label = label
        self._text = text
        self.placeholder = placeholder
    }

    var body: some View {
        DetailRow(label: label) {
            TextField(placeholder, text: $text)
                .textFieldStyle(.roundedBorder)
                .frame(minWidth: 160, maxWidth: 360)
        }
    }
}

/// "a, b , c" to ["a", "b", "c"].
func splitList(_ text: String) -> [String] {
    text.split(separator: ",")
        .map { $0.trimmingCharacters(in: .whitespaces) }
        .filter { !$0.isEmpty }
}
