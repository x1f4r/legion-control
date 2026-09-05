package com.x1f4r.legioncontrol.agent

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

    @Test fun `valid failed operation result is retained inside accepted read`() {
        val op = OpRecord(id = "same-id", state = "finished", result = OpResult(ok = false, action = "failed"))
        assertEquals(op, OpEnvelope(ok = true, op = op).record())
    }
}
