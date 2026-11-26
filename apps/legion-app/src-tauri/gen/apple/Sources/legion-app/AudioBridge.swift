import AVFoundation
import Foundation
import MediaPlayer
import UIKit
import WebKit

/// Origin-scoped WKWebView bridge configuring `AVAudioSession` (`.playback`), `MPNowPlayingInfoCenter`,
/// `MPRemoteCommandCenter`, call-interruption handling, and headphone-unplug detection on iOS and
/// iPadOS.
///
/// The message handler is attached deterministically at `WKWebView` creation (`main.mm`) and
/// restricted to the bundled main-frame origin (`tauri://localhost`). Remote commands and audio
/// session interruptions are forwarded back to `window.__LEGION_IOS_AUDIO_DISPATCH__`.
@objcMembers
final class LegionAudioBridge: NSObject, WKScriptMessageHandler {
  static let shared = LegionAudioBridge()

  private static let handlerName = "legionAudio"

  private let attachedWebViews = NSHashTable<WKWebView>.weakObjects()
  private var observersInstalled = false
  private var remoteCommandsInstalled = false
  private var sessionActive = false
  private var nowPlayingInfo: [String: Any] = [:]
  private var cachedArtworkDataUrl: String?
  private var cachedArtwork: MPMediaItemArtwork?

  func install() {
    let setup = { [weak self] in
      guard let self = self else { return }
      if !self.observersInstalled {
        self.observersInstalled = true
        let nc = NotificationCenter.default
        nc.addObserver(
          self,
          selector: #selector(self.handleInterruption(_:)),
          name: AVAudioSession.interruptionNotification,
          object: AVAudioSession.sharedInstance()
        )
        nc.addObserver(
          self,
          selector: #selector(self.handleRouteChange(_:)),
          name: AVAudioSession.routeChangeNotification,
          object: AVAudioSession.sharedInstance()
        )
        nc.addObserver(
          self,
          selector: #selector(self.handleWillTerminate),
          name: UIApplication.willTerminateNotification,
          object: nil
        )
      }
      self.installRemoteCommandsIfNeeded()
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
      self?.clearNowPlayingAndSession()
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
    attachedWebViews.add(webView)
  }

  private func installRemoteCommandsIfNeeded() {
    if remoteCommandsInstalled {
      return
    }
    remoteCommandsInstalled = true
    let center = MPRemoteCommandCenter.shared()

    center.playCommand.isEnabled = false
    center.playCommand.addTarget { [weak self] _ in
      self?.dispatchToWebView(["action": "play"])
      return .success
    }

    center.pauseCommand.isEnabled = false
    center.pauseCommand.addTarget { [weak self] _ in
      self?.dispatchToWebView(["action": "pause"])
      return .success
    }

    center.togglePlayPauseCommand.isEnabled = false
    center.togglePlayPauseCommand.addTarget { [weak self] _ in
      self?.dispatchToWebView(["action": "toggle"])
      return .success
    }

    center.nextTrackCommand.isEnabled = false
    center.nextTrackCommand.addTarget { [weak self] _ in
      self?.dispatchToWebView(["action": "next"])
      return .success
    }

    center.previousTrackCommand.isEnabled = false
    center.previousTrackCommand.addTarget { [weak self] _ in
      self?.dispatchToWebView(["action": "prev"])
      return .success
    }

    center.changePlaybackPositionCommand.isEnabled = false
    center.changePlaybackPositionCommand.addTarget { [weak self] event in
      guard let posEvent = event as? MPChangePlaybackPositionCommandEvent else {
        return .commandFailed
      }
      self?.dispatchToWebView([
        "action": "seek",
        "position": posEvent.positionTime,
      ])
      return .success
    }
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
    case "activate":
      activatePlaybackSession()
    case "now_playing":
      updateNowPlaying(body)
    case "playback_state":
      updatePlaybackState(body)
    case "clear":
      clearNowPlayingAndSession()
    default:
      break
    }
  }

