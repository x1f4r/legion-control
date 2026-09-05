import AppKit
import SwiftUI

/// Bounded agent reads, with the returned text available to inspect and save.
struct DiagnosticActions: View {
    var enabled: Bool
    var check: @MainActor () -> Void
    var deepCheck: @MainActor () -> Void
    var log: @MainActor () async -> [String]
    var bundle: @MainActor () async -> String
    @State private var loading: String?
    @State private var title = ""
    @State private var output: String?
    @State private var saveProblem: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            FlowRow(spacing: 10) {
                Button("Check agent", action: check)
                Button("Deep diagnostics", action: deepCheck)
                Button("Recent log") {
                    read("Recent agent log") { await log().joined(separator: "\n") }
                }
                Button("Export diagnostics") { read("Agent diagnostic bundle", action: bundle) }
            }
            .disabled(!enabled || loading != nil)
            if let loading { Text("Reading \(loading.lowercased())…").foregroundStyle(.secondary) }
        }
        .sheet(isPresented: Binding(get: { output != nil }, set: { if !$0 { output = nil } })) {
            VStack(alignment: .leading, spacing: 14) {
                Text(title).font(.title2)
                ScrollView([.vertical, .horizontal]) {
                    Text(output ?? "").font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                if let saveProblem { Text(saveProblem).foregroundStyle(.red) }
                HStack {
                    Button("Save…") { save() }
                    Spacer()
                    Button("Close") { output = nil }
                }
            }.padding(16).frame(minWidth: 360, idealWidth: 640, maxWidth: 760, minHeight: 300, idealHeight: 500, maxHeight: 580)
        }
    }

    private func read(_ title: String, action: @escaping @MainActor () async -> String) {
        loading = title
        Task { @MainActor in
            let result = await action()
            self.title = title
            self.output = result.isEmpty ? "The agent returned no log entries." : result
            self.saveProblem = nil
            self.loading = nil
        }
    }

    private func save() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = title == "Recent agent log" ? "legion-agent-log.txt" : "legion-agent-diagnostics.json"
        guard panel.runModal() == .OK, let url = panel.url, let output else { return }
        do { try Data(output.utf8).write(to: url, options: .atomic) }
        catch { saveProblem = error.localizedDescription }
    }
}
