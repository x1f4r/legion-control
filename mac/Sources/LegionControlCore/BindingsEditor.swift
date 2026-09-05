import SwiftUI

struct BindingsEditor: View {
    let model: AppModel
    @State private var deviceName = ""
    @State private var machine = ""
    @State private var system = ""
    @State private var argv = ""
    @State private var identity = ""
    @State private var overrideMachine = ""
    @State private var alias = ""
    @State private var machineIdentity = ""
    @State private var problem: String?
    @State private var notifications = Notifications.isEnabled

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("These settings stay on this device and are never published with the setup.")
            LabelledField("Device name", text: $deviceName, placeholder: model.bindings.bindings.effectiveDeviceName)
            Picker("This device is", selection: $machine) {
                Text("A separate controller").tag("")
                ForEach(model.config.machines) { Text($0.name).tag($0.id) }
            }
            if let selected = model.config.config?.machine(id: machine) {
                Picker("Local system", selection: $system) {
                    Text("Read from agent").tag("")
                    ForEach(selected.systems) { Text($0.name).tag($0.id) }
                }
                LabelledField("Local agent arguments", text: $argv, placeholder: "[\"/absolute/node\", \"/absolute/base/bin/launcher.mjs\"]")
                Text("Enter a JSON array, one string per argument. With an agent command, this machine is controlled locally without SSH.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            LabelledField("Default SSH key", text: $identity, placeholder: AppPaths.defaultIdentityFile.path)
            Toggle("Notify when an operation finishes", isOn: $notifications)
                .onChange(of: notifications) { _, enabled in Notifications.isEnabled = enabled }
            Button("Save device settings") {
                if !argv.isEmpty, CommandArguments.parse(argv) == nil {
                    problem = "The local agent command must be a JSON array of nonempty strings."
                    return
                }
                problem = model.bindings.update {
                    $0.deviceName = deviceName.isEmpty ? nil : deviceName
                    $0.selfBinding = machine.isEmpty ? nil : .init(machine: machine, system: system.isEmpty ? nil : system)
                    $0.localAgent = argv.isEmpty ? nil : .init(argv: CommandArguments.parse(argv) ?? [])
                    $0.identityFile = identity.isEmpty ? nil : identity
                }
            }
            Divider().padding(.vertical, 10)
            Picker("SSH override for", selection: $overrideMachine) {
                Text("Choose a machine").tag("")
                ForEach(model.config.machines) { Text($0.name).tag($0.id) }
            }.onChange(of: overrideMachine) { _, id in
                let binding = model.bindings.bindings.machines?[id]
                alias = binding?.sshAlias ?? ""
                machineIdentity = binding?.identityFile ?? ""
            }
            if !overrideMachine.isEmpty {
                LabelledField("Private SSH alias", text: $alias, placeholder: "Optional alias from your SSH config")
                LabelledField("SSH key override", text: $machineIdentity, placeholder: "Use the default key")
                Text("A private alias is tried before the shared endpoints and uses your configured SSH behavior.")
                    .font(.caption).foregroundStyle(.secondary)
                Button("Save SSH override") {
                    problem = model.bindings.update {
                        var binding = $0.machines?[overrideMachine] ?? .init()
                        binding.sshAlias = alias.isEmpty ? nil : alias
                        binding.identityFile = machineIdentity.isEmpty ? nil : machineIdentity
                        if $0.machines == nil { $0.machines = [:] }
                        $0.machines?[overrideMachine] = binding
                    }
                }
            }
            if let problem { Text(problem).foregroundStyle(.orange) }
        }.onAppear {
            let current = model.bindings.bindings
            deviceName = current.deviceName ?? ""
            machine = current.selfBinding?.machine ?? ""
            system = current.selfBinding?.system ?? ""
            argv = current.localAgent.map { CommandArguments.text($0.argv) } ?? ""
            identity = current.identityFile ?? ""
        }
    }
}