  private func activatePlaybackSession() {
    let session = AVAudioSession.sharedInstance()
    do {
      try session.setCategory(.playback, mode: .default, options: [])
      try session.setActive(true)
      sessionActive = true
    } catch {
      // Keep running gracefully even if another higher-priority session holds audio.
    }
  }

  private func updateNowPlaying(_ body: [String: Any]) {
    installRemoteCommandsIfNeeded()
    if (body["playing"] as? Bool) == true {
      activatePlaybackSession()
    }

    var info: [String: Any] = [:]
    if let title = (body["title"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines),
       !title.isEmpty {
      info[MPMediaItemPropertyTitle] = title
    }
    if let artist = (body["artist"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines),
       !artist.isEmpty {
      info[MPMediaItemPropertyArtist] = artist
    }
    if let album = (body["album"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines),
       !album.isEmpty {
      info[MPMediaItemPropertyAlbumTitle] = album
    }
    if let duration = body["duration"] as? Double, duration.isFinite, duration > 0 {
      info[MPMediaItemPropertyPlaybackDuration] = duration
    }
    if let elapsed = body["elapsed"] as? Double, elapsed.isFinite, elapsed >= 0 {
      info[MPNowPlayingInfoPropertyElapsedPlaybackTime] = elapsed
    }
    let playing = (body["playing"] as? Bool) ?? false
    let rate = playing ? ((body["rate"] as? Double) ?? 1.0) : 0.0
    info[MPNowPlayingInfoPropertyPlaybackRate] = rate

    if let dataUrl = body["artworkDataUrl"] as? String, !dataUrl.isEmpty {
      if let art = artworkForDataUrl(dataUrl) {
        info[MPMediaItemPropertyArtwork] = art
      }
    } else {
      cachedArtworkDataUrl = nil
      cachedArtwork = nil
    }

    nowPlayingInfo = info
    MPNowPlayingInfoCenter.default().nowPlayingInfo = info
    updateRemoteCommandAvailability(
      active: true,
      canNext: (body["canNext"] as? Bool) ?? false,
      canPrev: (body["canPrev"] as? Bool) ?? true,
      canSeek: ((body["duration"] as? Double) ?? 0.0) > 0.0
    )
  }

  private func updatePlaybackState(_ body: [String: Any]) {
    if (body["playing"] as? Bool) == true && !sessionActive {
      activatePlaybackSession()
    }
    if let duration = body["duration"] as? Double, duration.isFinite, duration > 0 {
      nowPlayingInfo[MPMediaItemPropertyPlaybackDuration] = duration
    }
    if let elapsed = body["elapsed"] as? Double, elapsed.isFinite, elapsed >= 0 {
      nowPlayingInfo[MPNowPlayingInfoPropertyElapsedPlaybackTime] = elapsed
    }
    let playing = (body["playing"] as? Bool) ?? false
    let rate = playing ? ((body["rate"] as? Double) ?? 1.0) : 0.0
    nowPlayingInfo[MPNowPlayingInfoPropertyPlaybackRate] = rate
    if !nowPlayingInfo.isEmpty {
      MPNowPlayingInfoCenter.default().nowPlayingInfo = nowPlayingInfo
    }
    updateRemoteCommandAvailability(
      active: !nowPlayingInfo.isEmpty,
      canNext: (body["canNext"] as? Bool) ?? false,
      canPrev: (body["canPrev"] as? Bool) ?? true,
      canSeek: ((body["duration"] as? Double) ?? 0.0) > 0.0
    )
  }

  private func clearNowPlayingAndSession() {
    nowPlayingInfo.removeAll()
    cachedArtworkDataUrl = nil
    cachedArtwork = nil
    MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
    updateRemoteCommandAvailability(active: false, canNext: false, canPrev: false, canSeek: false)
    if sessionActive {
      sessionActive = false
      try? AVAudioSession.sharedInstance().setActive(
        false,
        options: .notifyOthersOnDeactivation
      )
    }
  }

  private func updateRemoteCommandAvailability(
    active: Bool,
    canNext: Bool,
    canPrev: Bool,
    canSeek: Bool
  ) {
    let center = MPRemoteCommandCenter.shared()
    center.playCommand.isEnabled = active
    center.pauseCommand.isEnabled = active
    center.togglePlayPauseCommand.isEnabled = active
    center.nextTrackCommand.isEnabled = active && canNext
    center.previousTrackCommand.isEnabled = active && canPrev
    center.changePlaybackPositionCommand.isEnabled = active && canSeek
  }

  private func artworkForDataUrl(_ dataUrl: String) -> MPMediaItemArtwork? {
    if cachedArtworkDataUrl == dataUrl, let cached = cachedArtwork {
      return cached
    }
    guard let comma = dataUrl.firstIndex(of: ",") else { return nil }
    let base64 = String(dataUrl[dataUrl.index(after: comma)...])
    guard let raw = Data(base64Encoded: base64, options: .ignoreUnknownCharacters),
          let image = UIImage(data: raw) else {
      return nil
    }
    let artwork = MPMediaItemArtwork(boundsSize: image.size) { _ in image }
    cachedArtworkDataUrl = dataUrl
    cachedArtwork = artwork
    return artwork
  }

  private func dispatchToWebView(_ payload: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: payload, options: []),
          let json = String(data: data, encoding: .utf8) else {
      return
    }
    let js = "if (typeof window.__LEGION_IOS_AUDIO_DISPATCH__ === 'function') { window.__LEGION_IOS_AUDIO_DISPATCH__(\(json)); }"
    let run = { [weak self] in
      guard let self = self else { return }
      for webView in self.attachedWebViews.allObjects {
        webView.evaluateJavaScript(js, completionHandler: nil)
      }
    }
    if Thread.isMainThread {
      run()
    } else {
      DispatchQueue.main.async(execute: run)
    }
  }

  @objc private func handleInterruption(_ notification: Notification) {
    guard let userInfo = notification.userInfo,
          let rawType = userInfo[AVAudioSessionInterruptionTypeKey] as? UInt,
          let type = AVAudioSession.InterruptionType(rawValue: rawType) else {
      return
    }
    switch type {
    case .began:
      dispatchToWebView(["action": "interruption_began"])
    case .ended:
      let rawOptions = (userInfo[AVAudioSessionInterruptionOptionKey] as? UInt) ?? 0
      let shouldResume = AVAudioSession.InterruptionOptions(rawValue: rawOptions)
        .contains(.shouldResume)
      dispatchToWebView([
        "action": "interruption_ended",
        "shouldResume": shouldResume,
      ])
    @unknown default:
      break
    }
  }

  @objc private func handleRouteChange(_ notification: Notification) {
    guard let userInfo = notification.userInfo,
          let rawReason = userInfo[AVAudioSessionRouteChangeReasonKey] as? UInt,
          let reason = AVAudioSession.RouteChangeReason(rawValue: rawReason) else {
      return
    }
    if reason == .oldDeviceUnavailable {
      dispatchToWebView(["action": "route_unavailable"])
    }
  }

  @objc private func handleWillTerminate() {
    clearNowPlayingAndSession()
  }
}

@_cdecl("legion_install_ios_audio_bridge")
public func legionInstallIosAudioBridge() {
  LegionAudioBridge.shared.install()
}

@_cdecl("legion_ios_audio_attach_webview")
public func legionIosAudioAttachWebview(_ webviewPtr: UnsafeMutableRawPointer?) {
  guard let webviewPtr = webviewPtr else { return }
  let webView = Unmanaged<WKWebView>.fromOpaque(webviewPtr).takeUnretainedValue()
  LegionAudioBridge.shared.attach(to: webView)
}

@_cdecl("legion_ios_audio_reset")
public func legionIosAudioReset() {
  LegionAudioBridge.shared.resetAll()
}
