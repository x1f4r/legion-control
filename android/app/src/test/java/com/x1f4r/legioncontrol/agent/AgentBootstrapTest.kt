package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.MachineHints
import com.x1f4r.legioncontrol.data.sha256Hex
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.CommandTransport
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteKind
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.StdinSource
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * The legacy bootstrap, gated as the Mac's `AgentBootstrap.swift` gates it.
 *
 * It is the one path in the app that runs code a machine has only just received, so what matters
 * most here is when it is not offered: before an authenticated status, through a restricted key, on a
 * system this device has not approved, and on a command whose installation base cannot be told. When
 * it does run, the scripts are the shared ones byte for byte, the upload lands inside the base, and
 * the install is handed the size and hash it checks before tar reads anything.
 */
class AgentBootstrapTest {

    @get:Rule
    val folder = TemporaryFolder()

    private fun status(json: String): AgentStatus = AgentJson.decodeFromString(AgentStatus.serializer(), json)

    private val legacy = status("""{"ok":true,"agentVersion":"2.1.0","system":{"id":"linux"}}""")
    private val v3 = status("""{"ok":true,"contract":3,"agent":{"version":"3.0.1","contract":3}}""")

    private val linux = MachineSystem(
        id = "linux",
        name = "Linux",
        platform = Platform.LINUX,
        agent = listOf("/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"),
        grammar = RemoteShell.POSIX,
    )

    private val windows = MachineSystem(
        id = "windows",
        name = "Windows",
        platform = Platform.WINDOWS,
        agent = listOf("C:\\Program Files\\nodejs\\node.exe", "C:\\Users\\me\\.legion-control\\bin\\launcher.mjs"),
        grammar = RemoteShell.CMD,
    )

    private fun decide(status: AgentStatus?, system: MachineSystem?) =
        AgentDeploymentPath.decide(status?.let { AgentAbilities.of(it) } ?: AgentAbilities.unknown, status, system)

    private fun reason(path: AgentDeploymentPath): String =
        (path as? AgentDeploymentPath.Unavailable)?.reason ?: throw AssertionError("expected unavailable, got $path")

    // MARK: - The scripts

    @Test
    fun `the scripts are the shared ones byte for byte`() {
        assertArrayEquals(ContractCorpus.file("bootstrap/prepare.cjs").readBytes(), AgentBootstrap.PREPARE.toByteArray())
        assertArrayEquals(ContractCorpus.file("bootstrap/install.cjs").readBytes(), AgentBootstrap.INSTALL.toByteArray())
    }

    @Test
    fun `a script joined onto one line is the same program`() {
        // Section 15.2 refuses a line break in every grammar, so a script travels with its lines
        // joined. That is only the same program while every line is a whole statement and nothing
        // runs to the end of a line, which is what this holds the shared files to.
        for (script in listOf(AgentBootstrap.PREPARE, AgentBootstrap.INSTALL)) {
            for (line in script.lines()) {
                assertTrue("every line ends a statement: $line", line.endsWith(";"))
                assertFalse("no comment runs to the end of a line: $line", "//" in line || "/*" in line)
                assertFalse("no template literal spans lines: $line", '`' in line)
            }
            val joined = AgentBootstrap.onCommandLine(script)
            assertEquals(script.replace('\n', ' '), joined)
            assertFalse(joined.any { it.code < 0x20 || it.code == 0x7f })
        }
    }

    // MARK: - Recognising a legacy agent and its base

