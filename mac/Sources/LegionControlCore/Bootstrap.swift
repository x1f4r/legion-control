import AppKit

/// A plain AppKit entry point rather than a SwiftUI App.
///
/// The app has to survive its window closing, drop out of the Dock while it is away and come back
/// when the menu bar icon is clicked. All three are decided by the window delegate and the activation
/// policy, so AppKit owns the lifecycle and SwiftUI is hosted inside the window it makes.
@MainActor
public enum LegionControlMain {
    // NSApplication holds its delegate weakly, so it has to be kept alive from here.
    private static let delegate = AppDelegate()

    public static func main() {
        let app = NSApplication.shared
        app.delegate = delegate
        // Starts as a normal app with a Dock icon and a window. It only becomes an accessory once the
        // user puts the window away, which keeps launching from Raycast or Spotlight unsurprising.
        app.setActivationPolicy(.regular)
        app.run()
    }
}
