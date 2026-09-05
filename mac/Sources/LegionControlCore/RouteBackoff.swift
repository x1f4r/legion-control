import Foundation

/// Failed addresses get a full minute before another authenticated connection attempt, including
/// after a boot changes the preferred system. Copies of a transport share this small cache.
final class RouteBackoff: @unchecked Sendable {
    private let lock = NSLock()
    private var failures: [String: (until: Date, failure: AgentFailure)] = [:]
    private let now: @Sendable () -> Date

    init(now: @escaping @Sendable () -> Date = { Date() }) { self.now = now }

    private func key(_ target: SSHTarget) -> String { "\(target.host):\(target.port ?? 22)" }

    func failure(for target: SSHTarget) -> AgentFailure? {
        lock.withLock {
            guard let entry = failures[key(target)], entry.until > now() else { return nil }
            return entry.failure
        }
    }

    func record(_ failure: AgentFailure, target: SSHTarget) {
        lock.withLock { failures[key(target)] = (now().addingTimeInterval(60), failure) }
    }

    func clear(_ target: SSHTarget) {
        _ = lock.withLock { failures.removeValue(forKey: key(target)) }
    }
}