    @Test
    fun `only an authenticated status from an actual 1_x or 2_x agent counts as legacy`() {
        assertTrue(AgentBootstrap.isAuthenticatedLegacy(legacy))
        assertTrue(AgentBootstrap.isAuthenticatedLegacy(status("""{"ok":true,"agentVersion":"1.4.2"}""")))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(null))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(status("""{"ok":false,"agentVersion":"2.1.0"}""")))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(status("""{"agentVersion":"2.1.0"}""")))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(v3))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(status("""{"ok":true,"agentVersion":"2.1.0","contract":3}""")))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(status("""{"ok":true}""")))
        assertFalse(AgentBootstrap.isAuthenticatedLegacy(status("""{"ok":true,"agentVersion":"3.0.0"}""")))
    }

    @Test
    fun `the base is read only off the two installed shapes, and only when it is absolute`() {
        val posix = AgentBootstrap.layout(listOf("/opt/node", "/home/me/agent base/agent/src/index.mjs"))
        assertEquals(AgentBootstrap.Layout("/home/me/agent base", listOf("/opt/node")), posix)

        val launcher = AgentBootstrap.layout(listOf("/usr/bin/node", "/home/me/.legion-control/bin/launcher.mjs"))
        assertEquals("/home/me/.legion-control", launcher?.base)

        val windowsLayout = AgentBootstrap.layout(listOf("C:\\Node\\node.exe", "C:\\Users\\me\\agent base\\bin\\launcher.mjs"))
        assertEquals("C:\\Users\\me\\agent base", windowsLayout?.base)
        assertEquals(
            "C:/Users/me/agent base/incoming/legion-bootstrap-0a1b2c3d.tgz",
            AgentBootstrap.archivePath(windowsLayout!!, "0a1b2c3d"),
        )

        assertNull(AgentBootstrap.layout(listOf("node", "/custom/control.mjs")))
        assertNull(AgentBootstrap.layout(listOf("/usr/bin/node")))
        assertNull(AgentBootstrap.layout(listOf("/usr/bin/node", "~/.legion-control/agent/src/index.mjs")))
        assertNull(AgentBootstrap.layout(listOf("/usr/bin/node", "/home/me/../other/agent/src/index.mjs")))
        // The first-device fetch shape: a shell, its script and the script's name. Nothing to read.
        assertNull(
            AgentBootstrap.layout(
                listOf("/bin/sh", "-c", "exec node \"\$HOME/.legion-control/agent/src/index.mjs\" \"\$@\"", "legionctl"),
            ),
        )

        assertFalse(AgentBootstrap.isAbsolute("~/base"))
        assertFalse(AgentBootstrap.isAbsolute("relative/base"))
        assertFalse(AgentBootstrap.isAbsolute("/home/me/"))
        assertFalse(AgentBootstrap.isAbsolute("/home/me\nother"))
        assertTrue(AgentBootstrap.isAbsolute("/home/me"))
        assertTrue(AgentBootstrap.isAbsolute("D:\\lc"))
    }

    // MARK: - The gate

    @Test
    fun `nothing is offered before an authenticated status`() {
        // The defect: a contract that had not been read was zero, below 3, and that picked the
        // legacy upload for a machine nobody had heard from.
        assertEquals("Read an authenticated agent status first.", reason(decide(null, linux)))
        assertEquals(
            "Read an authenticated agent status first.",
            reason(decide(status("""{"ok":false,"agentVersion":"2.1.0"}"""), linux)),
        )
        assertNull(AgentAbilities.unknown.contract)
        assertFalse(AgentAbilities.unknown.speaksV3)
    }

    @Test
    fun `a contract 3 agent takes the bundle on standard input, restricted key or not`() {
        assertEquals(AgentDeploymentPath.Stdin, decide(v3, linux))
        val restricted = status("""{"ok":true,"contract":3,"agent":{"contract":3,"restrictedSession":true}}""")
        assertEquals(AgentDeploymentPath.Stdin, decide(restricted, linux.copy(grammar = RemoteShell.DISPATCH)))
    }

    @Test
    fun `a status that does not name a legacy version is not bootstrapped`() {
        assertTrue(decide(status("""{"ok":true}"""), linux) is AgentDeploymentPath.Unavailable)
        assertTrue(decide(status("""{"ok":true,"agentVersion":"9.0.0"}"""), linux) is AgentDeploymentPath.Unavailable)
    }

    @Test
    fun `a restricted key or a system this device does not drive is refused`() {
        assertEquals("Needs an administrator connection.", reason(decide(legacy, linux.copy(grammar = RemoteShell.DISPATCH))))
        val restrictedSession = status("""{"ok":true,"agentVersion":"2.1.0","agent":{"restrictedSession":true}}""")
        assertEquals("Needs an administrator connection.", reason(decide(restrictedSession, linux)))
        assertEquals("Not approved on this device.", reason(decide(legacy, linux.copy(launchState = LaunchState.NOT_DRIVEN))))
        assertTrue(decide(legacy, null) is AgentDeploymentPath.Unavailable)
    }

    @Test
    fun `only a POSIX system with a recognised base is bootstrapped`() {
        assertEquals("Only POSIX systems are bootstrapped from the phone.", reason(decide(legacy, windows)))
        assertEquals(
            "The installation base cannot be derived from this command.",
            reason(decide(legacy, linux.copy(agent = listOf("node", "/custom/control.mjs")))),
        )

        val path = decide(legacy, linux)
        assertTrue(path is AgentDeploymentPath.UploadAndInstall)
        path as AgentDeploymentPath.UploadAndInstall
        assertEquals(linux, path.system)
        assertEquals(AgentBootstrap.Layout("/home/me/.legion-control", listOf("/usr/bin/node")), path.layout)
    }

    // MARK: - Running it

    private class Hints : MachineHints {
        private val routes = mutableMapOf<String, MutableStateFlow<String?>>()
        private val systems = mutableMapOf<String, MutableStateFlow<String?>>()

        override fun lastGoodRoute(machineId: String): StateFlow<String?> = routes.getOrPut(machineId) { MutableStateFlow(null) }
        override fun rememberRoute(machineId: String, id: String) {
            routes.getOrPut(machineId) { MutableStateFlow(null) }.value = id
        }
        override fun forgetRoute(machineId: String) {
            routes.getOrPut(machineId) { MutableStateFlow(null) }.value = null
        }
        override fun lastKnownSystem(machineId: String): StateFlow<String?> = systems.getOrPut(machineId) { MutableStateFlow(null) }
        override fun rememberSystem(machineId: String, systemId: String) {
            systems.getOrPut(machineId) { MutableStateFlow(null) }.value = systemId
        }
    }

    /** Every line run and every file uploaded, in the order they happened. */
    private class Recorder(
        private val answer: (line: String) -> CommandOutcome,
        private val uploadFailure: AgentFailure? = null,
    ) : CommandTransport {
        val events = mutableListOf<String>()
        val lines = mutableListOf<String>()
        val uploads = mutableListOf<Pair<String, String>>()

        override suspend fun run(endpoint: Endpoint, command: String, timeoutMillis: Long, stdin: StdinSource?): CommandOutcome {
            events += "run"
            lines += command
            return answer(command)
        }

        override suspend fun upload(endpoint: Endpoint, source: File, remotePath: String, timeoutMillis: Long) {
            events += "upload"
            uploadFailure?.let { throw it }
            uploads += source.path to remotePath
        }
    }

    private val prepared = CommandOutcome(0, """{"ok":true}""", "")
    private val installed = CommandOutcome(0, """{"ok":true,"contract":3,"agentVersion":"3.1.0","action":"installed"}""", "")
    private val operation = "0a1b2c3d"
    private val layout = AgentBootstrap.Layout("/home/me/.legion-control", listOf("/usr/bin/node"))

    /** A tarball as a verified bundle hands it over: the file, and the hash and size its signature gave. */
    private fun tarball(size: Long = 1234): SignedTarball {
        val file = folder.newFile("legionctl-agent-3.1.0.tgz").apply { writeBytes(ByteArray(1234) { it.toByte() }) }
        return SignedTarball(file, sha256Hex(file.readBytes()), size)
    }

    private fun client(transport: CommandTransport): AgentClient {
        val machine = Machine(
            id = "tower",
            name = "Tower",
            endpoints = listOf(
                Endpoint(id = "lan", kind = RouteKind.LAN, host = "192.0.2.10", port = 22, user = "me", systemHint = null, label = "lan"),
            ),
            systems = listOf(linux, windows),
            wake = null,
        )
        val hints = Hints()
        return AgentClient(machine, transport, RouteSelector(machine, hints) { 0L }, hints) { 0L }
    }

    private fun isPrepare(line: String) = line.contains("bootstrap refused")

    @Test
    fun `prepare, upload into the base, then install with the size and hash it checks`() = runBlocking {
        val archive = tarball()
        val transport = Recorder({ line -> if (isPrepare(line)) prepared else installed })
        val reply = client(transport).bootstrapLegacy(linux, layout, archive, operation)

        assertEquals("installed", reply.value.action)
        assertEquals(listOf("run", "upload", "run"), transport.events)

        val remote = "/home/me/.legion-control/incoming/legion-bootstrap-$operation.tgz"
        assertEquals(listOf(archive.file.path to remote), transport.uploads)
        assertEquals(
            buildRemoteCommand(RemoteShell.POSIX, listOf("/usr/bin/node", "-e", AgentBootstrap.onCommandLine(AgentBootstrap.PREPARE), "--", "/home/me/.legion-control")),
            transport.lines[0],
        )
        assertEquals(
            buildRemoteCommand(
                RemoteShell.POSIX,
                listOf(
                    "/usr/bin/node", "-e", AgentBootstrap.onCommandLine(AgentBootstrap.INSTALL), "--",
                    "/home/me/.legion-control", remote, archive.sha256, "1234", operation,
                ),
            ),
            transport.lines[1],
        )
    }

    @Test
    fun `the install checks the upload against the signed hash, not one read back from the file`() = runBlocking {
        // Every screen loads the bundle and writes it to the same cache path. Hashed again at install
        // time, a file being rewritten underneath was sent with its own hash, and the far side's
        // check passed whatever arrived.
        val signedHash = "ab".repeat(32)
        val archive = tarball().copy(sha256 = signedHash)
        val transport = Recorder({ line -> if (isPrepare(line)) prepared else installed })
        client(transport).bootstrapLegacy(linux, layout, archive, operation)

        val install = transport.lines.last()
        assertTrue(install.contains(" $signedHash 1234 $operation"))
        assertFalse(install.contains(sha256Hex(archive.file.readBytes())))
    }

    @Test
    fun `a bundle file that is not the signed size is refused before anything is sent`() = runBlocking {
        val transport = Recorder({ prepared })
        try {
            client(transport).bootstrapLegacy(linux, layout, tarball(size = 2048), operation)
            fail("a file that is not the signed size has to stop the bootstrap")
        } catch (stopped: AgentFailure.BootstrapStopped) {
            assertEquals(DispatchStage.NOT_STARTED, stopped.dispatch)
            assertTrue(stopped.detail!!.contains("2048"))
        }
        assertTrue(transport.events.isEmpty())
    }

    @Test
    fun `a refused preflight stops it before anything is uploaded, and says nothing was installed`() = runBlocking {
        val transport = Recorder({
            CommandOutcome(1, "", "Error: No installed agent exists in this base; bootstrap refused")
        })
        try {
            client(transport).bootstrapLegacy(linux, layout, tarball(), operation)
            fail("a refused preflight has to stop the bootstrap")
        } catch (stopped: AgentFailure.BootstrapStopped) {
            assertEquals(DispatchStage.NOT_STARTED, stopped.dispatch)
            assertTrue(stopped.detail!!.contains("No installed agent exists"))
        }
        assertEquals(listOf("run"), transport.events)
    }

    @Test
    fun `an upload that did not arrive stops it before the install is sent`() = runBlocking {
        val transport = Recorder(
            answer = { prepared },
            uploadFailure = AgentFailure.Unreachable("Tower", "link dropped", stage = DispatchStage.AMBIGUOUS),
        )
        try {
            client(transport).bootstrapLegacy(linux, layout, tarball(), operation)
            fail("a failed upload has to stop the bootstrap")
        } catch (stopped: AgentFailure.BootstrapStopped) {
            assertEquals(DispatchStage.NOT_STARTED, stopped.dispatch)
        }
        assertEquals(listOf("run", "upload"), transport.events)
    }

    @Test
    fun `an install whose link dropped is never sent again`() = runBlocking {
        val transport = Recorder({ line ->
            if (isPrepare(line)) prepared else throw AgentFailure.Unreachable("Tower", "link dropped", stage = DispatchStage.AMBIGUOUS)
        })
        try {
            client(transport).bootstrapLegacy(linux, layout, tarball(), operation)
            fail("a dropped install has to be reported")
        } catch (failure: AgentFailure) {
            assertFalse(failure is AgentFailure.BootstrapStopped)
            assertTrue(failure.dispatch.mayHaveRun)
        }
        assertEquals(listOf("run", "upload", "run"), transport.events)
    }

    @Test
    fun `the steps run as the one system that asked for them, never as another`() = runBlocking {
        // The interpreter was not found. On an ordinary command the walk could try the machine's
        // other system; a bootstrap step is written for one system and stops there.
        val transport = Recorder({ CommandOutcome(127, "", "sh: 1: /usr/bin/node: not found") })
        try {
            client(transport).bootstrapLegacy(linux, layout, tarball(), operation)
            fail("a preflight that never ran has to stop the bootstrap")
        } catch (_: AgentFailure.BootstrapStopped) {
        }
        assertEquals(1, transport.lines.size)
        assertTrue(transport.lines.single().startsWith("/usr/bin/node -e "))
    }

    @Test
    fun `an operation id that cannot name the upload sends nothing`() = runBlocking {
        val transport = Recorder({ prepared })
        try {
            client(transport).bootstrapLegacy(linux, layout, tarball(), "../evil")
            fail("an id outside the grammar has to be refused")
        } catch (refused: AgentFailure.BadRequest) {
            assertEquals(DispatchStage.NOT_STARTED, refused.dispatch)
        }
        assertTrue(transport.events.isEmpty())
    }
}
