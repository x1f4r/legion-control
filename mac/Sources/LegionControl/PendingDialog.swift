import Foundation

/// A question the app is waiting on an answer to.
///
/// Every question in this app has the same shape: a title, a sentence saying what pressing yes
/// costs, the word on the yes button, and the thing to do when it is pressed. Carrying the work as
/// a closure rather than as a case in an enum is what lets the wording come from a service name or
/// an action's own `confirm` text instead of being written into the app.
///
/// The window draws it as an alert and the menu bar panel draws it inline, so the two can never
/// disagree about what a confirmation does: there is only one of them.
struct PendingDialog: Identifiable {
    var id: String
    var title: String
    var message: String
    var confirmTitle: String
    var perform: @MainActor () -> Void
}
