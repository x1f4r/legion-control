import Foundation
import Testing
@testable import LegionControlCore

/// Setup documents as the tests start from them.
extension ControllerConfig {
    /// A document a test starts from, decoded once section 9.7 has accepted it under peer intake.
    /// A test setup this app would refuse is a broken test, not a premise.
    static func accepted(_ data: Data) throws -> ControllerConfig {
        let errors = DocumentAcceptance.check(data, intake: .peer).errors
        guard errors.isEmpty else { throw ConfigProblem(errors.map(\.message).joined(separator: " ")) }
        return try JSONDecoder().decode(ControllerConfig.self, from: data)
    }
}

extension ConfigStore {
    /// Adopt over whatever is applied this instant, as a fetch decided and committed at once would:
    /// nil when it was applied, otherwise why not.
    @MainActor
    func adoptOverCurrent(_ bytes: Data, describedAs description: String) -> String? {
        switch adopt(bytes, describedAs: description, replacing: document?.hash) {
        case .adopted: nil
        case .headMoved(let current): "the applied setup moved to \(current ?? "nothing")"
        case .refused(let problem): problem
        }
    }
}

extension SetupJSON {
    /// A Foundation value as the document text this app writes, new keys in code point order.
    static func document(_ value: Any) throws -> Data {
        try SetupJSON(foundation: value).documentData
    }
}
