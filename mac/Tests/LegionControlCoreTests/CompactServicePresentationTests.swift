import Foundation
import Testing
@testable import LegionControlCore

struct CompactServicePresentationTests {
    @Test("fail-closed busy flags retain unknown and unmonitored activity in the compact row")
    func uncertainActivity() throws {
        let unknown = try JSONDecoder().decode(ServiceStatus.self, from: Data("""
        {"id":"tool","running":true,"upToDate":false,"stagedVersion":"2.0","busy":{"busy":true,"unknown":true,"monitored":true}}
        """.utf8))
        #expect(CompactServicePresentation.state(for: unknown) == "Activity unknown")
        let unmonitored = try JSONDecoder().decode(ServiceStatus.self, from: Data("""
        {"id":"tool","running":true,"upToDate":false,"busy":{"busy":true,"monitored":false}}
        """.utf8))
        #expect(CompactServicePresentation.state(for: unmonitored) == "Not monitored")
    }
}

@MainActor
struct SheetActionRelayTests {
    @Test("sheet actions run once after dismissal and a plain dismissal runs nothing")
    func dismissalBoundary() {
        let relay = SheetActionRelay()
        var calls = 0
        relay.didDismiss()
        #expect(calls == 0)
        relay.queue { calls += 1 }
        #expect(calls == 0)
        relay.didDismiss()
        #expect(calls == 1)
        relay.didDismiss()
        #expect(calls == 1)
    }
}
