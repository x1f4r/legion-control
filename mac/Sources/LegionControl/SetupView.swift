import SwiftUI

/// What the window shows when there is nothing to control yet.
///
/// This app knows no machines until a file names some, so a fresh install has to say what the file
/// is, where it goes and how to start it. One paragraph, the path, and the two buttons that are the
/// whole of the answer: write the example, then open it.
struct SetupView: View {
    let model: AppModel

    @State private var problem: String?

    private var store: ConfigStore { model.config }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                PageHeading(title: "Nothing configured yet")

                Text("Legion Control reads one file to learn which machines exist, how to reach them and which systems each of them can boot into. Write the example, edit it to describe your own machines, and the window fills in as soon as the file is saved.")
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.bottom, 20)

                DetailRow(label: "Config file") {
                    Text(store.path)
                        .font(.system(.callout, design: .monospaced))
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }

                SectionHeading(title: "Get started")

                HStack(spacing: 10) {
                    PrimaryActionButton(
                        title: "Write example config",
                        isHighlighted: store.isMissing,
                        isEnabled: store.isMissing
                    ) { problem = model.writeExampleConfig() }

                    Button("Open in editor") { model.openConfigInEditor() }
                        .disabled(store.isMissing)
                }

                QuietNote(text: store.isMissing
                    ? "There is no file at that path yet. Writing the example creates it and never overwrites anything."
                    : "The file is read again the moment it is saved. Nothing has to be restarted.")
                    .padding(.top, 12)

                if let problem {
                    Text(problem)
                        .font(.callout)
                        .foregroundStyle(.orange)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, 10)
                }

                if let parseProblem = store.problem {
                    SectionHeading(title: "The file could not be read")

                    Text(parseProblem)
                        .font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(.horizontal, 32)
            .padding(.top, 30)
            .padding(.bottom, 30)
            .frame(maxWidth: 620, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
