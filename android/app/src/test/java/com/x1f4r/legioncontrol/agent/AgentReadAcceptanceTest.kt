package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.contract.ContractCorpus
import kotlinx.serialization.json.JsonObject
import org.junit.Assert.*
import org.junit.Test

class AgentReadAcceptanceTest {
    @Test fun `refused status cannot establish capabilities or readiness even with system fields`() {
        val reply = AgentJson.decodeFromString(AgentStatus.serializer(), """{"ok":false,"contract":3,"system":{"id":"linux"},"services":[],"reasonCode":"restricted-command","message":"Command refused"}""")
        val failure = assertThrows(AgentFailure.Reported::class.java) { requireAcceptedRead(reply) }
        assertEquals("Command refused", failure.detail)
        assertEquals(DispatchStage.NOT_STARTED, failure.dispatch)
    }

    @Test fun `successful and legacy status reads remain accepted`() {
        requireAcceptedRead(AgentStatus(ok = true, contract = 3))
        requireAcceptedRead(AgentStatus(osName = "linux"))
    }

    @Test fun `refused or empty operation read cannot fabricate a running record`() {
        val reply = AgentJson.decodeFromString(OpEnvelope.serializer(), """{"ok":false,"reasonCode":"restricted-command","message":"Command refused"}""")
        assertThrows(AgentFailure.Reported::class.java) { reply.record() }
        assertThrows(AgentFailure.BadOutput::class.java) { OpEnvelope(ok = true).record() }
    }

    @Test fun `an operation the machine never had is an answer, under either code`() {
        // 3.1.0 says not-configured, and 3.0.x said bad-argument with keys of its own. A present
        // op null is what both have in common, and it is what decides (contract section 4).
        val current = ContractCorpus.json("fixtures/op.not-found.json") as JsonObject
        val missing = assertThrows(AgentFailure.NoSuchOperation::class.java) {
            readOperation("11111111-2222-4333-8444-555555555555", current)
        }
        assertEquals("11111111-2222-4333-8444-555555555555", missing.operationId)
        assertEquals(DispatchStage.NOT_STARTED, missing.dispatch)

        val older = AgentJson.parseToJsonElement(
            """{"ok":false,"contract":3,"reasonCode":"bad-argument","message":"there is no operation 0123abcd-0000 on this machine","id":"0123abcd-0000","op":null,"command":"op","error":"unknown operation 0123abcd-0000"}""",
        ) as JsonObject
        assertThrows(AgentFailure.NoSuchOperation::class.java) { readOperation("0123abcd-0000", older) }
    }

    @Test fun `a refused read without op null is still a failure to read`() {
        val refused = AgentJson.parseToJsonElement("""{"ok":false,"contract":3,"reasonCode":"bad-argument","message":"\"x\" is not an operation id"}""") as JsonObject
        assertThrows(AgentFailure.Reported::class.java) { readOperation("x", refused) }

        val running = ContractCorpus.json("fixtures/op.running.json") as JsonObject
        assertEquals("running", readOperation("ignored", running).state)
    }

    @Test fun `valid failed operation result is retained inside accepted read`() {
        val op = OpRecord(id = "same-id", state = "finished", result = OpResult(ok = false, action = "failed"))
        assertEquals(op, OpEnvelope(ok = true, op = op).record())
    }
}
