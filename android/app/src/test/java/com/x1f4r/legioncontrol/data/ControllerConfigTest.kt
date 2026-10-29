package com.x1f4r.legioncontrol.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The configuration is the one thing in this app the user types by hand, so what it accepts and
 * what it says when it refuses are both part of the contract (section 9.7): a document is refused
 * for a broken schema constraint or a relationship rule, and everything this app merely cannot use
 * is a warning.
 */
class ControllerConfigTest {

    private fun error(text: String): String =
        readControllerConfig(text).exceptionOrNull()?.message ?: "no failure"

    private fun warnings(text: String): List<String> =
        acceptControllerConfig(text).getOrThrow().verdict.warnings.map { it.code }

    @Test
    fun `the example is refused only for its placeholder hardware address`() {
        // The example is a template, not a working configuration, and the one field it makes
        // impossible to miss is the hardware address. Everything else in it has to hold up.
        val message = error(EXAMPLE_CONTROLLER_CONFIG)
        assertTrue(message, message.contains("not a hardware address"))

        val filled = EXAMPLE_CONTROLLER_CONFIG.replace("XX:XX:XX:XX:XX:XX", "02:00:00:00:00:01")
        val config = readControllerConfig(filled).getOrNull()
        assertNotNull(config)
        assertEquals(1, config!!.machines.size)
        assertEquals("workstation", config.machines.single().id)
        assertEquals(2, config.machines.single().systems.size)
        assertEquals(DEFAULT_GITHUB_REPO, config.appUpdates.githubRepo)
    }

    @Test
    fun `the Mac's own keys are read past rather than refused`() {
        // One document is meant to serve both apps. "ssh" is how the Mac dials and "local" is the
        // Mac looking after itself; neither is this app's business and neither may fail a paste.
        val config = readControllerConfig(
            """
            {
              "version": 1,
              "machines": [
                {
                  "id": "one", "name": "One",
                  "ssh": { "host": "one" },
                  "endpoints": [{ "id": "lan", "kind": "lan", "host": "host.invalid", "user": "me" }],
                  "systems": [{ "id": "linux", "platform": "linux", "agent": ["node", "agent.mjs"] }]
                }
              ],
              "local": { "enabled": true, "name": "This Mac", "agent": "~/agent.mjs" }
            }
            """.trimIndent(),
        )
        assertNull(config.exceptionOrNull())
        // A missing port is 22 and a missing label is the host, so neither has to be written out.
        val endpoint = config.getOrThrow().machines.single().endpoints.single()
        assertEquals(22, endpoint.port)
        assertNull(endpoint.label)
    }

    @Test
    fun `a document that is not JSON says so rather than throwing`() {
        assertTrue(error("not json at all").startsWith("That is not a configuration"))
        assertTrue(error("   ").contains("nothing to apply"))
        // JSON, and not a setup: a setup has a version and a list of machines.
        assertTrue(error("""{ "machines": [] }""").startsWith("That is not a configuration"))
    }

    @Test
    fun `ids have to be unique`() {
        val machine = """
            { "id": "one", "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }],
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }
        """.trimIndent()
        assertTrue(error("""{ "version": 1, "machines": [$machine, $machine] }""").contains("share the id"))
    }

    @Test
    fun `a machine that cannot be driven yet is accepted, and says why`() {
        // C3: each of these used to refuse the whole setup, so a machine somebody was halfway
        // through adding on another device stopped replication for everybody.
        assertEquals(listOf("no-machines"), warnings("""{ "version": 1, "machines": [] }"""))
        assertEquals(
            listOf("machine-no-endpoints"),
            warnings("""{ "version": 1, "machines": [{ "id": "one", "systems": [{ "id": "l", "agent": ["n"] }] }] }"""),
        )
        assertEquals(
            listOf("machine-no-systems"),
            warnings("""{ "version": 1, "machines": [{ "id": "one", "endpoints": [{ "id": "lan", "host": "h", "user": "me" }] }] }"""),
        )
        assertEquals(
            listOf("system-no-agent"),
            warnings(
                """{ "version": 1, "machines": [{ "id": "one", "endpoints": [{ "id": "lan", "host": "h", "user": "me" }],
                     "systems": [{ "id": "linux", "agent": [] }] }] }""",
            ),
        )
    }

    @Test
    fun `an endpoint with no user or a hint to nothing is a warning, and an unknown kind is refused`() {
        fun endpoint(fields: String) = """
            { "version": 1, "machines": [{ "id": "one",
              "endpoints": [$fields],
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }] }
        """.trimIndent()
        assertEquals(listOf("endpoint-no-user"), warnings(endpoint("""{ "id": "lan", "kind": "lan", "host": "h" }""")))
        assertEquals(
            listOf("endpoint-unknown-system"),
            warnings(endpoint("""{ "id": "e", "kind": "remote", "host": "h", "user": "me", "system": "bsd" }""")),
        )
        assertTrue(error(endpoint("""{ "id": "lan", "kind": "tailnet", "host": "h", "user": "me" }""")).contains("lan, remote"))
    }

    @Test
    fun `wake is refused only for a hardware address no packet can carry`() {
        fun wake(fields: String) = """
            { "version": 1, "machines": [{ "id": "one",
              "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }],
              "wake": $fields,
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }] }
        """.trimIndent()
        assertTrue(error(wake("""{ "mac": "nonsense", "broadcast": ["192.0.2.255"] }""")).contains("not a hardware address"))
        // No broadcast is a default, not a fault: direct wake falls back to the derived one (C8).
        assertEquals(listOf("wake-broadcast-defaulted"), warnings(wake("""{ "mac": "02:00:00:00:00:01", "lanPrefix": "192.0.2." }""")))
    }

    @Test
    fun `a version this app does not understand is refused rather than half read`() {
        val message = error(
            """
            { "version": 2, "machines": [{ "id": "one",
              "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }],
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }] }
            """.trimIndent(),
        )
        assertTrue(message, message.contains("version 2"))
    }
}
