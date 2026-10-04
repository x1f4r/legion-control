package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.AgentClient
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.Platform
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.data.HostTrustSnapshot
import com.x1f4r.legioncontrol.data.MachineHints
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import net.schmizz.sshj.Config
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.connection.ConnectionException
import net.schmizz.sshj.connection.channel.direct.Session
import net.schmizz.sshj.transport.TransportException
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import net.schmizz.sshj.userauth.UserAuthException
import net.schmizz.sshj.userauth.keyprovider.KeyProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.lang.reflect.Proxy
import java.net.ConnectException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.security.KeyPairGenerator
import java.util.Collections
import java.util.concurrent.CountDownLatch

/**
 * The ssh transport against an sshj client that never opens a socket.
 *
 * What is being checked is the order of events around the library, which is where a change gets
 * sent twice: whether a failure is reported as before or after the command was handed over, on which
 * thread the connection is closed, and what is kept of a far side that will not stop printing.
 */
class SshTransportTest {

    private val endpoint = Endpoint(
        id = "lan",
        kind = RouteKind.LAN,
        host = "192.0.2.10",
        port = 22,
        user = "me",
        systemHint = null,
        label = "Home LAN",
    )

    /** An sshj client whose network steps are whatever the test says they are. */
    private class FakeClient(
        config: Config,
        private val onConnect: (FakeClient) -> Unit = {},
        private val onAuthenticate: () -> Unit = {},
        private val onSession: () -> Session = { error("no session in this test") },
        private val onClose: () -> Unit = {},
    ) : SSHClient(config) {
        val closedOn: MutableList<String> = Collections.synchronizedList(mutableListOf())
        var verifier: HostKeyVerifier? = null

        override fun addHostKeyVerifier(verifier: HostKeyVerifier) {
            this.verifier = verifier
        }

        override fun connect(hostname: String?, port: Int) = onConnect(this)

        override fun authPublickey(username: String?, vararg keyProviders: KeyProvider?) = onAuthenticate()

        override fun startSession(): Session = onSession()

        override fun close() {
            closedOn += Thread.currentThread().name
            onClose()
        }
    }

    private val clients = Collections.synchronizedList(mutableListOf<FakeClient>())

    private fun transport(capBytes: Int = SshTransport.OUTPUT_CAP_BYTES, client: (Config) -> FakeClient) = SshTransport(
        keys = { proxy(KeyProvider::class.java) { _, _ -> error("the fake client never asks for the key") } },
        verifierFor = { address -> PinnedHostKeyVerifier({ HostTrustSnapshot(0, emptyList(), emptyList()) }, address) },
        machineName = "Tower",
        newClient = { config -> client(config).also { clients += it } },
        outputCapBytes = capBytes,
    )

    private fun session(exec: (String) -> Session.Command): Session = proxy(Session::class.java) { name, args ->
        when (name) {
            "exec" -> exec(args!![0] as String)
            else -> null
        }
    }

    private fun command(stdout: InputStream, stderr: InputStream = ByteArrayInputStream(ByteArray(0)), exit: Int? = 0): Session.Command =
        proxy(Session.Command::class.java) { name, _ ->
            when (name) {
                "getInputStream" -> stdout
                "getErrorStream" -> stderr
                "getOutputStream" -> ByteArrayOutputStream()
                "getExitStatus" -> exit
                else -> null
            }
        }

    @Suppress("UNCHECKED_CAST")
    private fun <T> proxy(type: Class<T>, handle: (String, Array<Any?>?) -> Any?): T =
        Proxy.newProxyInstance(type.classLoader, arrayOf(type)) { _, method, args -> handle(method.name, args) } as T

    private inline fun <reified T : AgentFailure> failsWith(block: () -> Unit): T {
        try {
            block()
        } catch (failure: AgentFailure) {
            assertTrue("expected ${T::class.simpleName}, got $failure", failure is T)
            return failure as T
        }
        throw AssertionError("expected ${T::class.simpleName}")
    }

