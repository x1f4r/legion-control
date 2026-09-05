package com.x1f4r.legioncontrol.net

import org.junit.Assert.assertEquals
import org.junit.Test

class RouteSelectorTest {
    private fun endpoint(id: String, kind: RouteKind, system: String? = null) = Endpoint(
        id = id,
        kind = kind,
        host = "$id.invalid",
        port = 22,
        user = "me",
        systemHint = system,
        label = id,
    )

    private val linux = endpoint("linux-remote", RouteKind.REMOTE, "linux")
    private val windows = endpoint("windows-remote", RouteKind.REMOTE, "windows")
    private val lan = endpoint("lan", RouteKind.LAN)

    @Test
    fun `boot target leads and on-site LAN is next`() {
        val ordered = orderEndpoints(listOf(linux, windows, lan), null, "windows", preferLan = true)
        assertEquals(listOf("windows-remote", "lan", "linux-remote"), ordered.map { it.id })
    }

    @Test
    fun `off-site routing keeps remote endpoints ahead of LAN`() {
        val ordered = orderEndpoints(listOf(lan, linux, windows), null, null, preferLan = false)
        assertEquals(listOf("linux-remote", "windows-remote", "lan"), ordered.map { it.id })
    }

    @Test
    fun `an authenticated remembered route still leads ordinary calls`() {
        val ordered = orderEndpoints(listOf(linux, windows, lan), "lan", null, preferLan = false)
        assertEquals("lan", ordered.first().id)
    }
}
