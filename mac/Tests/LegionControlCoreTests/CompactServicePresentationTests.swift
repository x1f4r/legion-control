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
        {"id":"tool","running":true,"upToDate":false,"busy":{"busy":false,"unknown":false,"monitored":false,"evidence":"unmonitored"}}
        """.utf8))
        #expect(CompactServicePresentation.state(for: unmonitored) == "Not monitored")
    }

    @Test("a service declared never busy is not warned about, whatever an older agent says of monitored")
    func declaredNeverBusy() throws {
        // Agents before 3.1.0 said monitored false for busy.type none, and for a machine with no
        // services. That is an answer, and only the evidence says whether anything watches (C2).
        let declared = try JSONDecoder().decode(ServiceStatus.self, from: Data("""
        {"id":"tool","running":true,"upToDate":false,"busy":{"busy":false,"unknown":false,"monitored":false,"reason":"declared never busy","evidence":"none"}}
        """.utf8))
        #expect(CompactServicePresentation.state(for: declared) == "Update available")
        #expect(declared.busy?.verdict == .idle)

        let empty = try JSONDecoder().decode(AgentStatus.self, from: Data("""
        {"ok":true,"contract":3,"agentVersion":"3.0.1","busy":{"busy":false,"unknown":false,"monitored":false,"reason":"no services are configured","evidence":"none"},"services":[]}
        """.utf8))
        #expect(empty.busy?.verdict == .idle)
        #expect(empty.busy?.summary == "no services are configured")

        // What agent 3.1.0 sends for the same two, and for a service with no probe at all.
        let reply = try JSONDecoder().decode(AgentBusyResult.self, from: ContractCorpus.data("fixtures/busy.declared-none.json"))
        #expect(reply.services?.map(\.verdict) == [.idle, .busy])
        var unwatched = try #require(reply.services?.last)
        unwatched.busy = false
        unwatched.unknown = false
        #expect(unwatched.verdict == .unmonitored)
        let none = try JSONDecoder().decode(AgentBusyResult.self, from: ContractCorpus.data("fixtures/busy.no-services.json"))
        #expect(!none.blocksDisruptiveWork)
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
