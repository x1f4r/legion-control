import Foundation
import UIKit
import WebKit

/// Origin-scoped WKWebView bridge implementing Legion's `immersive(on, { orientation })` and
/// `keepScreenOn(on)` on iOS and iPadOS using `TaoUIViewController`'s dynamic properties.
@objcMembers
final class LegionImmersiveBridge: NSObject, WKScriptMessageHandler {
  static let shared = LegionImmersiveBridge()

  private static let handlerName = "legionImmersive"
  private let attachedWebViews = NSHashTable<WKWebView>.weakObjects()
  private var observersInstalled = false
  private var immersiveActive = false
  private var orientationLocked = false
  private var attachAttempts = 0

  func install() {
    DispatchQueue.main.async { [weak self] in
      guard let self else { return }
      if !self.observersInstalled {
        self.observersInstalled = true
        let nc = NotificationCenter.default
        nc.addObserver(
          self,
          selector: #selector(self.handleWindowEvent),
          name: UIWindow.didBecomeKeyNotification,
          object: nil
        )
        nc.addObserver(
          self,
          selector: #selector(self.handleWindowEvent),
          name: UIApplication.didBecomeActiveNotification,
          object: nil
        )
        nc.addObserver(
          self,
          selector: #selector(self.handleWillTerminate),
          name: UIApplication.willTerminateNotification,
          object: nil
        )
      }
      self.attachIfReady()
      self.scheduleInitialAttachPass()
    }
  }

  private func scheduleInitialAttachPass() {
    attachAttempts = 0
    retryAttachSoon()
  }

