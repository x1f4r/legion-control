package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.OperationStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/**
 * What "Try again" sends for a change that is over, and when it offers nothing.
 *
 * The button on screen and the press behind it both read [retryRequest], so this is the whole answer
 * to what pressing it does: the request the first press sent, rebuilt from its intent and sent under
 * a new id, or nothing at all. Nothing else is ever an acceptable answer, least of all an update in
 * place of something that was not one.
 */
class RetryRequestTest {

    private val at = 1_700_000_000_000L

    private fun asked(intent: OperationIntent) = OperationRecord(
        id = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
        machineId = "tower",
        machineName = "Tower",
        intent = intent,
        subjectName = intent.subject ?: "Tower",
        initiator = "this phone",
        startedAt = at,
        updatedAt = at,
        state = TrackedState.REQUESTED,
        summary = "asked for",
    )

    /** Sent while the machine could not be reached: nothing arrived, so asking again is safe. */
    private fun neverArrived(intent: OperationIntent) = asked(intent)
        .reduce(OperationEvent.Dispatched(trackable = true, at = at))
        .reduce(OperationEvent.TransportFailed("Tower did not answer.", null, DispatchStage.NOT_STARTED, at + 1))

    @Test
    fun `each change a button asked for is asked for again exactly as it was`() {
        val cases = listOf(
            OperationIntent(OpKind.UPDATE, service = "t3", force = true, repeatable = true) to
                RetryRequest.Update(UpdateRequest(serviceId = "t3", force = true, detach = true)),
            OperationIntent(OpKind.RESTART, service = "t3", whenIdle = true, expires = "4h", repeatable = true) to
                RetryRequest.Restart("t3", InterruptRequest(whenIdle = true, expires = "4h", detach = true)),
            // A boot and a sleep take no --detach, whatever the agent speaks.
            OperationIntent(OpKind.BOOT, target = "windows", force = true, repeatable = true) to
                RetryRequest.Boot("windows", InterruptRequest(force = true)),
            OperationIntent(OpKind.SLEEP, whenIdle = true, expires = "4h", repeatable = true) to
                RetryRequest.Sleep(InterruptRequest(whenIdle = true, expires = "4h")),
            OperationIntent(OpKind.RUN, actionId = "count", repeatable = true) to
                RetryRequest.Run("count", InterruptRequest(detach = true)),
        )

        cases.forEach { (intent, expected) ->
            val record = neverArrived(intent)
            assertTrue(intent.kind.name, record.retryable)
            assertTrue(intent.kind.name, record.canTryAgain)
            assertEquals(intent.kind.name, expected, record.retryRequest(detach = true))
        }
    }

    @Test
    fun `the service is named the way the buttons name it to that agent`() {
        val update = neverArrived(OperationIntent(OpKind.UPDATE, service = "t3", repeatable = true))
        val restart = neverArrived(OperationIntent(OpKind.RESTART, service = "t3", repeatable = true))

        assertEquals(
            RetryRequest.Update(UpdateRequest(serviceId = "t3")),
            update.retryRequest(false, ServiceNaming.BY_NAME),
        )
        assertEquals(
            RetryRequest.Restart("t3", InterruptRequest()),
            restart.retryRequest(false, ServiceNaming.BY_NAME),
        )
        // An agent that lists no services takes no --service, and the buttons send none to it.
        assertEquals(
            RetryRequest.Update(UpdateRequest(serviceId = null)),
            update.retryRequest(false, ServiceNaming.NOT_TAKEN),
        )
        assertEquals(
            RetryRequest.Restart(null, InterruptRequest()),
            restart.retryRequest(false, ServiceNaming.NOT_TAKEN),
        )
    }

    @Test
    fun `a named service is never asked for again without its name while the agent is unknown`() {
        // Pressed Update on the second service while the machine was away, so it failed before
        // anything arrived. Until the machine answers again, nobody knows whether this agent takes
        // --service, and to one that does a request without it is the first service.
        listOf(OpKind.UPDATE, OpKind.RESTART).forEach { kind ->
            val record = neverArrived(OperationIntent(kind, service = "second", repeatable = true))
            assertTrue(kind.name, record.retryable)
            assertTrue("drawn, since a reading can still say how to name it", record.canTryAgain)
            assertNull(kind.name, record.retryRequest(detach = true, services = ServiceNaming.UNKNOWN))
        }
    }

