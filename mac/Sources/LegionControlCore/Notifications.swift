import Foundation
import UserNotifications

/// Telling the user that something they asked for has finished.
///
/// Only for operations a person started, and only for the three outcomes worth interrupting them
/// about: it worked, it failed, or nobody knows. An update takes minutes and the window is usually
/// closed by then, so without this the answer sits in a panel nobody has open.
///
/// Authorisation is asked for the first time there is actually something to say, rather than at
/// launch. An app that asks for permission to notify before it has ever had anything to notify
/// about is an app that gets denied.
@MainActor
enum Notifications {
    private static var authorisationAsked = false
    private static var isAllowed = false
    /// Turned off in tests and in any run that has no bundle identity to register with.
    static var isEnabled: Bool {
        get { AppPreferences.bool(forKey: "operationNotifications") ?? false }
        set { AppPreferences.set(newValue, forKey: "operationNotifications") }
    }

    static func post(_ record: OperationRecord) {
        guard isEnabled, Bundle.main.bundleIdentifier != nil else { return }
        let title: String
        switch record.state {
        case .succeeded: title = "\(record.subject) on \(record.machineName)"
        case .failed: title = "\(record.subject) failed on \(record.machineName)"
        case .unknown: title = "\(record.subject): outcome unknown"
        default: return
        }
        deliver(title: title, body: record.summary, id: record.id)
    }

    private static func deliver(title: String, body: String, id: String) {
        let centre = UNUserNotificationCenter.current()
        if !authorisationAsked {
            authorisationAsked = true
            centre.requestAuthorization(options: [.alert, .sound]) { granted, _ in
                Task { @MainActor in
                    isAllowed = granted
                    if granted { send(title: title, body: body, id: id) }
                }
            }
            return
        }
        guard isAllowed else { return }
        send(title: title, body: body, id: id)
    }

    private static func send(title: String, body: String, id: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        UNUserNotificationCenter.current().add(
            UNNotificationRequest(identifier: id, content: content, trigger: nil)
        )
    }
}
