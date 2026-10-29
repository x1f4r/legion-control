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
