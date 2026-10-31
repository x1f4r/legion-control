package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.CanonicalSetup
import com.x1f4r.legioncontrol.data.readControllerConfig
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.CommandTransport
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.LanPrefix
import com.x1f4r.legioncontrol.net.RouteKind
import com.x1f4r.legioncontrol.net.StdinSource
import com.x1f4r.legioncontrol.net.TAILNET
import kotlinx.coroutines.runBlocking
import java.io.File
import java.nio.file.Files
import java.util.Base64
import org.junit.Assert.assertSame
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
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
        val commands = setupCommands()

        // The layouts the installers write, and nothing else. This list exists for the one machine
        // that is not in any configuration yet, because it is the machine the configuration is
        // about to come from; everything else is dialled with the argv the document names. The home
        // directory is the far side's own $HOME, expanded by a shell that is asked for by name,
        // because a bare ~ is a literal directory now.
        assertEquals(
            listOf(
                "/bin/sh -c 'exec \"\$HOME/.legion-control/bin/legionctl\" \"\$@\"' legionctl config",
                "@\"powershell.exe\" \"-NoProfile\" \"-NonInteractive\" \"-ExecutionPolicy\" \"Bypass\" " +
                    "\"-EncodedCommand\" \"${commands[1].arguments.last()}\"",
                "& 'powershell.exe' '-NoProfile' '-NonInteractive' '-ExecutionPolicy' 'Bypass' " +
                    "'-EncodedCommand' '${commands[2].arguments.last()}'",
                "/bin/sh -c 'exec node \"\$HOME/.legion-control/agent/src/index.mjs\" \"\$@\"' legionctl config",
            ),
            commands.map { it.line() },
        )
        assertEquals(
            listOf(RemoteShell.POSIX, RemoteShell.CMD, RemoteShell.POWERSHELL, RemoteShell.POSIX),
            commands.map { it.grammar },
        )
        assertTrue("no shape carries a tilde", commands.flatMap { it.arguments }.none { it.contains('~') })
    }

    @Test
    fun `both Windows shapes run the same fixed script, whatever the login user is`() {
        val commands = setupCommands()
        val windows = commands[1]
        assertEquals(windows.arguments, commands[2].arguments)
        assertEquals("powershell.exe", windows.arguments.first())
        assertEquals("-EncodedCommand", windows.arguments[5])
        val script = String(Base64.getDecoder().decode(windows.arguments.last()), Charsets.UTF_16LE)
        assertTrue(script.startsWith("\$ErrorActionPreference = 'Stop'; try {"))
        assertTrue(script.contains("& (Join-Path \$HOME '.legion-control/bin/legionctl.ps1') config"))
        assertTrue(script.contains("exit \$LASTEXITCODE"))
        assertTrue(script.endsWith("exit 1 }"))
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
            val process = ProcessBuilder("/bin/sh", "-c", setupCommands().first().line()!!)
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
    fun `bootstrap stops at the first reply and moves on from a shell that could not start it`() = runBlocking {
        val tried = mutableListOf<String>()
        val powershell = setupCommands()[2].line()
        val result = fetchController { line ->
            tried += line
            if (line == powershell) {
                CommandOutcome(0, document, "")
            } else {
                CommandOutcome(9009, "", "'powershell.exe' is not recognized as an internal or external command")
            }
        }
        assertTrue(result is ControllerFetch.Document)
        assertEquals(setupCommands().take(3).map { it.line() }, tried)
        var count = 0
        assertEquals(ControllerFetch.NothingStored, fetchController {
            count++
            CommandOutcome(0, "{\"controller\":null}", "")
        })
        assertEquals(1, count)
    }

    @Test
    fun `explicit agent refusal ends discovery without claiming an old or absent agent`() = runBlocking {
        var attempts = 0
        val failure = try {
            fetchController {
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
            fetchController {
                attempts++
                CommandOutcome(127, "", "runtime unavailable " + "x".repeat(500))
            }
            error("Expected discovery failure")
        } catch (failure: AgentFailure.DiscoveryFailed) { failure }
        assertEquals(setupCommands().size, attempts)
        assertEquals("Could not locate or start the control agent on that machine.", failure.summary)
        assertFalse(failure.summary.contains("not installed"))
        assertEquals(403, failure.detail!!.length)
    }

    @Test
    fun `a lost link or a timeout ends discovery, because the address itself is in question`() = runBlocking {
        val endings = listOf(
            CommandOutcome(null, "", "") to AgentFailure.Unreachable::class.java,
            CommandOutcome(255, "", "") to AgentFailure.Unreachable::class.java,
            CommandOutcome(null, "partial", "", timedOut = true) to AgentFailure.TimedOut::class.java,
        )
        for ((outcome, expected) in endings) {
            var count = 0
            val failure = try {
                fetchController { count++; outcome }
                error("Expected a route-level failure")
            } catch (failure: AgentFailure) { failure }
            assertTrue("$outcome gave $failure", expected.isInstance(failure))
            assertTrue(failure.dispatch.mayHaveRun)
            assertEquals(1, count)
        }
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
                fetchController { count++; throw expected }
                error("Expected transport failure")
            } catch (failure: AgentFailure) { failure }
            assertSame(expected, actual)
            assertEquals(1, count)
        }
    }

    /**
     * Fetch from a machine dials an address typed by hand, often a tailnet one. With Tailscale off it
     * said the machine did not answer, as a machine in the setup used to.
     */
    @Test
    fun `a typed tailnet address no tunnel carries says the phone is off the tailnet`() = runBlocking {
        val endpoint = Endpoint("setup", RouteKind.REMOTE, "100.64.0.9", 22, "me", null, "100.64.0.9")
        val silent = object : CommandTransport {
            override suspend fun run(endpoint: Endpoint, command: String, timeoutMillis: Long, stdin: StdinSource?): CommandOutcome =
                throw AgentFailure.Unreachable(null, "100.64.0.9:22 did not answer in time.")

            override suspend fun upload(endpoint: Endpoint, source: File, remotePath: String, timeoutMillis: Long) = error("not used")
        }
        suspend fun fetchFailure(tunnels: List<LanPrefix>?): AgentFailure = try {
            fetchController(silent, endpoint, tunnels)
            error("Expected a failure")
        } catch (failure: AgentFailure) { failure }

        val failure = fetchFailure(tunnels = emptyList())
        assertTrue("$failure", failure is AgentFailure.NoTunnel)
        assertEquals("This phone is not connected to the tailnet. Is Tailscale on?", failure.summary)
        assertTrue(fetchFailure(tunnels = listOf(TAILNET)) is AgentFailure.Unreachable)
        assertTrue(fetchFailure(tunnels = null) is AgentFailure.Unreachable)
    }

    /**
     * C1: an agent from 3.1.0 on returns the document as it stores it, under `raw`, and that is what
     * is hashed and adopted. The shared fixture's `raw` hashes to its `hash`; a rebuild of its parsed
     * `controller` in this app's spelling would only match when that spelling happened to agree.
     */
    @Test
    fun `the stored text is taken verbatim when the agent sends it`() {
        val reply = ContractCorpus.json("fixtures/config.read.json") as kotlinx.serialization.json.JsonObject
        val fetched = readControllerReply(reply) as ControllerFetch.Document
        val raw = (reply["raw"] as kotlinx.serialization.json.JsonPrimitive).content
        val hash = (reply["hash"] as kotlinx.serialization.json.JsonPrimitive).content
        assertEquals(raw, fetched.text)
        assertEquals(hash, fetched.replyHash)
        assertEquals(hash, CanonicalSetup.hashOf(fetched.text))
    }

    /**
     * An agent before 3.1.0 sends no `raw`. The document is printed again in the order it arrived,
     * the way `JSON.stringify(controller, null, 2)` prints it, and the hash check alone says whether
     * that reproduced the bytes: here it does, because the stored document was written in that form.
     */
    @Test
    fun `without raw the document is rebuilt in received order and the hash decides`() {
        val reply = ContractCorpus.json("fixtures/config.read-without-raw.json") as kotlinx.serialization.json.JsonObject
        val fetched = readControllerReply(reply) as ControllerFetch.Document
        assertEquals((reply["hash"] as kotlinx.serialization.json.JsonPrimitive).content, fetched.replyHash)
        val withRaw = ContractCorpus.json("fixtures/config.read.json") as kotlinx.serialization.json.JsonObject
        assertEquals((withRaw["raw"] as kotlinx.serialization.json.JsonPrimitive).content, fetched.text)
        assertEquals(fetched.replyHash, CanonicalSetup.hashOf(fetched.text))
    }

    @Test
    fun `a reply that names no hash carries none, and so is never adopted`() {
        val fetched = readControllerReply("""{ "ok": true, "controller": { "version": 1, "machines": [] } }""") as ControllerFetch.Document
        assertNull(fetched.replyHash)
    }
}
