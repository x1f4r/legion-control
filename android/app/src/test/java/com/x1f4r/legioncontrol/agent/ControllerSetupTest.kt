package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.readControllerConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The setup, as it comes off a machine.
 *
 * Three replies matter and they are told apart by one key: a machine carrying a document, a machine
 * carrying none, and an agent so old that `config` is not a command it knows. The last two both
 * arrive as "no document", and the app has to say completely different things about them.
 */
class ControllerSetupTest {

    private val document = """
        {
          "ok": true,
          "controller": {
            "version": 1,
            "machines": [
              {
                "id": "workstation",
                "name": "Workstation",
                "endpoints": [
                  { "id": "tailnet", "kind": "remote", "host": "100.64.0.10", "port": 22, "user": "me" }
                ],
                "systems": [
                  { "id": "linux", "platform": "linux", "agent": ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"] }
                ]
              }
            ]
          },
          "hash": "9f2c1b0a"
        }
    """.trimIndent()

    @Test
    fun `a machine carrying a document hands over its text and its hash`() {
        val fetched = readControllerReply(document) as ControllerFetch.Document
        assertEquals("9f2c1b0a", fetched.hash)

        // Written back out to be read and edited, so it is not the one line it arrived as.
        assertTrue(fetched.text.lines().size > 1)

        // And it is still the same document: what comes out of the fetch is what the paste field
        // validates, because both go through the same reading.
        val configuration = readControllerConfig(fetched.text).getOrThrow()
        assertEquals(1, configuration.machines.size)
        assertEquals("workstation", configuration.machines.single().id)
    }

    @Test
    fun `a null controller is a machine that has been shared nothing yet`() {
        val reply = """{ "ok": true, "controller": null, "hash": null }"""
        assertEquals(ControllerFetch.NothingStored, readControllerReply(reply))
    }

    @Test
    fun `a reply with no controller key at all is an agent that predates the command`() {
        // What a 1.x agent answers when it is handed a command it has never heard of. There is a
        // key called "hash" nowhere in it, and no document either, and the difference from the case
        // above is the whole reason this is read off the raw object.
        val old = """
            {
              "ok": false,
              "error": "unknown command: config",
              "commands": ["status", "busy", "update", "restart", "auto-update", "boot", "version", "help"]
            }
        """.trimIndent()
        assertEquals(ControllerFetch.TooOld, readControllerReply(old))
    }

    @Test
    fun `output that holds no reply is a reason to try the next command shape`() {
        assertEquals(
            ControllerFetch.Unreadable,
            readControllerReply("bash: node: command not found\n"),
        )
    }

    @Test
    fun `the reply is taken out of whatever the shell wrapped it in`() {
        // PowerShell puts a CLIXML banner in front of remote output, which is why the reply is
        // sliced out rather than parsed as a whole stream.
        val noisy = "#< CLIXML\n<Objs Version=\"1.1.0.1\"/>\n" +
            """{ "ok": true, "controller": null, "hash": null }""" + "\nProgress: done\n"
        assertEquals(ControllerFetch.NothingStored, readControllerReply(noisy))
    }

    @Test
    fun `the command shapes are the standard installs, in order`() {
        assertEquals(
            listOf(
                "/usr/bin/node /home/me/.legion-control/agent/src/index.mjs config",
                "node C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs config",
                "node ~/.legion-control/agent/src/index.mjs config",
                "/opt/homebrew/bin/node ~/.legion-control/agent/src/index.mjs config",
            ),
            setupCommands("me"),
        )
    }

    /**
     * The rule that keeps the phone in step with the Mac without asking a machine the same question
     * every fifteen seconds.
     */
    @Test
    fun `a hash is followed once, and only when it is news`() {
        // An agent that says nothing about a setup has nothing to fetch.
        assertFalse(shouldFetchSetup(reported = null, applied = "a", lastSeen = null))
        assertFalse(shouldFetchSetup(reported = "", applied = null, lastSeen = null))

        // The phone is already running the document this machine is carrying.
        assertFalse(shouldFetchSetup(reported = "a", applied = "a", lastSeen = null))

        // It is carrying something else, and nothing has been fetched from it yet.
        assertTrue(shouldFetchSetup(reported = "b", applied = "a", lastSeen = null))
        assertTrue(shouldFetchSetup(reported = "b", applied = null, lastSeen = "a"))

        // The same document that was fetched a moment ago and did not hold up. Asking again would
        // be refused the same way, once every poll, for as long as it stays wrong.
        assertFalse(shouldFetchSetup(reported = "b", applied = "a", lastSeen = "b"))
    }
}
