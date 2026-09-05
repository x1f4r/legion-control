import Foundation
import ServiceManagement

/// Start at login, through the modern registration: no helper bundle, no launchd plist written by
/// hand, and the user can always overrule it in System Settings under Login Items.
enum LoginItem {
    static var isEnabled: Bool {
        SMAppService.mainApp.status == .enabled
    }

    /// System Settings can hold a registration in "requires approval", which looks like off but is
    /// not: registering again would not help, the user has to allow it.
    static var needsApproval: Bool {
        SMAppService.mainApp.status == .requiresApproval
    }



    /// Returns nil on success, or a sentence to show the user.
    @discardableResult
    static func set(_ enabled: Bool) -> String? {
        guard AppPaths.homeOverride == nil else { return "Login items cannot be changed from an isolated run." }
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            return nil
        } catch {
            return enabled
                ? "Legion Control could not be added to your login items. \(error.localizedDescription)"
                : "Legion Control could not be removed from your login items. \(error.localizedDescription)"
        }
    }
}
