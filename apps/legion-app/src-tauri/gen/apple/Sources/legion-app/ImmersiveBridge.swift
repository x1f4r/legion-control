import Foundation
import UIKit
import WebKit

/// Origin-scoped WKWebView bridge implementing Legion's `immersive(on, { orientation })` and
/// `keepScreenOn(on)` on iOS and iPadOS using `TaoUIViewController`'s dynamic properties.
///
/// The message handler and main-frame reset script are attached deterministically when each
/// `WKWebView` is initialized (`main.mm`), with window-activation notifications kept as a
/// non-polling fallback. Immersive state and the idle timer are reset on every main-frame load,
/// on `webViewWebContentProcessDidTerminate`, and when the app terminates.
@objcMembers
final class LegionImmersiveBridge: NSObject, WKScriptMessageHandler {
  static let shared = LegionImmersiveBridge()

  private static let handlerName = "legionImmersive"
  private static let resetUserScriptSource = """
  (function(){try{var h=window.webkit&&window.webkit.messageHandlers&&window.webkit.messageHandlers.legionImmersive;if(h&&typeof h.postMessage==='function'){h.postMessage({type:'reset'});}}catch(_){}})();
  """

  private let attachedWebViews = NSHashTable<WKWebView>.weakObjects()
  private var observersInstalled = false
  private var immersiveActive = false
  private var orientationLocked = false

  func install() {
    let setup = { [weak self] in
      guard let self = self else { return }
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
    }
    if Thread.isMainThread {
      setup()
    } else {
      DispatchQueue.main.async(execute: setup)
    }
  }

  func attach(to webView: WKWebView) {
    if Thread.isMainThread {
      attachOnMain(webView)
    } else {
      DispatchQueue.main.async { [weak self, weak webView] in
        guard let self = self, let webView = webView else { return }
        self.attachOnMain(webView)
      }
    }
  }

  func resetAll() {
    let work = { [weak self] in
      UIApplication.shared.isIdleTimerDisabled = false
      guard let self = self else { return }
      if self.immersiveActive || self.orientationLocked {
        self.applyImmersive(on: false, orientation: "none")
      }
    }
    if Thread.isMainThread {
      work()
    } else {
      DispatchQueue.main.async(execute: work)
    }
  }

  private func attachOnMain(_ webView: WKWebView) {
    if attachedWebViews.contains(webView) {
      return
    }
    let controller = webView.configuration.userContentController
    controller.removeScriptMessageHandler(forName: Self.handlerName)
    controller.add(self, name: Self.handlerName)
    let resetScript = WKUserScript(
      source: Self.resetUserScriptSource,
      injectionTime: .atDocumentStart,
      forMainFrameOnly: true
    )
    controller.addUserScript(resetScript)
    attachedWebViews.add(webView)
  }

  @objc private func handleWindowEvent() {
    attachIfReady()
  }

  @objc private func handleWillTerminate() {
    resetAll()
  }

  @discardableResult
  private func attachIfReady() -> Bool {
    var attachedAny = false
    for window in allApplicationWindows() {
      guard let rootView = window.rootViewController?.view else { continue }
      for webView in findWebViews(in: rootView) {
        attachOnMain(webView)
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
    case "haptic":
      let kind = (body["kind"] as? String) ?? ""
      performHaptic(kind: kind)
    case "reset":
      resetAll()
    default:
      break
    }
  }

  private func performHaptic(kind: String) {
    let normalized = kind.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    let style: UIImpactFeedbackGenerator.FeedbackStyle
    switch normalized {
    case "tick":
      style = .light
    case "tap":
      style = .medium
    default:
      return
    }
    let generator = UIImpactFeedbackGenerator(style: style)
    generator.prepare()
    generator.impactOccurred()
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
      immersiveActive = false
      orientationLocked = false
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
          scene.requestGeometryUpdate(.iOS(interfaceOrientations: restoreMask)) { _ in }
          vc.setNeedsUpdateOfSupportedInterfaceOrientations()
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

@_cdecl("legion_ios_attach_webview")
public func legionIosAttachWebview(_ webviewPtr: UnsafeMutableRawPointer?) {
  guard let webviewPtr = webviewPtr else { return }
  let webView = Unmanaged<WKWebView>.fromOpaque(webviewPtr).takeUnretainedValue()
  LegionImmersiveBridge.shared.attach(to: webView)
}

@_cdecl("legion_ios_reset_player_state")
public func legionIosResetPlayerState() {
  LegionImmersiveBridge.shared.resetAll()
}
