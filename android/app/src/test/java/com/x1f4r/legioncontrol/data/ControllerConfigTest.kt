package com.x1f4r.legioncontrol.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The configuration is the one thing in this app the user types by hand, so what it accepts and
 * what it says when it refuses are both part of the contract.
 */
class ControllerConfigTest {

    private fun error(text: String): String =
        readControllerConfig(text).exceptionOrNull()?.message ?: "no failure"

    @Test
    fun `the example is refused only for its placeholder hardware address`() {
        // The example is a template, not a working configuration, and the one field it makes
        // impossible to miss is the hardware address. Everything else in it has to hold up.
        val message = error(EXAMPLE_CONTROLLER_CONFIG)
        assertTrue(message, message.contains("not a hardware address"))

        val filled = EXAMPLE_CONTROLLER_CONFIG.replace("XX:XX:XX:XX:XX:XX", "aa:bb:cc:dd:ee:ff")
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
    }

    @Test
    fun `ids have to be unique`() {
        val machine = """
            { "id": "one", "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }],
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }
        """.trimIndent()
        assertTrue(error("""{ "machines": [$machine, $machine] }""").contains("share the id"))
    }

    @Test
    fun `a machine needs somewhere to dial and something to run`() {
        assertTrue(error("""{ "machines": [] }""").contains("no machines"))
        assertTrue(
            error(
                """{ "machines": [{ "id": "one", "systems": [{ "id": "l", "agent": ["n"] }] }] }""",
            ).contains("no endpoints"),
        )
        assertTrue(
            error(
                """
                { "machines": [{ "id": "one",
                  "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }] }] }
                """.trimIndent(),
            ).contains("no systems"),
        )
    }

    @Test
    fun `a system without an agent command cannot be reached and is refused`() {
        val message = error(
            """
            { "machines": [{ "id": "one",
              "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }],
              "systems": [{ "id": "linux", "agent": [] }] }] }
            """.trimIndent(),
        )
        assertTrue(message, message.contains("no agent command"))
    }

    @Test
    fun `an endpoint needs a user, a known kind and a system that exists`() {
        fun endpoint(fields: String) = error(
            """
            { "machines": [{ "id": "one",
              "endpoints": [$fields],
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }] }
            """.trimIndent(),
        )
        assertTrue(endpoint("""{ "id": "lan", "kind": "lan", "host": "h" }""").contains("no user"))
        assertTrue(
            endpoint("""{ "id": "lan", "kind": "tailnet", "host": "h", "user": "me" }""")
                .contains("\"lan\" or \"remote\""),
        )
        assertTrue(
            endpoint("""{ "id": "e", "kind": "remote", "host": "h", "user": "me", "system": "bsd" }""")
                .contains("does not have"),
        )
    }

    @Test
    fun `wake is refused when the packet could not be built or sent`() {
        fun wake(fields: String) = error(
            """
            { "machines": [{ "id": "one",
              "endpoints": [{ "id": "lan", "kind": "lan", "host": "h", "user": "me" }],
              "wake": $fields,
              "systems": [{ "id": "linux", "agent": ["node", "a.mjs"] }] }] }
            """.trimIndent(),
        )
        assertTrue(wake("""{ "mac": "nonsense", "broadcast": ["h"] }""").contains("not a hardware address"))
        assertTrue(wake("""{ "mac": "aa:bb:cc:dd:ee:ff" }""").contains("no broadcast address"))
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
