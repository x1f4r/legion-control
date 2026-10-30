package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.BusyStatus
import org.junit.Assert.*
import org.junit.Test

class CompactStatusTest {
    @Test fun `explicit unknown wins and otherwise unmonitored stays distinct from observed busy`() {
        assertEquals("Activity unknown", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":true,"monitored":false}""")))
        assertEquals("Not monitored", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":false,"monitored":false,"evidence":"unmonitored"}""")))
        assertEquals("Not monitored", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"evidence":"unmonitored"}""")))
    }

    @Test fun `a service declared never busy is idle, whatever an older agent says of monitored`() {
        // Agents before 3.1.0 said monitored false for busy.type none and for a machine with no
        // services. Only the evidence says whether anything is watching.
        val declared = AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":false,"unknown":false,"monitored":false,"reason":"declared never busy","evidence":"none"}""")
        assertFalse(declared.isUnmonitored)
        assertTrue(declared.isIdleForSure)
        assertNull(compactActivityState(declared))

        val empty = AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":false,"unknown":false,"monitored":false,"reason":"no services are configured","evidence":"none"}""")
        assertFalse(empty.isUnmonitored)
        assertEquals("no services are configured", empty.summary)
    }

    @Test fun `unknown fail closed activity is never relabeled as observed busy work`() {
        val unknown = AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":true,"reason":"probe-timeout"}""")
        assertEquals("Activity unknown", compactActivityState(unknown))
        assertEquals("Busy", compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":true,"unknown":false}""")))
        assertNull(compactActivityState(AgentJson.decodeFromString(BusyStatus.serializer(), """{"busy":false,"unknown":false}""")))
    }
}
