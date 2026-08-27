import Foundation

/// Whether the window should be on screen when the app starts.
///
/// Remembered rather than detected. See the note in AppDelegate for why every attempt to detect a
/// login launch was abandoned: none of the available signals actually distinguish one.
enum WindowState {
    private static let key = "WindowOpenAtLaunch"

    /// Defaults to true, so the very first launch shows something rather than looking like nothing
    /// happened.
    static var shouldOpenAtLaunch: Bool {
        UserDefaults.standard.object(forKey: key) as? Bool ?? true
    }

    static func remember(open: Bool) {
        UserDefaults.standard.set(open, forKey: key)
    }
}
