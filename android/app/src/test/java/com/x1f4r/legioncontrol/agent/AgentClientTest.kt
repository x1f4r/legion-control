package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.MachineHints
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.CommandTransport
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteKind
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.StdinSource
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.File

/**
 * The walk over routes and systems, against a transport that records every line it was handed.
 *
 * This is the part that decides whether one press can become two restarts, and before it had a fake
 * to run against it could only be checked by running the app against a machine. Every test here
 * counts what was sent, because "it did the right thing" and "it did the right thing twice" look
 * the same from the outside.
 */
class AgentClientTest {

    private var now = 1_000_000L

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

    /** One attempt, as the fake transport saw it. */
    private data class Sent(val endpoint: String, val line: String, val timeoutMillis: Long)

    private class FakeTransport(private val answer: suspend (Sent) -> CommandOutcome) : CommandTransport {
        val sent = mutableListOf<Sent>()

        override suspend fun run(
            endpoint: Endpoint,
            command: String,
            timeoutMillis: Long,
            stdin: StdinSource?,
        ): CommandOutcome {
            val attempt = Sent(endpoint.id, command, timeoutMillis)
            sent += attempt
            return answer(attempt)
        }

        override suspend fun upload(endpoint: Endpoint, source: File, remotePath: String, timeoutMillis: Long) =
            fail("not used")
    }

    private fun endpoint(id: String, host: String = "192.0.2.10") =
        Endpoint(id = id, kind = RouteKind.REMOTE, host = host, port = 22, user = "me", systemHint = null, label = id)

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

    private fun machine(
        endpoints: List<Endpoint> = listOf(endpoint("first"), endpoint("second", "192.0.2.11")),
        systems: List<MachineSystem> = listOf(linux, windows),
    ) = Machine(id = "tower", name = "Tower", endpoints = endpoints, systems = systems, wake = null)

    private fun client(machine: Machine, transport: CommandTransport): Pair<AgentClient, RouteSelector> {
        val hints = Hints()
        val routes = RouteSelector(machine, hints) { now }
        return AgentClient(machine, transport, routes, hints) { now } to routes
    }