    @Test
    fun `a service id the command line cannot carry is not dropped in its place`() {
        // The buttons leave such an id out, which to an agent that lists services is its first.
        listOf(OpKind.UPDATE, OpKind.RESTART).forEach { kind ->
            val record = neverArrived(OperationIntent(kind, service = "second service", repeatable = true))
            assertFalse(kind.name, record.canTryAgain)
            assertNull(kind.name, record.retryRequest(detach = true, services = ServiceNaming.BY_NAME))
        }
    }

    @Test
    fun `what names no service is asked for again whatever is known of the services`() {
        val cases = listOf(
            OperationIntent(OpKind.BOOT, target = "windows", repeatable = true) to
                RetryRequest.Boot("windows", InterruptRequest()),
            OperationIntent(OpKind.SLEEP, repeatable = true) to RetryRequest.Sleep(InterruptRequest()),
            OperationIntent(OpKind.RUN, actionId = "count", repeatable = true) to
                RetryRequest.Run("count", InterruptRequest()),
        )
        cases.forEach { (intent, expected) ->
            ServiceNaming.entries.forEach { naming ->
                assertEquals("${intent.kind} $naming", expected, neverArrived(intent).retryRequest(false, naming))
            }
        }
    }

    @Test
    fun `how the services are named is read off the last good reading only`() {
        fun status(json: String) = AgentJson.decodeFromString(AgentStatus.serializer(), json)

        assertEquals(ServiceNaming.UNKNOWN, ServiceNaming.of(null))
        // A refusal is not an account of what the agent runs, even one that lists services.
        assertEquals(
            ServiceNaming.UNKNOWN,
            ServiceNaming.of(status("""{"ok":false,"contract":3,"error":"config-invalid","message":"broken"}""")),
        )
        assertEquals(
            ServiceNaming.UNKNOWN,
            ServiceNaming.of(status("""{"ok":false,"contract":3,"services":[{"id":"t3"}]}""")),
        )
        assertEquals(
            ServiceNaming.BY_NAME,
            ServiceNaming.of(status("""{"ok":true,"contract":3,"services":[{"id":"t3"},{"id":"second"}]}""")),
        )
        assertEquals(ServiceNaming.BY_NAME, ServiceNaming.of(status("""{"ok":true,"contract":3,"services":[]}""")))
        // The first agent, which drives the one service it reports in a t3 block.
        assertEquals(
            ServiceNaming.NOT_TAKEN,
            ServiceNaming.of(status("""{"ok":true,"agentVersion":"1.4.0","t3":{"installed":"0.0.36"}}""")),
        )
    }

    @Test
    fun `a policy write that failed offers nothing, and never an update`() {
        val machineWide = neverArrived(OperationIntent.policyWrite(null))
        val oneService = neverArrived(OperationIntent.policyWrite("t3"))
        val refused = asked(OperationIntent.policyWrite("t3"))
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(
                OperationEvent.Replied(
                    AgentActionResult(ok = false, contract = 3, reasonCode = "restricted-command", message = "refused"),
                    at + 1,
                ),
            )

        listOf(machineWide, oneService, refused).forEach { write ->
            assertEquals("recorded under the kind it governs", OpKind.UPDATE, write.kind)
            assertEquals(TrackedState.FINISHED, write.state)
            assertTrue("nothing arrived or nothing changed, so it would be safe", write.retryable)
            assertFalse("but the switch or patch it sent is not kept, so nothing is offered", write.canTryAgain)
            assertNull(write.retryRequest(detach = true))
        }
    }

