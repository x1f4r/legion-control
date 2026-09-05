import SwiftUI

/// Available updates stay visible across sections without opening a dialog on their own.
struct AppUpdateNotice: View {
    let updates: AppUpdateModel
    @State private var showsDetails = false

    var body: some View {
        if let version = updates.availableVersion {
            HStack(spacing: 10) {
                Text("Legion Control \(version) is available")
                    .font(.callout)
                    .lineLimit(2)
                Spacer(minLength: 8)
                Button(updates.phase == .idle ? "Install" : updates.summary) { updates.install() }
                    .disabled(updates.phase.isWorking || updates.installBlockedReason != nil)
                    .help(updates.installBlockedReason ?? "Verify the signed release, install it, and restart Legion Control.")
                Button("Details") { showsDetails = true }
            }
            .controlSize(.small)
            .popover(isPresented: $showsDetails) {
                VStack(alignment: .leading, spacing: 12) {
                    Text("Legion Control \(version)").font(.headline)
                    if let blocked = updates.installBlockedReason { Text(blocked).foregroundStyle(.orange) }
                    if let notes = updates.releaseNotes { Text(notes).textSelection(.enabled) }
                    if let failure = updates.checkFailure { Text("Last check: \(failure)").foregroundStyle(.secondary) }
                    if let failure = updates.failure { Text(failure).foregroundStyle(.orange) }
                    Button("Check now") { updates.checkNow() }.disabled(updates.phase.isWorking)
                }.padding(18).frame(width: 360, alignment: .leading)
            }
        }
    }
}