    private val accepted = CommandOutcome(0, """{"ok":true,"contract":3,"action":"restarted"}""", "")
    private val v3Status = CommandOutcome(0, """{"ok":true,"contract":3,"system":{"id":"linux"}}""", "")

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
    fun `a mutation whose link dropped after the command was handed over is never sent again`() = runBlocking {
        val transport = FakeTransport {
            throw AgentFailure.Unreachable("Tower", "link dropped", null, DispatchStage.AMBIGUOUS)
        }
        val (agent, _) = client(machine(), transport)
        val failure = failsWith<AgentFailure.Unreachable> { agent.restart("t3") }
        assertEquals(DispatchStage.AMBIGUOUS, failure.dispatch)
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `a mutation moves on only from a shell that proved it never started the interpreter`() = runBlocking {
        // 127 with nothing on stdout is a shell saying it could not start node, so the command never
        // ran and the next system may be tried. The same machine, the same route.
        val transport = FakeTransport { sent ->
            if (sent.line.startsWith("/usr/bin/node")) CommandOutcome(127, "", "sh: 1: /usr/bin/node: not found") else accepted
        }
        val (agent, _) = client(machine(), transport)
        agent.restart("t3")
        assertEquals(listOf("first", "first"), transport.sent.map { it.endpoint })
        assertTrue(transport.sent[1].line.startsWith("@\"C:\\Program Files\\nodejs\\node.exe\""))
    }

    @Test
    fun `a mutation that printed Cannot find module or a PowerShell not-recognized is not sent anywhere else`() = runBlocking {
        // A4. Remote stderr is a sentence and never evidence: a program that printed this may have done
        // half of its work first, so the restart stops here and is reconciled rather than resent.
        val stderrs = listOf(
            1 to "Error: Cannot find module '/home/me/.legion-control/agent/src/index.mjs'",
            1 to "node : The term 'node' is not recognized as the name of a cmdlet, function, script file, or operable program.",
            1 to "bash: /home/me/missing: No such file or directory",
        )
        for ((exit, stderr) in stderrs) {
            val transport = FakeTransport { CommandOutcome(exit, "", stderr) }
            val (agent, _) = client(machine(), transport)
            val failure = try {
                agent.restart("t3")
                error("expected a failure")
            } catch (failure: AgentFailure) {
                failure
            }
            assertEquals(stderr, 1, transport.sent.size)
            assertTrue(stderr, failure.dispatch.mayHaveRun)
        }
    }

    @Test
    fun `a read moves on after anything, and still reports a missing agent first`() = runBlocking {
        val transport = FakeTransport { CommandOutcome(1, "", "Error: Cannot find module 'x'") }
        val (agent, _) = client(machine(), transport)
        val failure = failsWith<AgentFailure.AgentMissing> { agent.history() }
        assertEquals(FailureKind.AGENT_MISSING, failure.kind)
        // Two routes, two systems each: the read tried every one of them.
        assertEquals(4, transport.sent.size)
    }

    @Test
    fun `only the configured command is sent, never a derived launcher shape`() = runBlocking {
        val transport = FakeTransport { CommandOutcome(127, "", "not found") }
        val (agent, _) = client(machine(systems = listOf(linux)), transport)
        failsWith<AgentFailure.AgentMissing> { agent.status() }
        assertTrue(transport.sent.isNotEmpty())
        transport.sent.forEach { assertFalse(it.line, it.line.contains("bin/launcher.mjs")) }
    }

    @Test
    fun `a route-level failure skips the rest of that route's systems`() = runBlocking {
        val transport = FakeTransport { sent ->
            if (sent.endpoint == "first") {
                throw AgentFailure.Unreachable("Tower", "refused", null, DispatchStage.NOT_STARTED, FailureKind.CONNECTION_REFUSED)
            }
            accepted
        }
        val (agent, _) = client(machine(), transport)
        agent.restart("t3")
        assertEquals(listOf("first", "second"), transport.sent.map { it.endpoint })
    }

    @Test
    fun `a refused key no longer ends the walk, and is still what is reported`() = runBlocking {
        val first = endpoint("first")
        val transport = FakeTransport {
            throw AgentFailure.NotAuthorised(first, "Tower", "key refused")
        }
        val (agent, _) = client(machine(), transport)
        failsWith<AgentFailure.NotAuthorised> { agent.status() }
        // One attempt per route: the refusal is route-level, so the other system on it is skipped.
        assertEquals(listOf("first", "second"), transport.sent.map { it.endpoint })
    }

    @Test
    fun `a route whose status said restricted is written for the dispatcher from then on`() = runBlocking {
        var restricted = true
        val transport = FakeTransport { sent ->
            if (sent.line.contains("status")) {
                CommandOutcome(0, """{"ok":true,"contract":3,"system":{"id":"windows"},"agent":{"restrictedSession":$restricted}}""", "")
            } else {
                accepted
            }
        }
        val (agent, _) = client(machine(endpoints = listOf(endpoint("first")), systems = listOf(windows)), transport)
        agent.status()
        // The first status went out in the document's grammar, because nothing had said otherwise.
        assertTrue(transport.sent[0].line, transport.sent[0].line.startsWith("@\""))

        agent.restart("t3")
        assertEquals(
            "'C:\\Program Files\\nodejs\\node.exe' 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs' restart --service t3",
            transport.sent[1].line.substringBefore(" --op"),
        )

        restricted = false
        agent.status()
        agent.restart("t3")
        assertTrue(transport.sent.last().line, transport.sent.last().line.startsWith("@\""))
    }

    @Test
    fun `a status sends the budget that is left, so a working third route is not cut off`() = runBlocking {
        // C9. Two dead routes leave about 14 s of the 30, and the third attempt asks the agent for
        // 9 s rather than the 20 s that would run past this app's own deadline.
        val endpoints = listOf(endpoint("first"), endpoint("second", "192.0.2.11"), endpoint("third", "192.0.2.12"))
        val machine = machine(endpoints = endpoints, systems = listOf(linux))
        var dead = false
        val transport = FakeTransport { sent ->
            if (dead && sent.endpoint != "third") {
                now += 8_000L
                throw AgentFailure.Unreachable("Tower", "no answer", null, DispatchStage.NOT_STARTED)
            }
            v3Status
        }
        val (agent, routes) = client(machine, transport)
        agent.status()
        // Nothing is known about the agent at first, so no budget is sent to it.
        assertFalse(transport.sent.single().line.contains("--budget-ms"))

        routes.forget()
        dead = true
        transport.sent.clear()
        agent.status()
        assertEquals(listOf("first", "second", "third"), transport.sent.map { it.endpoint })
        assertEquals(
            listOf("20000", "17000", "9000"),
            transport.sent.map { it.line.substringAfter("--budget-ms ") },
        )
        // And each attempt is given only what is left of the one deadline.
        assertEquals(listOf(30_000L, 22_000L, 14_000L), transport.sent.map { it.timeoutMillis })
    }

    @Test
    fun `once an attempt has been made, none starts with less than four seconds left`() = runBlocking {
        val transport = FakeTransport {
            now += 27_000L
            throw AgentFailure.Unreachable("Tower", "no answer", null, DispatchStage.NOT_STARTED)
        }
        val (agent, _) = client(machine(), transport)
        failsWith<AgentFailure.Unreachable> { agent.status() }
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `a value that is not a token is a refused request, and nothing is sent`() = runBlocking {
        // A5. These used to be IllegalArgumentExceptions, which nothing between the client and the
        // screen caught, on a scope with no handler.
        val transport = FakeTransport { accepted }
        val (agent, _) = client(machine(), transport)
        failsWith<AgentFailure.BadRequest> { agent.run("wake;reboot") }
        failsWith<AgentFailure.BadRequest> { agent.boot("../windows") }
        failsWith<AgentFailure.BadRequest> { agent.restart("t3 --force") }
        failsWith<AgentFailure.BadRequest> { agent.pauseUpdates("forever") }
        failsWith<AgentFailure.BadRequest> { agent.operation("not an id") }
        assertTrue(transport.sent.isEmpty())

        agent.status()
        failsWith<AgentFailure.BadRequest> { agent.restart("t3", operationId = "NOT-LOWERCASE-ID") }
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `an address outside the dial grammar is never handed to the transport`() = runBlocking {
        val transport = FakeTransport { accepted }
        val endpoints = listOf(endpoint("odd", host = "victim.example,*"), endpoint("good"))
        val (agent, _) = client(machine(endpoints = endpoints), transport)
        agent.restart("t3")
        assertEquals(listOf("good"), transport.sent.map { it.endpoint })

        val only = FakeTransport { accepted }
        val (alone, _) = client(machine(endpoints = listOf(endpoint("odd", host = "-oProxyCommand=x"))), only)
        failsWith<AgentFailure.BadRequest> { alone.restart("t3") }
        assertTrue(only.sent.isEmpty())
    }

    @Test
    fun `a skipped address beside one that did not answer still reads as a machine that is off`() = runBlocking {
        // A trailing dot or a zone passes the document's guard and fails the dial grammar, so the
        // setup is valid. Reported ahead of the address that was dialled, the skipped entry made
        // every sleep a setup fault, polled at the blocked rate; and once the good address was in
        // backoff, the next poll dialled nothing at all.
        val endpoints = listOf(
            endpoint("dotted", host = "tower.example."),
            endpoint("zoned", host = "fe80::1%en0"),
            endpoint("lan"),
        )
        val transport = FakeTransport {
            throw AgentFailure.Unreachable("Tower", "no answer", null, DispatchStage.NOT_STARTED, FailureKind.HOST_UNREACHABLE)
        }
        val (agent, _) = client(machine(endpoints = endpoints, systems = listOf(linux)), transport)
        failsWith<AgentFailure.Unreachable> { agent.status() }
        failsWith<AgentFailure.Unreachable> { agent.status() }
        failsWith<AgentFailure.Unreachable> { agent.restart("t3") }
        assertEquals(listOf("lan", "lan", "lan"), transport.sent.map { it.endpoint })
    }

    @Test
    fun `a phone key that could not be read outranks an address that did not answer`() = runBlocking {
        // The first address is off and the second answered, and only this phone's key stood in the
        // way. Recorded in the same place as the first, it was reported as a machine that was asleep.
        val transport = FakeTransport { sent ->
            if (sent.endpoint == "first") {
                throw AgentFailure.Unreachable("Tower", "no answer", null, DispatchStage.NOT_STARTED, FailureKind.HOST_UNREACHABLE)
            }
            throw AgentFailure.KeyUnavailable(IllegalStateException("keystore is not ready"))
        }
        val (agent, _) = client(machine(systems = listOf(linux)), transport)
        failsWith<AgentFailure.KeyUnavailable> { agent.status() }
        assertEquals(listOf("first", "second"), transport.sent.map { it.endpoint })
    }

    @Test
    fun `a command that cannot be written is refused, and another system's line still goes`() = runBlocking {
        val transport = FakeTransport { accepted }
        val awkward = linux.copy(agent = listOf("/usr/bin/node", "/home/me/odd\\\\name/index.mjs"))
        val (agent, _) = client(machine(systems = listOf(awkward, windows)), transport)
        agent.restart("t3")
        assertEquals(1, transport.sent.size)
        assertTrue(transport.sent.single().line.startsWith("@\""))

        val nothing = FakeTransport { accepted }
        val (only, _) = client(machine(systems = listOf(awkward)), nothing)
        val failure = failsWith<AgentFailure.BadRequest> { only.restart("t3") }
        assertEquals(DispatchStage.NOT_STARTED, failure.dispatch)
        assertTrue(nothing.sent.isEmpty())
    }

    @Test
    fun `a system with a shell this app does not write for is not driven`() = runBlocking {
        val transport = FakeTransport { accepted }
        val unknown = windows.copy(grammar = null, grammarRefusal = UNKNOWN_SHELL)
        val (agent, _) = client(machine(systems = listOf(unknown, linux)), transport)
        agent.restart("t3")
        assertTrue(transport.sent.all { it.line.startsWith("/usr/bin/node") })

        val nothing = FakeTransport { accepted }
        val (alone, _) = client(machine(systems = listOf(unknown)), nothing)
        failsWith<AgentFailure.BadRequest> { alone.restart("t3") }
        assertTrue(nothing.sent.isEmpty())
    }

    @Test
    fun `a reply that cannot be read stops a mutation as acknowledged`() = runBlocking {
        val transport = FakeTransport { CommandOutcome(0, """{"ok":{"not":"a boolean"}}""", "") }
        val (agent, _) = client(machine(), transport)
        val failure = failsWith<AgentFailure.BadOutput> { agent.restart("t3") }
        assertEquals(DispatchStage.ACKNOWLEDGED, failure.dispatch)
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `a reply cut short at the output cap is not a reply`() = runBlocking {
        // T11. Whatever braces survive in a truncated stdout are not the agent's account of anything.
        val transport = FakeTransport { CommandOutcome(0, accepted.stdout, "", stdoutTruncated = true) }
        val (agent, _) = client(machine(), transport)
        val failure = failsWith<AgentFailure.BadOutput> { agent.restart("t3") }
        assertEquals(DispatchStage.AMBIGUOUS, failure.dispatch)
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `a reply wins even when it arrived as the watchdog fired`() = runBlocking {
        val transport = FakeTransport { CommandOutcome(null, accepted.stdout, "", timedOut = true) }
        val (agent, _) = client(machine(), transport)
        assertEquals(true, agent.restart("t3").value.ok)
    }

    @Test
    fun `a timed out command is ambiguous and stops a mutation`() = runBlocking {
        val transport = FakeTransport { CommandOutcome(null, "partial", "", timedOut = true) }
        val (agent, _) = client(machine(), transport)
        val failure = failsWith<AgentFailure.TimedOut> { agent.restart("t3") }
        assertEquals(DispatchStage.AMBIGUOUS, failure.dispatch)
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `cancelling a call stops the walk where it is`() = runBlocking {
        val started = CompletableDeferred<Unit>()
        val transport = FakeTransport {
            started.complete(Unit)
            awaitCancellation()
        }
        val (agent, _) = client(machine(), transport)
        val call = launch(start = CoroutineStart.UNDISPATCHED) {
            agent.status()
            fail("a cancelled call returned")
        }
        started.await()
        call.cancel()
        call.join()
        assertTrue(call.isCancelled)
        assertEquals(1, transport.sent.size)
    }

    @Test
    fun `the diagnostics page names what one address did`() = runBlocking {
        val first = endpoint("first")
        val transport = FakeTransport { throw AgentFailure.NotAuthorised(first, "Tower", "key refused") }
        val (agent, _) = client(machine(), transport)
        assertEquals(DiagnosisStatus.REFUSED_KEY, agent.diagnose(first).status)
        assertTrue(transport.sent.all { it.endpoint == "first" })

        val odd = endpoint("odd", host = "1234")
        assertEquals(DiagnosisStatus.NOT_DIALLED, agent.diagnose(odd).status)
    }
}
