package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Test

class PolicyPatchTest {
    @Test
    fun `unchanged keys are omitted and explicit inheritance is null`() {
        assertEquals("{}", String(encodePolicyPatch(PolicyPatch()), Charsets.UTF_8))
        assertEquals(
            "{\"automatic\":null,\"pauseUntil\":null,\"maintenanceWindows\":null}",
            String(encodePolicyPatch(PolicyPatch.inheritAll()), Charsets.UTF_8),
        )
    }

    @Test
    fun `windows keep their contract shape including overnight values`() {
        val bytes = encodePolicyPatch(
            PolicyPatch(
                maintenanceWindows = listOf(
                    MaintenanceWindow(listOf("fri", "sat"), "23:30", "04:00"),
                ),
            ),
        )
        assertEquals(
            "{\"maintenanceWindows\":[{\"days\":[\"fri\",\"sat\"],\"from\":\"23:30\",\"to\":\"04:00\"}]}",
            String(bytes, Charsets.UTF_8),
        )
    }
}
