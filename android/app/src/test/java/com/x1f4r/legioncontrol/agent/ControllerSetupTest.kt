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
        assertEquals("9f2c1b0a", fetched.provenance.hash)

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
    fun `the command shapes are the standard installs, written for their own shells`() {
        val commands = setupCommands("me")

        // The layouts the installers write, and nothing else. This list exists for the one machine
        // that is not in any configuration yet, because it is the machine the configuration is
        // about to come from; everything else is dialled with the argv the document names.
        assertEquals(
            listOf(
                "/usr/bin/node /home/me/.legion-control/agent/src/index.mjs config",
                "node ~/.legion-control/agent/src/index.mjs config",
                "/opt/homebrew/bin/node ~/.legion-control/agent/src/index.mjs config",
                "node C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs config",
            ),
            // Distinct lines: nothing here needs quoting, so the cmd.exe and PowerShell forms are
            // the same string, and the fetch runs it once rather than twice.
            commands.mapNotNull { it.line() }.distinct(),
        )
    }

    @Test
    fun `a user name that needs quoting is written correctly, or the shape is skipped`() {
        // Unusual and not impossible, and the one value in these commands that comes from outside.
        val commands = setupCommands("first last")

        // The one POSIX shape that carries the user name quotes it rather than producing two
        // arguments. The other two use ~, which does not contain the name at all.
        val posix = commands.filter { it.shell == RemoteShell.POSIX }.mapNotNull { it.line() }
        assertTrue(
            posix.any { it.contains("'/home/first last/.legion-control/agent/src/index.mjs'") },
        )

        // A tilde is never quoted, because a quoted tilde is a directory literally called "~".
        assertTrue(posix.any { it.contains(" ~/.legion-control/agent/src/index.mjs ") })

        // The Windows shapes carry it in the two forms those shells actually read.
        val cmd = commands.first { it.shell == RemoteShell.CMD }.line()
        assertTrue(cmd!!.contains("\"C:\\Users\\first last\\.legion-control\\agent\\src\\index.mjs\""))
        val powershell = commands.first { it.shell == RemoteShell.POWERSHELL }.line()
        assertTrue(powershell!!.startsWith("& 'node' '"))
        assertTrue(powershell.contains("C:\\Users\\first last\\"))

        // Nothing that would run a second command can be produced from it.
        assertTrue(commands.mapNotNull { it.line() }.none { it.contains(";") })
    }

    /**
     * The rule that keeps a machine from being interrogated about the same copy on every poll.
     */
    @Test
    fun `a machine is asked about its ancestry once per distinct copy`() {
        // An agent that says nothing about a setup has nothing to ask about.
        assertFalse(shouldAskForMeta(reportedHash = null, applied = "a", asked = emptySet()))
        assertFalse(shouldAskForMeta(reportedHash = "", applied = null, asked = emptySet()))

        // This device already has the copy that machine is carrying.
        assertFalse(shouldAskForMeta(reportedHash = "a", applied = "a", asked = emptySet()))

        // A different copy, and nothing has been asked about it yet.
        assertTrue(shouldAskForMeta(reportedHash = "b", applied = "a", asked = emptySet()))

        // The same copy that was asked about a moment ago. A copy that is genuinely diverged gives
        // the same answer every time, so asking again is a round trip for nothing.
        assertFalse(shouldAskForMeta(reportedHash = "b", applied = "a", asked = setOf("b")))

        // A copy that has moved on since is a new question.
        assertTrue(shouldAskForMeta(reportedHash = "c", applied = "a", asked = setOf("b")))
    }
}
