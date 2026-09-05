package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.BusyStatus
import org.junit.Assert.*
import org.junit.Test

class CompactStatusTest {
    @Test fun `explicit unknown wins and otherwise unmonitored stays distinct from observed busy`() {
        assertEquals("Activity unknown", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":true,"monitored":false}""")))
        assertEquals("Not monitored", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":false,"monitored":false}""")))
        assertEquals("Not monitored", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"evidence":"unmonitored"}""")))
    }

    @Test fun `unknown fail closed activity is never relabeled as observed busy work`() {
        val unknown = AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":true,"reason":"probe-timeout"}""")
        assertEquals("Activity unknown", compactActivityState(unknown))
        assertEquals("Busy", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":false}""")))
        assertNull(compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":false,"unknown":false}""")))
    }
}
