package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.InterruptRequest
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OperationEvent
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.RetryRequest
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.agent.UpdateRequest
import com.x1f4r.legioncontrol.agent.reduce
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * What Try again sends from the machine page, read against the status the page has at the time.
 *
 * The page drops its status after any failed read and has none before the first, and an update of
 * the second service retried then went out with no `--service`, which the agent reads as the first
 * service: that one was stopped and reinstalled instead. The page enables the button and sends the
 * press from [tryAgainFor], so this is both what can be pressed and what pressing it sends.
 */
class TryAgainTest {

    private val at = 1_700_000_000_000L

    private fun status(json: String): AgentStatus = AgentJson.decodeFromString(AgentStatus.serializer(), json)

    private val listing = status("""{"ok":true,"contract":3,"services":[{"id":"t3"},{"id":"second"}]}""")
    private val refusal = status("""{"ok":false,"contract":3,"error":"config-invalid","message":"broken"}""")
    private val first = status("""{"ok":true,"agentVersion":"1.4.0","t3":{"installed":"0.0.36"}}""")

    /** Pressed while the machine could not be reached: it failed before anything arrived. */
    private fun neverArrived(intent: OperationIntent) = OperationRecord(
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
        .reduce(OperationEvent.Dispatched(trackable = true, at = at))
        .reduce(OperationEvent.TransportFailed("Tower did not answer.", null, DispatchStage.NOT_STARTED, at + 1))

    private val update = neverArrived(OperationIntent(OpKind.UPDATE, service = "second", repeatable = true))
    private val restart = neverArrived(OperationIntent(OpKind.RESTART, service = "second", repeatable = true))

    @Test
    fun `an update or restart of a named service waits for a reading that says how to name it`() {
        listOf(update, restart).forEach { record ->
            assertTrue("the record itself can be asked for again", record.canTryAgain)
            assertNull("no reading yet, or the last read failed", tryAgainFor(record, null, detach = true))
            assertNull("an ok:false reading is no account of the services", tryAgainFor(record, refusal, detach = true))
        }
    }

    @Test
    fun `an update or restart is never sent without its service to an agent that lists services`() {
        assertEquals(
            RetryRequest.Update(UpdateRequest(serviceId = "second", detach = true)),
            tryAgainFor(update, listing, detach = true),
        )
        assertEquals(
            RetryRequest.Restart("second", InterruptRequest(detach = true)),
            tryAgainFor(restart, listing, detach = true),
        )
    }

    @Test
    fun `the first agent, which lists no services, is sent none, as the buttons send it none`() {
        val t3 = neverArrived(OperationIntent(OpKind.UPDATE, service = "t3", repeatable = true))
        assertEquals(RetryRequest.Update(UpdateRequest(serviceId = null)), tryAgainFor(t3, first, detach = false))
    }

    @Test
    fun `a sleep and an action do not wait for the services`() {
        val sleep = neverArrived(OperationIntent(OpKind.SLEEP, whenIdle = true, expires = "4h", repeatable = true))
        val action = neverArrived(OperationIntent(OpKind.RUN, actionId = "count", repeatable = true))

        assertEquals(RetryRequest.Sleep(InterruptRequest(whenIdle = true, expires = "4h")), tryAgainFor(sleep, null, detach = true))
        assertEquals(RetryRequest.Run("count", InterruptRequest(detach = true)), tryAgainFor(action, null, detach = true))
    }
}