  private func retryAttachSoon() {
    guard attachAttempts < 30 else { return }
    attachAttempts += 1
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in
      guard let self else { return }
      if !self.attachIfReady() {
        self.retryAttachSoon()
      }
    }
  }

  @objc private func handleWindowEvent() {
    attachIfReady()
  }

  @objc private func handleWillTerminate() {
    UIApplication.shared.isIdleTimerDisabled = false
  }

  @discardableResult
  private func attachIfReady() -> Bool {
    var attachedAny = false
    for window in allApplicationWindows() {
      guard let rootView = window.rootViewController?.view else { continue }
      for webView in findWebViews(in: rootView) {
        if !attachedWebViews.contains(webView) {
          let controller = webView.configuration.userContentController
          controller.removeScriptMessageHandler(forName: Self.handlerName)
          controller.add(self, name: Self.handlerName)
          attachedWebViews.add(webView)
        }
        attachedAny = true
      }
    }
    return attachedAny
  }

  private func allApplicationWindows() -> [UIWindow] {
    var result: [UIWindow] = []
    for scene in UIApplication.shared.connectedScenes {
      if let windowScene = scene as? UIWindowScene {
        result.append(contentsOf: windowScene.windows)
      }
    }
    return result
  }

  private func activeWindow() -> UIWindow? {
    let windows = allApplicationWindows()
    if let key = windows.first(where: { $0.isKeyWindow }) {
      return key
    }
    return windows.first
  }

  private func findWebViews(in view: UIView) -> [WKWebView] {
    var found: [WKWebView] = []
    if let webView = view as? WKWebView {
      found.append(webView)
    }
    for child in view.subviews {
      found.append(contentsOf: findWebViews(in: child))
    }
    return found
  }

  func userContentController(
    _ userContentController: WKUserContentController,
    didReceive message: WKScriptMessage
  ) {
    guard message.name == Self.handlerName else { return }
    guard message.frameInfo.isMainFrame,
          message.frameInfo.securityOrigin.protocol == "tauri",
          message.frameInfo.securityOrigin.host == "localhost" else {
      return
    }
    guard let body = message.body as? [String: Any],
          let type = body["type"] as? String else {
      return
    }

    switch type {
    case "immersive":
      let on = (body["on"] as? Bool) ?? false
      let orientation = (body["orientation"] as? String) ?? "none"
      applyImmersive(on: on, orientation: orientation)
    case "keep_screen_on":
      let on = (body["on"] as? Bool) ?? false
      UIApplication.shared.isIdleTimerDisabled = on
    default:
      break
    }
  }

  private func defaultOrientationMask() -> UIInterfaceOrientationMask {
    if UIDevice.current.userInterfaceIdiom == .pad {
      return .all
    }
    return .allButUpsideDown
  }

  private func applyImmersive(on: Bool, orientation: String) {
    guard let window = activeWindow(),
          let vc = window.rootViewController else {
      return
    }

    let normalized = orientation.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    let defaultMask = defaultOrientationMask()
    let targetMask: UIInterfaceOrientationMask
    let shouldLockOrientation: Bool

    if on {
      switch normalized {
      case "landscape":
        targetMask = .landscape
        shouldLockOrientation = true
      case "portrait":
        targetMask = .portrait
        shouldLockOrientation = true
      default:
        targetMask = defaultMask
        shouldLockOrientation = false
      }
    } else {
      targetMask = defaultMask
      shouldLockOrientation = false
    }

    let wasLocked = orientationLocked
    immersiveActive = on
    orientationLocked = on && shouldLockOrientation

    if vc.responds(to: NSSelectorFromString("setPrefersStatusBarHidden:")) {
      vc.setValue(NSNumber(value: on), forKey: "prefersStatusBarHidden")
    }
    if vc.responds(to: NSSelectorFromString("setPrefersHomeIndicatorAutoHidden:")) {
      vc.setValue(NSNumber(value: on), forKey: "prefersHomeIndicatorAutoHidden")
    }
    let deferredEdges: UIRectEdge = on ? .all : []
    if vc.responds(to: NSSelectorFromString("setPreferredScreenEdgesDeferringSystemGestures:")) {
      vc.setValue(
        NSNumber(value: deferredEdges.rawValue),
        forKey: "preferredScreenEdgesDeferringSystemGestures"
      )
    }
    if vc.responds(to: NSSelectorFromString("setSupportedInterfaceOrientations:")) {
      vc.setValue(NSNumber(value: targetMask.rawValue), forKey: "supportedInterfaceOrientations")
    }

    if #available(iOS 16.0, *) {
      vc.setNeedsUpdateOfSupportedInterfaceOrientations()
      if let scene = window.windowScene {
        if on && shouldLockOrientation {
          scene.requestGeometryUpdate(.iOS(interfaceOrientations: targetMask)) { _ in }
        } else if !on && wasLocked {
          let restoreMask: UIInterfaceOrientationMask =
            UIDevice.current.orientation.isLandscape ? defaultMask : .portrait
          scene.requestGeometryUpdate(.iOS(interfaceOrientations: restoreMask)) { _ in
            vc.setNeedsUpdateOfSupportedInterfaceOrientations()
          }
        }
      }
    } else {
      if on && normalized == "landscape" {
        UIDevice.current.setValue(
          UIInterfaceOrientation.landscapeRight.rawValue,
          forKey: "orientation"
        )
      } else if on && normalized == "portrait" {
        UIDevice.current.setValue(
          UIInterfaceOrientation.portrait.rawValue,
          forKey: "orientation"
        )
      } else if !on && wasLocked && !UIDevice.current.orientation.isLandscape {
        UIDevice.current.setValue(
          UIInterfaceOrientation.portrait.rawValue,
          forKey: "orientation"
        )
      }
      UIViewController.attemptRotationToDeviceOrientation()
    }

    vc.setNeedsStatusBarAppearanceUpdate()
    vc.setNeedsUpdateOfHomeIndicatorAutoHidden()
    vc.setNeedsUpdateOfScreenEdgesDeferringSystemGestures()
  }
}

@_cdecl("legion_install_ios_immersive_bridge")
public func legionInstallIosImmersiveBridge() {
  LegionImmersiveBridge.shared.install()
}
