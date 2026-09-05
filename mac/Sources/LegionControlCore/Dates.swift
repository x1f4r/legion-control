import Foundation

/// Parses both ISO 8601 shapes the agent emits.
///
/// Some of its fields carry fractional seconds and some do not, and a parser configured for one
/// rejects the other outright. Trying both is the difference between a timestamp being shown and a
/// row quietly saying "not known" because of three digits.
///
/// Built on `Date.ISO8601FormatStyle`, which is a value type and therefore safe to hold in a `let`
/// and use from anywhere. `ISO8601DateFormatter` is a class, is not `Sendable`, and would have to be
/// locked around every call.
struct LenientISO8601: Sendable {
    private let withFraction = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
    private let withoutFraction = Date.ISO8601FormatStyle(includingFractionalSeconds: false)

    func date(from text: String?) -> Date? {
        guard let text, !text.isEmpty else { return nil }
        if let date = try? withFraction.parse(text) { return date }
        return try? withoutFraction.parse(text)
    }

    /// The form this app writes when it has to name a moment to the agent.
    func string(from date: Date) -> String {
        date.formatted(withoutFraction)
    }
}

extension ISO8601DateFormatter {
    /// The shared lenient parser. Named on `ISO8601DateFormatter` so the call sites read the way
    /// anyone would expect them to.
    static let lenient = LenientISO8601()
}

extension Date {
    /// "in 3 hours", "4 minutes ago". Used for expiry lines, where the absolute time is less useful
    /// than how long is left.
    func relativeDescription(from now: Date = Date()) -> String {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .full
        return formatter.localizedString(for: self, relativeTo: now)
    }
}
