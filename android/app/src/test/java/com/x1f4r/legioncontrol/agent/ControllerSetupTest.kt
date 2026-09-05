package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.readControllerConfig
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteKind
import kotlinx.coroutines.runBlocking
import java.nio.file.Files
import java.util.Base64
import org.junit.Assert.assertSame
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
            commands.drop(2).mapNotNull { it.line() }.distinct(),
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
        val cmd = commands.last { it.shell == RemoteShell.CMD }.line()
        assertTrue(cmd!!.contains("\"C:\\Users\\first last\\.legion-control\\agent\\src\\index.mjs\""))
        val powershell = commands.first { it.shell == RemoteShell.POWERSHELL }.line()
        assertTrue(powershell!!.startsWith("& 'node' '"))
        assertTrue(powershell.contains("C:\\Users\\first last\\"))

        // Nothing that would run a second command can be produced from it.
        assertTrue(commands.mapNotNull { it.line() }.none { it.contains(";") })
    }

    @Test
    fun `stable wrappers precede all runtime guesses and ignore the login user`() {
        val commands = setupCommands("me")
        assertEquals("~/.legion-control/bin/legionctl config", commands[0].line())
        val windows = commands[1]
        assertEquals("powershell.exe", windows.arguments.first())
        assertEquals("-EncodedCommand", windows.arguments[5])
        val script = String(Base64.getDecoder().decode(windows.arguments.last()), Charsets.UTF_16LE)
        assertTrue(script.startsWith("\$ErrorActionPreference = 'Stop'; try {"))
        assertTrue(script.contains("& (Join-Path \$HOME '.legion-control/bin/legionctl.ps1') config"))
        assertTrue(script.contains("exit \$LASTEXITCODE"))
        assertTrue(script.endsWith("exit 1 }"))
        assertEquals(windows.line(), buildRemoteCommand(RemoteShell.POWERSHELL, windows.arguments))
        assertEquals(commands.take(2), setupCommands("';$(touch injected);%USERPROFILE%!&").take(2))
    }

    @Test
    fun `POSIX bootstrap runs a wrapper with its pinned runtime and an empty PATH`() {
        val home = Files.createTempDirectory("setup home ").toFile()
        try {
            val runtime = home.resolve("pinned runtime").apply {
                writeText("#!/bin/sh\n[ \"\$1\" = config ] || exit 9\nprintf '%s' '{\"ok\":true,\"controller\":null}'\n")
                setExecutable(true)
            }
            home.resolve(".legion-control/bin").mkdirs()
            home.resolve(".legion-control/bin/legionctl").apply {
                writeText("#!/bin/sh\nexec " + quotePosix(runtime.absolutePath) + " \"\$@\"\n")
                setExecutable(true)
            }
            val process = ProcessBuilder("/bin/sh", "-c", setupCommands("unused").first().line()!!)
                .apply { environment()["HOME"] = home.absolutePath; environment()["PATH"] = "/missing" }
                .start()
            val stdout = process.inputStream.bufferedReader().readText()
            val stderr = process.errorStream.bufferedReader().readText()
            assertEquals(stderr, 0, process.waitFor())
            assertEquals(ControllerFetch.NothingStored, readControllerReply(stdout))
        } finally {
            home.deleteRecursively()
        }
    }

    @Test
    fun `bootstrap stops at the first reply and retains legacy fallback`() = runBlocking {
        val tried = mutableListOf<String>()
        val expectedLegacy = setupCommands("me")[2].line()
        val result = fetchController("me") { line ->
            tried += line
            if (line == expectedLegacy) CommandOutcome(0, document, "")
            else CommandOutcome(127, "", "command not found")
        }
        assertTrue(result is ControllerFetch.Document)
        assertEquals(setupCommands("me").take(3).map { it.line() }, tried)
        var count = 0
        assertEquals(ControllerFetch.NothingStored, fetchController("me") {
            count++
            CommandOutcome(0, "{\"controller\":null}", "")
        })
        assertEquals(1, count)
    }

    @Test
    fun `explicit agent refusal ends discovery without claiming an old or absent agent`() = runBlocking {
        var attempts = 0
        val failure = try {
            fetchController("me") {
                attempts++
                CommandOutcome(1, "{\"ok\":false,\"error\":\"restricted command refused\",\"controller\":null}", "")
            }
            error("Expected reported failure")
        } catch (failure: AgentFailure.Reported) { failure }
        assertEquals("restricted command refused", failure.detail)
        assertEquals(1, attempts)
    }

    @Test
    fun `exhausted bootstrap reports discovery uncertainty with bounded diagnostics`() = runBlocking {
        var attempts = 0
        val failure = try {
            fetchController("me") {
                attempts++
                CommandOutcome(127, "", "runtime unavailable " + "x".repeat(500))
            }
            error("Expected discovery failure")
        } catch (failure: AgentFailure.DiscoveryFailed) { failure }
        assertEquals(setupCommands("me").mapNotNull { it.line() }.distinct().size, attempts)
        assertEquals("Could not locate or start the control agent on that machine.", failure.summary)
        assertFalse(failure.summary.contains("not installed"))
        assertEquals(403, failure.detail!!.length)
    }

    @Test
    fun `bootstrap preserves transport authorization and trust failures without fallback`() = runBlocking {
        val endpoint = Endpoint("test", RouteKind.LAN, "example.test", 22, "me", null, "Test")
        val failures = listOf(
            AgentFailure.Unreachable(null, "socket refused"),
            AgentFailure.NotAuthorised(endpoint, null, "key refused"),
            AgentFailure.HostKeyChanged(endpoint.address, emptyList(), "SHA256:test", "public"),
            AgentFailure.TimedOut(40, "timeout"),
        )
        for (expected in failures) {
            var count = 0
            val actual = try {
                fetchController("me") { count++; throw expected }
                error("Expected transport failure")
            } catch (failure: AgentFailure) { failure }
            assertSame(expected, actual)
            assertEquals(1, count)
        }
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