    @Test
    fun `an exec that throws may have started the command, and a mutation is sent once`() = runBlocking {
        // A3. The exec request is on the wire the moment exec starts writing it. The flag used to be
        // set after exec returned, so this failure read as never sent and the next address got the
        // same restart.
        val ssh = transport { config ->
            FakeClient(config, onSession = { session { throw ConnectionException("channel closed by the far side") } })
        }
        val failure = failsWith<AgentFailure.Unreachable> { ssh.run(endpoint, "node agent.mjs restart", 10_000L) }
        assertEquals(DispatchStage.AMBIGUOUS, failure.dispatch)
        assertEquals(FailureKind.LINK_LOST, failure.kind)

        clients.clear()
        val machine = Machine(
            id = "tower",
            name = "Tower",
            endpoints = listOf(endpoint, endpoint.copy(id = "tailnet", kind = RouteKind.REMOTE, host = "100.64.0.10")),
            systems = listOf(
                MachineSystem("linux", "Linux", Platform.LINUX, listOf("/usr/bin/node", "/srv/agent.mjs"), RemoteShell.POSIX),
                MachineSystem("windows", "Windows", Platform.WINDOWS, listOf("node", "C:\\agent.mjs"), RemoteShell.CMD),
            ),
            wake = null,
        )
        val hints = NoHints()
        val agent = AgentClient(machine, ssh, RouteSelector(machine, hints), hints)
        failsWith<AgentFailure.Unreachable> { agent.restart("t3") }
        assertEquals("one connection, one attempt, on any number of routes and systems", 1, clients.size)
    }

    @Test
    fun `failures before the exec request are named by what failed and were never sent`() = runBlocking {
        val cases = listOf(
            ConnectException("Connection refused") to FailureKind.CONNECTION_REFUSED,
            UnknownHostException("desk-tower.local") to FailureKind.HOST_UNREACHABLE,
            SocketTimeoutException("connect timed out") to FailureKind.HOST_UNREACHABLE,
            TransportException("key exchange failed") to FailureKind.CONNECTION_REFUSED,
        )
        for ((thrown, kind) in cases) {
            val ssh = transport { config -> FakeClient(config, onConnect = { throw thrown }) }
            val failure = failsWith<AgentFailure.Unreachable> { ssh.run(endpoint, "status", 10_000L) }
            assertEquals(thrown.toString(), kind, failure.kind)
            assertEquals(DispatchStage.NOT_STARTED, failure.dispatch)
        }

        val refusedKey = transport { config -> FakeClient(config, onAuthenticate = { throw UserAuthException("publickey") }) }
        assertEquals(
            FailureKind.AUTHENTICATION_FAILED,
            failsWith<AgentFailure.NotAuthorised> { refusedKey.run(endpoint, "status", 10_000L) }.kind,
        )

        val noSession = transport { config -> FakeClient(config, onSession = { throw ConnectionException("open failed") }) }
        val failure = failsWith<AgentFailure.Unreachable> { noSession.run(endpoint, "status", 10_000L) }
        assertEquals(FailureKind.CONNECTION_REFUSED, failure.kind)
        assertEquals(DispatchStage.NOT_STARTED, failure.dispatch)
    }

    @Test
    fun `an unknown host key is a question about a key, before anything is sent`() = runBlocking {
        val key = KeyPairGenerator.getInstance("RSA").apply { initialize(1024) }.generateKeyPair().public
        val ssh = transport { config ->
            FakeClient(config, onConnect = { client ->
                assertFalse(client.verifier!!.verify(endpoint.host, endpoint.port, key))
                throw TransportException("host key refused")
            })
        }
        val failure = failsWith<AgentFailure.HostKeyChanged> { ssh.run(endpoint, "status", 10_000L) }
        assertTrue(failure.isFirstContact)
        assertEquals(FailureKind.HOST_KEY_UNKNOWN, failure.kind)
        assertEquals(DispatchStage.NOT_STARTED, failure.dispatch)
    }

    @Test
    fun `an address outside the dial grammar is never dialled`() = runBlocking {
        // T5. `victim.example,*` would be a wildcard pin in a known_hosts file, and a host string that
        // never passed the grammar is never one a key is written down for.
        val ssh = transport { config -> FakeClient(config) }
        listOf(
            endpoint.copy(host = "victim.example,*"),
            endpoint.copy(host = "-oProxyCommand=x"),
            endpoint.copy(user = "me;id"),
            endpoint.copy(port = 0),
        ).forEach { odd ->
            val failure = failsWith<AgentFailure.BadRequest> { ssh.run(odd, "status", 10_000L) }
            assertEquals(DispatchStage.NOT_STARTED, failure.dispatch)
        }
        assertTrue(clients.isEmpty())
    }

