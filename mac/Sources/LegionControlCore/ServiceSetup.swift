import Foundation
import Observation
import SwiftUI

struct ServiceConfigReply: Decodable, Sendable {
    var ok: Bool?
    var valid: Bool?
    var saved: Bool?
    var hash: String?
    var document: JSONAny?
    var templates: [Template]?
    var profiles: [AIProfile]?
    var changes: Changes?
    var warnings: [Issue]?
    var errors: [Issue]?
    var reasonCode: String?
    var message: String?
    struct Template: Decodable, Sendable, Identifiable {
        var id: String
        var name: String
        var description: String
        var service: JSONAny
    }
    struct AIProfile: Decodable, Sendable, Identifiable {
        var id: String
        var name: String
        var platform: String
        var detected: Bool
        var availability: String
        var message: String
        var updateMethod: String?
        var service: JSONAny?
        var canMonitor: Bool { detected && availability == "manual" && service != nil }
        var canAdd: Bool { (availability == "available" || canMonitor) && service != nil }
    }
    struct Changes: Decodable, Sendable {
        var added: [String]
        var removed: [String]
        var changed: [String]
        var summary: String {
            [added.isEmpty ? nil : "Add: \(added.joined(separator: ", "))",
             removed.isEmpty ? nil : "Remove: \(removed.joined(separator: ", "))",
             changed.isEmpty ? nil : "Change: \(changed.joined(separator: ", "))"].compactMap { $0 }.joined(separator: "\n")
        }
    }
    struct Issue: Decodable, Sendable { var path: String; var message: String }
}

@MainActor @Observable
final class ServiceSetupModel {
    var text = ""
    var expectedHash: String?
    var templates: [ServiceConfigReply.Template] = []
    var profiles: [ServiceConfigReply.AIProfile] = []
    var busy = false
    var problem: String?
    var preview: String?
    var validatedText: String?
    var restricted = false
    var saved = false
    let send: @MainActor (AgentRequest) async throws -> ServiceConfigReply
    init(send: @escaping @MainActor (AgentRequest) async throws -> ServiceConfigReply) { self.send = send }

    var canSave: Bool { !busy && !restricted && validatedText == text && validatedText != nil }
    var services: [(id: String, name: String)] {
        guard let root = try? parse(), let services = root["services"] as? [[String: Any]] else { return [] }
        return services.compactMap { service in
            guard let id = service["id"] as? String else { return nil }
            return (id, service["name"] as? String ?? id)
        }
    }
    func parse() throws -> [String: Any] {
        guard text.utf8.count <= 1_048_576,
              let root = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any] else {
            throw AgentBootstrap.Failure(message: "Configuration must be a JSON object of at most 1 MiB.")
        }
        return root
    }
    func load() async {
        busy = true; defer { busy = false }
        do {
            let reply = try await send(try .serviceConfig("get"))
            guard reply.ok == true, let hash = reply.hash, let document = reply.document?.value as? [String: Any] else {
                throw refusal(reply)
            }
            expectedHash = hash
            text = try Self.pretty(document)
            templates = reply.templates ?? []
            profiles = reply.profiles ?? []
            validatedText = nil; preview = nil; problem = nil; saved = false
        } catch { problem = error.localizedDescription }
    }
    func addTemplate(_ template: String, id: String, name: String) {
        do {
            guard AgentToken.isValidID(id), !services.contains(where: { $0.id == id }),
                  var service = templates.first(where: { $0.id == template })?.service.value as? [String: Any] else {
                throw AgentBootstrap.Failure(message: "Choose a template and a unique service id using letters, digits, dots, underscores or hyphens.")
            }
            var root = try parse()
            service["id"] = id; service["name"] = name.isEmpty ? id : name
            var services = root["services"] as? [[String: Any]] ?? []
            services.append(service); root["services"] = services
            if root["configVersion"] == nil { root["configVersion"] = 3 }
            text = try Self.pretty(root); problem = nil; saved = false
        } catch { problem = error.localizedDescription }
    }
    func rename(_ id: String, name: String) {
        do {
            var root = try parse()
            guard var services = root["services"] as? [[String: Any]],
                  let index = services.firstIndex(where: { $0["id"] as? String == id }) else { return }
            services[index]["name"] = name
            root["services"] = services
            text = try Self.pretty(root); saved = false
        } catch { problem = error.localizedDescription }
    }
    func addProfile(_ id: String) {
        do {
            guard let profile = profiles.first(where: { $0.id == id }), profile.canAdd,
                  var service = profile.service?.value as? [String: Any] else {
                throw AgentBootstrap.Failure(message: "This tool does not have an available managed profile.")
            }
            let serviceId = (service["id"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? profile.id
            guard AgentToken.isValidID(serviceId), !services.contains(where: { $0.id == serviceId }) else {
                throw AgentBootstrap.Failure(message: "This tool is already configured or its service id is invalid.")
            }
            service["id"] = serviceId
            var updates = service["updates"] as? [String: Any] ?? [:]
            updates["automatic"] = false; service["updates"] = updates
            var root = try parse()
            var services = root["services"] as? [[String: Any]] ?? []
            services.append(service); root["services"] = services
            if root["configVersion"] == nil { root["configVersion"] = 3 }
            text = try Self.pretty(root); saved = false; problem = nil
        } catch { problem = error.localizedDescription }
    }
    func validate() async { await request("validate") }
    func save() async {
        guard canSave else { return }
        await request("set")
    }
    private func request(_ verb: String) async {
        busy = true; defer { busy = false }
        do {
            guard let expectedHash else { throw AgentBootstrap.Failure(message: "Load the agent configuration first.") }
            let submitted = text
            let payload = try JSONSerialization.data(withJSONObject: ["expectedHash": expectedHash, "document": parse()])
            let reply = try await send(try .serviceConfig(verb, input: payload))
            guard reply.ok == true, reply.valid == true else { throw refusal(reply) }
            preview = (reply.changes?.summary ?? "") + (reply.warnings ?? []).map { "\n\($0.path): \($0.message)" }.joined()
            if preview?.isEmpty == true { preview = "No service changes. Other configuration fields were validated." }
            problem = nil
            if verb == "set" {
                guard reply.saved == true, let hash = reply.hash else {
                    throw AgentBootstrap.Failure(message: "The agent did not acknowledge saving the configuration.")
                }
                self.expectedHash = hash; validatedText = nil; saved = true
            } else { validatedText = submitted; saved = false }
        } catch { validatedText = nil; problem = error.localizedDescription }
    }
    private func refusal(_ reply: ServiceConfigReply) -> AgentBootstrap.Failure {
        if reply.reasonCode == "restricted" { restricted = true }
        return .init(message: ([reply.message ?? "The agent refused the service configuration."]
            + (reply.errors ?? []).map { "\($0.path): \($0.message)" }).joined(separator: "\n"))
    }
    static func pretty(_ root: [String: Any]) throws -> String {
        String(decoding: try JSONSerialization.data(withJSONObject: root, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]), as: UTF8.self)
    }
}

