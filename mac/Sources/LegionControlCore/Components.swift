import SwiftUI

/// One label plus its value, laid out on a shared left edge. Deliberately flat: no boxes, no frames,
/// no rounded backgrounds anywhere in this app.
struct DetailRow<Content: View>: View {
    var label: String
    @ViewBuilder var content: Content

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .firstTextBaseline, spacing: 14) {
                Text(label).font(.subheadline).foregroundStyle(.secondary)
                    .frame(width: 120, alignment: .leading)
                content.font(.body)
                Spacer(minLength: 0)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(label).font(.caption).foregroundStyle(.secondary)
                content.font(.body).frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

extension DetailRow where Content == Text {
    init(_ label: String, _ value: String) {
        self.init(label: label) { Text(value) }
    }
}

/// The heading at the top of a section page. The window title bar is empty on purpose, so this is
/// the only thing that names what you are looking at.
struct PageHeading: View {
    var title: String
    var note: String?

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text(title).font(.system(size: 22, weight: .semibold))
                if let note { Text(note).font(.callout).foregroundStyle(.secondary) }
                Spacer(minLength: 0)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.system(size: 22, weight: .semibold))
                if let note { Text(note).font(.callout).foregroundStyle(.secondary) }
            }
        }.padding(.bottom, 16)
    }
}

/// The quiet line under a row of buttons that says why one of them cannot be pressed, or what
/// pressing it would do. Never a badge, never coloured: it is an aside, not a warning.
struct QuietNote: View {
    var text: String

    var body: some View {
        Text(text)
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// A button that is filled only while it has something to do, and an ordinary quiet button the rest
/// of the time. Prominence here means "there is a newer build waiting", so it must never be the
/// permanent look of a button that would do nothing.
struct PrimaryActionButton: View {
    var title: String
    var isHighlighted: Bool
    var isEnabled: Bool
    var action: () -> Void

    var body: some View {
        Group {
            if isHighlighted {
                Button(title, action: action).buttonStyle(.borderedProminent)
            } else {
                Button(title, action: action).buttonStyle(.bordered)
            }
        }
        .disabled(!isEnabled)
        .animation(.easeOut(duration: 0.2), value: isHighlighted)
    }
}

/// A section heading with the hairline rule that separates it from what came before.
struct SectionHeading: View {
    var title: String
    var note: String?
    /// The first section on screen sets this to false. With no title above it, a rule there would sit
    /// directly under the traffic lights and read as window chrome rather than as a separator.
    var showsRule: Bool = true

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if showsRule { Divider() }
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(title)
                    .font(.headline)
                if let note {
                    Text(note)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        // More air between sections than inside them, which is what makes the grouping readable
        // without drawing a single box.
        .padding(.top, showsRule ? 30 : 0)
        .padding(.bottom, 14)
    }
}

/// A small symbol with plain text beside it. Status is never a badge or a capsule.
struct StatusText: View {
    var symbol: String
    var text: String
    var tint: Color = .secondary

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .imageScale(.medium)
            Text(text)
        }
    }
}

/// Version strings poll every fifteen seconds, so they get a fixed-width face and do not jitter.
struct VersionText: View {
    var value: String?
    var placeholder: String

    var body: some View {
        Text(value ?? placeholder)
            .font(.system(.body, design: .monospaced))
            .monospacedDigit()
            .foregroundStyle(value == nil ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
            .textSelection(.enabled)
    }
}

extension ISO8601DateFormatter {
    /// The agent stamps its state file with fractional seconds, which the default parser rejects.
    /// Main actor isolated because it is only ever read while drawing, and the type is not Sendable.
    @MainActor
    static let withFractional: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()
}
