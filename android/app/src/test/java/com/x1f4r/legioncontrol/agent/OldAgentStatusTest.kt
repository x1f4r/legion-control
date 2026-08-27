package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.EndpointConfig
import com.x1f4r.legioncontrol.data.MachineConfig
import com.x1f4r.legioncontrol.data.SystemConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The apps are updated before the machines are, so everything here is read from a reply written by
 * an agent that has never heard of a service, a system or a boot target.
 */
class OldAgentStatusTest {

    private val old = """
        {
          "ok": true,
          "os": "linux",
          "hostname": "workstation",
          "agentVersion": "1.4.0",
          "t3": {
            "installed": "0.0.36-nightly.20260827.1206",
            "nightly": "0.0.37-nightly.20260828.0101",
            "upToDate": false,
            "serverRunning": true,
            "healthy": true,
            "port": 3773
          },
          "busy": { "busy": false, "reason": "idle", "unknown": false },
          "autoUpdate": true,
          "pendingRestart": false,
          "lastUpdate": { "at": "2026-08-27T10:00:00Z", "from": "a", "to": "b", "result": "ok" },
          "connect": { "configured": true, "running": true },
          "notes": []
        }
    """.trimIndent()

    private fun decode(text: String) = AgentJson.decodeFromString(AgentStatus.serializer(), text)

    @Test
    fun `one service is built out of the t3 block`() {
        val status = decode(old)
        assertFalse(status.reportsServices)

        val service = status.effectiveServices.single()
        assertEquals("t3", service.id)
        assertEquals("T3 Code", service.name)
        assertEquals("0.0.36-nightly.20260827.1206", service.installed)
        assertEquals("0.0.37-nightly.20260828.0101", service.latest)
        assertEquals("nightly", service.channel)
        assertEquals(false, service.upToDate)
        assertEquals(true, service.running)
        assertEquals(true, service.healthy)
        assertEquals(3773, service.port)
        assertEquals(false, service.busy?.isBusy)
        assertEquals(true, service.relay?.running)
        assertEquals(false, service.pendingRestart)
        assertEquals("ok", service.lastUpdate?.result)
        // Nothing was said about either, and "unknown" must not read as "no".
        assertNull(service.canUpdate)
        assertNull(service.canRestart)
    }

    @Test
    fun `the platform name stands in for a system id that was never sent`() {
        assertEquals("linux", decode(old).systemId)
        assertNull(decode(old).systemName)
        assertNull(decode(old).bootTargets)
        assertTrue(decode(old).actions.isNullOrEmpty())
    }

    @Test
    fun `a system named by the configuration is matched by platform when the agent only has one`() {
        val machine = MachineConfig(
            id = "one",
            name = "One",
            endpoints = listOf(EndpointConfig(id = "lan", kind = "lan", host = "h", user = "me")),
            systems = listOf(
                SystemConfig(id = "cachyos", name = "CachyOS", platform = "linux", agent = listOf("node", "a.mjs")),
                SystemConfig(id = "windows", name = "Windows", platform = "windows", agent = listOf("node", "a.mjs")),
            ),
        ).let { ControllerConfig(machines = listOf(it)) }
            .toMachines()
            .single()

        val status = decode(old)
        val system = machine.resolveSystem(status.systemId, status.systemName, status.osName)
        assertEquals("cachyos", system?.id)
        assertEquals("CachyOS", system?.name)
        assertTrue(system!!.isConfigured)
    }

    @Test
    fun `a new reply is read as itself, services and all`() {
        val status = decode(
            """
            {
              "ok": true, "os": "linux",
              "system": { "id": "cachyos", "name": "CachyOS" },
              "services": [
                { "id": "t3", "name": "T3 Code", "installed": "1", "latest": "1", "upToDate": true,
                  "canUpdate": true, "canRestart": true },
                { "id": "sunshine", "name": "Sunshine", "installed": "2", "canUpdate": false }
              ],
              "bootTargets": [{ "id": "windows", "name": null }],
              "actions": [{ "id": "restart-sunshine", "name": "Restart Sunshine", "busyGated": false }],
              "busy": { "busy": true, "reason": "one turn running" }
            }
            """.trimIndent(),
        )
        assertTrue(status.reportsServices)
        assertEquals("cachyos", status.systemId)
        assertEquals("CachyOS", status.systemName)
        assertEquals(listOf("t3", "sunshine"), status.effectiveServices.map { it.id })
        assertEquals(false, status.effectiveServices[1].canUpdate)
        assertEquals("windows", status.bootTargets?.single()?.id)
        assertEquals("Restart Sunshine", status.actions?.single()?.displayName)
        assertTrue(status.isBusy)
    }

    @Test
    fun `a field nobody here has heard of costs nothing`() {
        val status = decode("""{ "ok": true, "os": "linux", "somethingNew": { "a": 1 } }""")
        assertEquals("linux", status.systemId)
        assertTrue(status.effectiveServices.isEmpty())
    }

    @Test
    fun `the one JSON object is taken out of whatever else was on the stream`() {
        val noisy = "#< CLIXML\n<Objs Version=\"1.1.0.1\">progress</Objs>\n{\"ok\":true}\nbye"
        assertEquals("""{"ok":true}""", sliceJsonObject(noisy))
        assertNull(sliceJsonObject("no braces here"))
        assertNull(sliceJsonObject("}{"))
        assertEquals("""{"a":{"b":1}}""", sliceJsonObject("""noise {"a":{"b":1}} noise"""))
    }
}