struct ServiceSetupButton: View {
    var enabled: Bool
    var restricted: Bool = false
    var send: @MainActor (AgentRequest) async throws -> ServiceConfigReply
    @State private var editor: ServiceSetupModel?
    var body: some View {
        Button("Service setup") { editor = ServiceSetupModel(send: send) }
            .disabled(!enabled || restricted)
            .help(restricted ? "Administrator access is required; restricted keys cannot edit service configuration." : "Configure services without running commands.")
            .sheet(isPresented: Binding(get: { editor != nil }, set: { if !$0 { editor = nil } })) {
                if let editor { ServiceSetupView(model: editor) }
            }
    }
}

private struct ServiceSetupView: View {
    @Bindable var model: ServiceSetupModel
    @Environment(\.dismiss) var dismiss
    @State private var template = ""
    @State private var newId = ""
    @State private var newName = ""
    @State private var selectedService = ""
    @State private var serviceName = ""
    @State private var profile = ""
    var body: some View {
        ScrollView {
        VStack(alignment: .leading, spacing: 14) {
            Text("Service setup").font(.title2)
            DisclosureGroup("About service setup") {
                Text("Administrator configuration on this machine. Validate the preview, then save. Saving runs no service commands.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            if model.expectedHash != nil {
                if !model.profiles.isEmpty {
                    DisclosureGroup("Add AI tool") {
                        VStack(alignment: .leading, spacing: 8) {
                            Picker("Tool", selection: $profile) {
                                Text("Choose a tool").tag("")
                                ForEach(model.profiles) { Text($0.name).tag($0.id) }
                            }
                            if let selected = model.profiles.first(where: { $0.id == profile }) {
                                Text(selected.message).font(.caption).foregroundStyle(.secondary)
                                if let method = selected.updateMethod { Text("Updates: \(method)").font(.caption) }
                                Button(selected.canMonitor ? "Monitor" : "Add tool") { model.addProfile(profile) }.disabled(!selected.canAdd)
                            }
                        }.padding(.top, 8)
                    }
                }
                VStack(alignment: .leading, spacing: 8) {
                    Picker("Service", selection: $selectedService) {
                        Text("Choose a service").tag("")
                        ForEach(model.services, id: \.id) { Text($0.name).tag($0.id) }
                    }.onChange(of: selectedService) { _, id in serviceName = model.services.first { $0.id == id }?.name ?? "" }
                    if !selectedService.isEmpty {
                        TextField("Name", text: $serviceName)
                        Button("Apply name") { model.rename(selectedService, name: serviceName) }
                    }
                }
                DisclosureGroup("Add from template") {
                    VStack(alignment: .leading, spacing: 10) {
                        Picker("Template", selection: $template) {
                            Text("Choose a template").tag("")
                            ForEach(model.templates) { Text($0.name).tag($0.id) }
                        }
                        if let selected = model.templates.first(where: { $0.id == template }) {
                            Text(selected.description).font(.caption).foregroundStyle(.secondary)
                        }
                        VStack(alignment: .leading, spacing: 8) {
                            TextField("Service id", text: $newId)
                            TextField("Name", text: $newName)
                            Button("Add draft") { model.addTemplate(template, id: newId, name: newName) }
                                .disabled(template.isEmpty || newId.isEmpty)
                        }
                    }.padding(.top, 8)
                }
                Text("Configuration").font(.headline)
                TextEditor(text: $model.text).font(.system(.caption, design: .monospaced))
                    .frame(minHeight: 240)
                    .disabled(model.busy)
                if let preview = model.preview, model.validatedText == model.text || model.saved {
                    Text(preview).textSelection(.enabled)
                }
            }
            if let problem = model.problem { Text(problem).foregroundStyle(.orange).textSelection(.enabled) }
            if model.saved { Text("Saved. No service commands were run.").foregroundStyle(.secondary) }
            FlowRow(spacing: 8) {
                Button("Reload") { Task { await model.load() } }.disabled(model.busy)
                Button("Validate preview") { Task { await model.validate() } }.disabled(model.busy || model.expectedHash == nil || model.restricted)
                Button("Save") { Task { await model.save() } }.disabled(!model.canSave)
                if model.busy { ProgressView().controlSize(.small) }
                Button("Close") { dismiss() }
            }
        }.padding(16)
        }.frame(minWidth: 360, idealWidth: 720, maxWidth: 820, minHeight: 360, idealHeight: 600, maxHeight: 700)
            .task { await model.load() }
    }
}
