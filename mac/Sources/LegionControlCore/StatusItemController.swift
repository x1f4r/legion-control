import AppKit
import SwiftUI

/// What the menu bar needs from the app around it.
@MainActor
protocol MenuBarHost: AnyObject {
    var isWindowVisible: Bool { get }
    func showWindow()
    func toggleWindow()
}

/// The menu bar icon and the panel under it.
///
/// This is the part of the app that is always there, so it is also the part that must cost nothing.
/// It keeps no timer: the icon is redrawn when the model says something changed, and the two
/// machines are only ever read while the panel is actually on screen.
///
/// The panel is a popover rather than a menu on purpose. A menu can hold a list of titles and
/// nothing else, and everything this app has to say (which system is up, which build, whether a
/// turn is running, what is staged on this Mac) had to be squeezed into greyed out title lines.
/// A popover draws the same SwiftUI the window uses, so switches, verdicts and confirmations all
/// live under the icon and the window becomes optional.
@MainActor
final class StatusItemController: NSObject, NSPopoverDelegate {
    private let model: AppModel
    private weak var host: (any MenuBarHost)?
    private let statusItem: NSStatusItem
    private let popover = NSPopover()
    private let panel = MenuBarPanelState()

    /// An invisible window that stays where the icon was.
    ///
    /// A popover is positioned relative to a view and follows that view around. The menu bar on
    /// this Mac hides itself a few seconds after the pointer leaves it, which slides the status
    /// item's window off the top of the screen, and the first time the popover re-laid itself out
    /// after that (a reading landing is enough) it followed the icon off screen and was clamped
    /// back into the top left corner mid-read. So the popover is anchored to a window of our own,
    /// put exactly over the icon at the moment of the click, that goes nowhere until it closes.
    private let anchor: NSWindow

    /// When the panel last closed. A transient popover closes on the mouse down of a click outside
    /// it, and a click on the icon is outside it, so the same click would arrive here a moment later
    /// and open the panel straight back up. Anything that lands this soon after a close is that click.
    private var panelClosedAt: Date = .distantPast

    init(model: AppModel, host: any MenuBarHost) {
        self.model = model
        self.host = host
        self.statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        self.anchor = Self.makeAnchor()
        super.init()

        panel.openWindow = { [weak self] in self?.openWindow() }
        panel.quit = { NSApp.terminate(nil) }

        configurePopover()
        configureButton()
        redraw()
    }

    // MARK: - The icon

    private func configureButton() {
        guard let button = statusItem.button else { return }
        button.target = self
        button.action = #selector(buttonClicked(_:))
        // Both buttons open the same thing. The right edge has to be asked for by name or a right
        // click never reaches the action at all.
        button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        button.imagePosition = .imageOnly
    }

    /// Any click opens the panel. Option-click is the one shortcut kept for the window, for the
    /// times the detail there is wanted without going through the panel first.
    @objc private func buttonClicked(_ sender: NSStatusBarButton) {
        if NSApp.currentEvent?.modifierFlags.contains(.option) == true {
            host?.toggleWindow()
            return
        }
        togglePanel()
    }

    /// Redraws the icon. Called when the model changes, never on a schedule. The panel itself
    /// observes the model directly and needs no help.
    func redraw() {
        guard let button = statusItem.button else { return }
        let image = NSImage(
            systemSymbolName: model.menuBarSymbol,
            accessibilityDescription: "Legion Control: \(model.menuBarDescription)"
        )
        image?.isTemplate = true
        button.image = image?.withSymbolConfiguration(
            NSImage.SymbolConfiguration(pointSize: 14, weight: .regular)
        ) ?? image
        button.toolTip = "Legion Control. \(model.menuBarDescription)."
    }

    // MARK: - The panel

    private static func makeAnchor() -> NSWindow {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1, height: 1),
            styleMask: .borderless,
            backing: .buffered,
            defer: false
        )
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = false
        window.ignoresMouseEvents = true
        window.level = .statusBar
        window.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle, .fullScreenAuxiliary]
        window.isReleasedWhenClosed = false
        window.isExcludedFromWindowsMenu = true
        return window
    }

    private func configurePopover() {
        let hosting = NSHostingController(rootView: MenuBarPanel(model: model, panel: panel))
        // The panel grows and shrinks as the machines change state and as questions come and go,
        // and the popover follows the hosting controller's preferred size when it is told to.
        hosting.sizingOptions = [.preferredContentSize]
        popover.contentViewController = hosting
        popover.behavior = .transient
        popover.animates = true
        popover.delegate = self
    }

    var isPanelShown: Bool { popover.isShown }

    func togglePanel() {
        if popover.isShown {
            closePanel()
        } else if Date().timeIntervalSince(panelClosedAt) > 0.3 {
            showPanel()
        }
    }

    /// Shows the panel if it is not already up. Safe to call from anywhere, including from a state
    /// change that arrives while it is open.
    func showPanel() {
        guard let button = statusItem.button, let bar = button.window, let anchorView = anchor.contentView,
              !popover.isShown else { return }
        // Read from launchd now rather than on every draw; System Settings can change it behind us.
        panel.rereadLogin()
        panel.isWindowVisible = host?.isWindowVisible == true
        // Activate first, then show. A transient popover only notices clicks outside itself while
        // its app is active, and activating after the show closes it again on the spot.
        NSApp.activate()
        anchor.setFrame(bar.convertToScreen(button.convert(button.bounds, to: nil)), display: false)
        anchor.orderFrontRegardless()
        popover.show(relativeTo: anchorView.bounds, of: anchorView, preferredEdge: .minY)
        button.highlight(true)
        // The popover hands key focus to the first control it finds, which paints a focus ring
        // around Wake or Sleep every time the panel opens as if it had been tabbed to. Nobody asked
        // for that; the keyboard gets it back the moment Tab is pressed.
        popover.contentViewController?.view.window?.makeFirstResponder(nil)
        // The panel is a viewer of the machines exactly as the window is, so it polls while it is
        // up. The first reading is asked for right away, and the last one is drawn until it lands.
        model.viewerAppeared()
    }

    func closePanel() {
        popover.performClose(nil)
    }

    private func openWindow() {
        closePanel()
        host?.showWindow()
    }

    // MARK: - NSPopoverDelegate

    func popoverDidClose(_ notification: Notification) {
        panelClosedAt = Date()
        statusItem.button?.highlight(false)
        anchor.orderOut(nil)
        guard host?.isWindowVisible != true else { return }
        // Nothing left is looking, so nothing is read until something is.
        model.viewerDisappeared()
        // A question that was open when the panel was dismissed was not answered, and closing the
        // panel is the plainest "no" there is. Leaving it would ambush the next open with a stale
        // offer to reboot or to interrupt a turn that may long since have finished.
        model.dialog = nil
    }
}