    @Test
    fun `the connection is closed on the IO dispatcher, never on the caller's thread`() = runBlocking {
        // A8. Closing writes a disconnect and can block on a dead link, and the caller is a screen.
        val caller = Thread.currentThread().name
        val ssh = transport { config ->
            FakeClient(config, onSession = { session { command(ByteArrayInputStream("{\"ok\":true}".toByteArray())) } })
        }
        val outcome = ssh.run(endpoint, "status", 10_000L)
        assertEquals("{\"ok\":true}", outcome.stdout)
        val closedOn = clients.single().closedOn
        assertTrue(closedOn.isNotEmpty())
        closedOn.forEach { assertTrue("closed on $it, called from $caller", it != caller && it.startsWith("DefaultDispatcher-worker")) }
    }

    @Test
    fun `cancelling a running command closes the connection on IO and lets the read unwind`() = runBlocking {
        val stdout = HangingStream("{\"partial\":".toByteArray())
        val executing = CompletableDeferred<Unit>()
        val ssh = transport { config ->
            FakeClient(
                config,
                onSession = {
                    session {
                        executing.complete(Unit)
                        command(stdout)
                    }
                },
                onClose = { stdout.release() },
            )
        }
        val caller = Thread.currentThread().name
        val call = launch(start = CoroutineStart.UNDISPATCHED) { ssh.run(endpoint, "update", 600_000L) }
        executing.await()
        call.cancel()
        call.join()
        assertTrue(call.isCancelled)
        val closedOn = clients.single().closedOn
        assertTrue(closedOn.isNotEmpty())
        closedOn.forEach { assertTrue("closed on $it", it != caller && it.startsWith("DefaultDispatcher-worker")) }
    }

    @Test
    fun `a command past its own budget is reported as timed out, with what it printed`() = runBlocking {
        val stdout = HangingStream("still working".toByteArray())
        val ssh = transport { config ->
            FakeClient(config, onSession = { session { command(stdout, exit = null) } }, onClose = { stdout.release() })
        }
        val outcome = ssh.run(endpoint, "update", 300L)
        assertTrue(outcome.timedOut)
        assertEquals("still working", outcome.stdout)
        assertEquals(null, outcome.exitStatus)
    }

    @Test
    fun `a missing exit status is passed through as evidence`() = runBlocking {
        val ssh = transport { config ->
            FakeClient(config, onSession = { session { command(ByteArrayInputStream(ByteArray(0)), exit = null) } })
        }
        val outcome = ssh.run(endpoint, "boot windows", 10_000L)
        assertEquals(null, outcome.exitStatus)
        assertFalse(outcome.timedOut)
    }

    @Test
    fun `output past the cap is read to its end and discarded, and the cut stdout is marked`() = runBlocking {
        // T11. A far side that prints without end must not take the phone's memory with it, and
        // what is left of a cut reply is not a reply.
        val ssh = transport(capBytes = 16) { config ->
            FakeClient(config, onSession = {
                session {
                    command(
                        stdout = ByteArrayInputStream("{\"ok\":true}".padEnd(4096, ' ').toByteArray()),
                        stderr = ByteArrayInputStream("e".repeat(4096).toByteArray()),
                    )
                }
            })
        }
        val outcome = ssh.run(endpoint, "status", 10_000L)
        assertEquals(16, outcome.stdout.length)
        assertEquals(16, outcome.stderr.length)
        assertTrue(outcome.stdoutTruncated)
        assertEquals(16 * 1024 * 1024, SshTransport.OUTPUT_CAP_BYTES)
    }

    /** A stream that gives its prefix and then blocks until the connection is closed under it. */
    private class HangingStream(private val prefix: ByteArray) : InputStream() {
        private var position = 0
        private val closed = CountDownLatch(1)

        fun release() = closed.countDown()

        override fun read(): Int {
            val one = ByteArray(1)
            return if (read(one, 0, 1) < 0) -1 else one[0].toInt() and 0xff
        }

        override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
            if (position < prefix.size) {
                val count = minOf(length, prefix.size - position)
                System.arraycopy(prefix, position, buffer, offset, count)
                position += count
                return count
            }
            closed.await()
            return -1
        }
    }

    private class NoHints : MachineHints {
        private val nothing = MutableStateFlow<String?>(null)
        override fun lastGoodRoute(machineId: String): StateFlow<String?> = nothing
        override fun rememberRoute(machineId: String, id: String) = Unit
        override fun forgetRoute(machineId: String) = Unit
        override fun lastKnownSystem(machineId: String): StateFlow<String?> = nothing
        override fun rememberSystem(machineId: String, systemId: String) = Unit
    }
}