    @Test
    fun `what the buttons never ask for again is not asked for again from here`() {
        val notRebuilt = listOf(
            "a cycle" to OperationIntent(OpKind.CYCLE, repeatable = true),
            "an agent install" to OperationIntent(OpKind.SELF_UPDATE, repeatable = true),
            // As the wake walk records a helper's action: a plain run would carry no wake hold.
            "a wake helper's action" to OperationIntent(OpKind.RUN, actionId = "wake-tower"),
            "an update that names no service" to OperationIntent(OpKind.UPDATE, repeatable = true),
            "a restart that names no service" to OperationIntent(OpKind.RESTART, repeatable = true),
            "a boot that names no system" to OperationIntent(OpKind.BOOT, repeatable = true),
            "an action that names no action" to OperationIntent(OpKind.RUN, repeatable = true),
        )

        notRebuilt.forEach { (what, intent) ->
            val record = neverArrived(intent)
            assertTrue(what, record.retryable)
            assertFalse(what, record.canTryAgain)
            assertNull(what, record.retryRequest(detach = true))
        }
    }

    @Test
    fun `nothing is offered for a change that may have happened, did, or is still going`() {
        val restart = OperationIntent(OpKind.RESTART, service = "t3", repeatable = true)
        val sent = asked(restart).reduce(OperationEvent.Dispatched(trackable = true, at = at))
        val unknown = sent.reduce(OperationEvent.TransportFailed("lost", null, DispatchStage.AMBIGUOUS, at + 1))
        val running = sent.reduce(
            OperationEvent.Replied(AgentActionResult(ok = true, contract = 3, op = OpRecord(id = sent.id, state = "running")), at + 1),
        )
        val done = sent.reduce(
            OperationEvent.Replied(
                AgentActionResult(
                    ok = true,
                    contract = 3,
                    op = OpRecord(id = sent.id, state = "finished", result = OpResult(ok = true, action = "restarted")),
                ),
                at + 1,
            ),
        )

        assertEquals(TrackedState.OUTCOME_UNKNOWN, unknown.state)
        assertEquals(TrackedState.RUNNING, running.state)
        assertEquals(true, done.succeeded)
        listOf(sent, unknown, running, done).forEach { record ->
            assertFalse(record.state.name, record.canTryAgain)
            assertNull(record.state.name, record.retryRequest(detach = true))
        }
    }

    @Test
    fun `a change a 1 3 build wrote down is never asked for again`() {
        // A 1.3 build wrote a failed auto-update switch exactly as it wrote a failed update, and
        // offered no Try again on anything finished. Neither of these is offered one now: one that
        // build finished, and one it left unresolved that this build settles as never arrived.
        val directory = Files.createTempDirectory("operations").toFile()
        try {
            val file = File(directory, "operations.json")
            file.writeText(
                "[" + legacy(id = "1f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0", state = "FINISHED", action = "failed", retryable = true) +
                    "," + legacy(id = "2f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0", state = "OUTCOME_UNKNOWN", action = null, retryable = true) +
                    "]",
            )
            val store = OperationStore(file)

            val finished = store.get("1f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0")!!
            assertTrue(finished.isOver)
            assertTrue(finished.retryable)
            assertFalse(finished.intent.repeatable)
            assertFalse(finished.canTryAgain)
            assertNull(finished.retryRequest(detach = true))

            val settled = store.get("2f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0")!!.reduce(OperationEvent.NotRecorded(at + 5))
            assertEquals(Outcome.FAILED, settled.outcome)
            assertTrue(settled.retryable)
            assertFalse(settled.canTryAgain)
        } finally {
            directory.deleteRecursively()
        }
    }

    /** One record as a 1.3 build wrote it, with every field that build had and none it did not. */
    private fun legacy(id: String, state: String, action: String?, retryable: Boolean): String = """
        {"id":"$id","machineId":"tower","machineName":"Tower",
         "intent":{"kind":"UPDATE","service":"t3","target":null,"actionId":null,"force":false,"whenIdle":false,"expires":null},
         "subjectName":"t3","initiator":"this phone","startedAt":$at,"updatedAt":${at + 1},"state":"$state",
         "phase":null,"progressNote":null,"progressStep":null,"progressOf":null,
         "action":${action?.let { "\"$it\"" } ?: "null"},"reasonCode":null,
         "summary":"Tower did not answer.","detail":null,"output":null,"fromVersion":null,"toVersion":null,
         "expiresAt":null,"verified":null,"retryable":$retryable,"trackable":true,"log":[]}
    """.trimIndent()
}
