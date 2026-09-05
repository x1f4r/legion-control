import Foundation

/// A question the app is waiting on an answer to.
///
/// Every question in this app has the same shape: a title, a sentence saying what pressing yes
/// costs, the word on the yes button, and the thing to do when it is pressed. Carrying the work as
/// a closure rather than as a case in an enum is what lets the wording come from a service name or
/// an action's own `confirm` text instead of being written into the app.
///
/// A disruptive question also carries a third answer. "Now" and "cancel" were the only two, which
/// meant the choice in front of a busy machine was interrupt the work or give up; "when idle" is the
/// one people actually want, and it belongs in the same sheet as the decision it replaces.
///
/// The window draws it as an alert and the menu bar panel draws it inline, so the two can never
/// disagree about what a confirmation does: there is only one of them.
struct PendingDialog: Identifiable {
    var id: String
    var title: String
    var message: String
    var confirmTitle: String
    var perform: @MainActor () -> Void
    /// The secondary answer, when there is one. Absent against an agent that cannot queue.
    var alternativeTitle: String?
    var alternative: (@MainActor () -> Void)?

    init(
        id: String,
        title: String,
        message: String,
        confirmTitle: String,
        perform: @escaping @MainActor () -> Void,
        alternativeTitle: String? = nil,
        alternative: (@MainActor () -> Void)? = nil
    ) {
        self.id = id
        self.title = title
        self.message = message
        self.confirmTitle = confirmTitle
        self.perform = perform
        self.alternativeTitle = alternativeTitle
        self.alternative = alternative
    }

    var hasAlternative: Bool { alternativeTitle != nil && alternative != nil }
}
