import AppKit
import SwiftUI

/// Owns the one window, the menu bar item and the activation policy.
///
/// The window is an AppKit window on purpose. Closing it has to mean "put it away", not "tear the
/// scene down", and the app has to be able to leave the Dock and come back at exactly those two
/// moments. Both of those are window delegate business, so the window is built here and SwiftUI is
/// hosted inside it.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, MenuBarHost {
    let model = AppModel()

    private static let frameAutosaveName = "LegionControlWindow"

    private var window: NSWindow?
    private var statusItemController: StatusItemController?
    /// Whether the window was on screen when the app was hidden, so unhiding puts back exactly the
    /// state that was there before and nothing else.
    private var windowWasVisibleBeforeHide = false

    // MARK: - Lifecycle

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = MainMenu.build()

        model.onStateChange = { [weak self] in
            guard let self else { return }
            // A question the app raises for itself, rather than one the user asked for, has to be
            // drawn somewhere. A boot started from the panel can come back deferred after the panel
            // was closed, so the panel is brought back rather than leaving a question nobody can see
            // or answer. The window draws it too when it is up, but it is never opened for this: the
            // whole point of the panel is that the window is optional.
            if self.model.dialog != nil, !self.isWindowVisible { self.statusItemController?.showPanel() }
            self.statusItemController?.redraw()
        }
        statusItemController = StatusItemController(model: model, host: self)
        makeWindow()

        // Come up the way it was left. If the window was put away to the menu bar last time, it
        // stays away; if it was on screen, it comes back.
        //
        // This is deliberately a remembered state rather than an attempt to detect a login launch.
        // There is no API for that, and every plausible signal turned out to be wrong when actually
        // checked on this machine: the parent process is launchd either way, XPC_SERVICE_NAME is
        // `application.<bundle id>.<n>.<n>` for a plain `open -a` as well, and NSApp.isActive is
        // still false this early even for a manual launch. Guessing wrong in one direction pops a
        // window in your face at login, and in the other direction makes the app look broken when
        // you launch it. Remembering is deterministic and needs no guess at all.
        //
        // It also lands on the behaviour you want for a login item without special casing it: to run
        // this at login you close the window to the menu bar, so that is the state it restores. And
        // because the app is then already running, opening it from Raycast or Spotlight arrives as
        // applicationShouldHandleReopen, which brings the window straight back.
        NSApp.setActivationPolicy(.accessory)
        if WindowState.shouldOpenAtLaunch { showWindow() }
    }

    /// The menu bar item is the app. Closing the window is not a reason to go away.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Clicking the Dock icon, or opening the app again from Raycast or Spotlight while it is already
    /// running, brings the window back rather than doing nothing.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showWindow()
        return true
    }

    /// Command-H, Hide Others from another app, or anything else that hides this one. The window ends
    /// up exactly as far off screen as closing it would put it, so it has to stop costing the same
    /// thing: without this the fifteen second poll keeps running, and an ssh round trip every fifteen
    /// seconds for a window nobody can see is the one thing this app is not allowed to do.
    ///
    /// willHide rather than didHide, because by the time didHide arrives the windows are already
    /// ordered out and there is no way left to tell whether one of them had been on screen.
    func applicationWillHide(_ notification: Notification) {
        windowWasVisibleBeforeHide = isWindowVisible
        model.viewerDisappeared()
    }

    /// Only the window that was put away by the hide comes back. An app that was hidden while its
    /// window was already closed unhides with nothing on screen, and must not start polling for it.
    func applicationDidUnhide(_ notification: Notification) {
        guard windowWasVisibleBeforeHide else { return }
        windowWasVisibleBeforeHide = false
        model.viewerAppeared()
        Task { @MainActor in await model.refreshIfNeeded() }
    }

    func applicationWillTerminate(_ notification: Notification) {
        model.stopPolling()
    }

    // MARK: - The window

    private func makeWindow() {
        let window = NSWindow(
            // Wide enough for the sidebar and a detail pane that does not wrap the button rows.
            contentRect: NSRect(x: 0, y: 0, width: 880, height: 720),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.title = "Legion Control"
        // The layout carries its own heading, so the system title bar would only say the same words
        // twice. The traffic lights still float over the content, which the header padding allows for.
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.isReleasedWhenClosed = false
        window.contentMinSize = NSSize(width: 700, height: 520)
        window.contentView = NSHostingView(rootView: ContentView(model: model))
        window.delegate = self
        // setFrameAutosaveName restores the saved frame as soon as it is set, so centring
        // unconditionally afterwards would throw the user's own placement away on every launch.
        // Only place the window when there is nothing remembered for it.
        let hasSavedFrame = UserDefaults.standard.string(forKey: "NSWindow Frame \(Self.frameAutosaveName)") != nil
        window.setFrameAutosaveName(Self.frameAutosaveName)
        if !hasSavedFrame { window.center() }
        self.window = window
    }

    var isWindowVisible: Bool {
        window?.isVisible == true
    }

    func showWindow() {
        guard let window else { return }
        WindowState.remember(open: true)
        NSApp.setActivationPolicy(.regular)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate()
        model.viewerAppeared()
        // Whatever is on screen is from the last time anything asked, which may be hours ago.
        Task { @MainActor in await model.refreshIfNeeded() }
    }

    func hideWindow() {
        WindowState.remember(open: false)
        window?.orderOut(nil)
        model.viewerDisappeared()
        // Out of the Dock and out of the app switcher. The menu bar item is the way back.
        NSApp.setActivationPolicy(.accessory)
        statusItemController?.redraw()
    }

    func toggleWindow() {
        if isWindowVisible, NSApp.isActive {
            hideWindow()
        } else {
            showWindow()
        }
    }

    // MARK: - NSWindowDelegate

    /// The close button hides the window. Nothing is destroyed, so coming back is instant and the
    /// state the app has already read is still there.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        hideWindow()
        return false
    }

    /// A minimised window is not being read, so it does not get polled either.
    func windowDidMiniaturize(_ notification: Notification) {
        model.viewerDisappeared()
    }

    func windowDidDeminiaturize(_ notification: Notification) {
        model.viewerAppeared()
        Task { @MainActor in await model.refreshIfNeeded() }
    }
}

/// The menu bar menu for the app itself. Built by hand because the app does not use a SwiftUI scene,
/// and because a window without Copy or Quit is a broken window.
@MainActor
enum MainMenu {
    static func build() -> NSMenu {
        let main = NSMenu()

        let appItem = NSMenuItem()
        main.addItem(appItem)
        let app = NSMenu()
        app.addItem(withTitle: "About Legion Control",
                    action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: "Hide Legion Control",
                    action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        let hideOthers = app.addItem(withTitle: "Hide Others",
                                     action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        app.addItem(withTitle: "Show All",
                    action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: "Quit Legion Control",
                    action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = app

        let editItem = NSMenuItem()
        main.addItem(editItem)
        let edit = NSMenu(title: "Edit")
        edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        edit.addItem(.separator())
        edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = edit

        let windowItem = NSMenuItem()
        main.addItem(windowItem)
        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(withTitle: "Minimize",
                           action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowMenu.addItem(.separator())
        // Close hides the window, the same as the close button: windowShouldClose sees both.
        windowMenu.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        windowItem.submenu = windowMenu
        NSApp.windowsMenu = windowMenu

        return main
    }
}
