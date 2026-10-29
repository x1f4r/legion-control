package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.OpResult
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.Outcome
import com.x1f4r.legioncontrol.agent.SystemInfo
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.data.OperationStore
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/**
 * What [OperationCenter] does after a change has been sent: what it reads, and when it stops.
 *
 * The reducer decides what a reply means. These are the questions around it: whether the machine is
 * asked again, how often, and under which id. Everything runs unconfined, so a test returns only
 * once the center has done everything it was going to do without waiting for a clock.
 */
class OperationCenterTest {

    private val directory: File = Files.createTempDirectory("operations").toFile()
    private val store = OperationStore(File(directory, "operations.json"))
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
    private val center = OperationCenter(store, scope, SilentNotifier)

    @After
    fun tearDown() {
        scope.cancel()
        directory.deleteRecursively()
    }

    /** A machine whose agent keeps records, answering `op` and `status` as the test says, counted. */
    private inner class Agent : MachineClient by unusable<MachineClient>("following a change") {
        override val machine = Machine(id = "tower", name = "Tower", endpoints = emptyList(), systems = emptyList(), wake = null)
        override val abilities = AgentAbilities(contract = 3)
        val reads = mutableListOf<String>()
        var op: (String) -> Result<OpRecord> = { error("no op read was expected") }
        var reading: () -> Result<AgentStatus> = { error("no status was expected") }

        override suspend fun operation(operationId: String, waitSeconds: Int?): Result<OpRecord> {
            reads += operationId
            return op(operationId)
        }

        override suspend fun status(): Result<AgentStatus> = reading()
    }

    private val agent = Agent()

    private fun press(kind: OpKind = OpKind.RESTART, reply: () -> Result<AgentActionResult>) = center.start(
        client = agent,
        intent = OperationIntent(kind = kind, service = "demo".takeIf { kind != OpKind.BOOT }, target = "windows".takeIf { kind == OpKind.BOOT }),
        subjectName = "Demo",
        initiator = "this phone",
    ) { reply() }

    private fun finished(id: String, action: String, reasonCode: String? = null) = OpRecord(
        id = id,
        state = "finished",
        result = OpResult(ok = reasonCode == null, action = action, reasonCode = reasonCode),
    )

    @Test
    fun `a change held back is not read again`() {
        val record = press {
            Result.success(AgentActionResult(ok = true, contract = 3, op = finished("x", "deferred", "busy")))
        }

        val stored = store.get(record.id)!!
        assertEquals(TrackedState.AWAITING_DECISION, stored.state)
        assertEquals("the refusal is final: reading it again only reads it again", emptyList<String>(), agent.reads)
    }

    @Test
    fun `a lost reply the machine has no record of is settled by one read`() {
        agent.op = { id -> Result.failure(AgentFailure.NoSuchOperation(id, "no operation $id on this machine")) }
        val record = press { Result.failure(AgentFailure.Unreachable("Tower", "reset", stage = DispatchStage.AMBIGUOUS)) }

        val stored = store.get(record.id)!!
        assertEquals(listOf(record.id), agent.reads)
        assertEquals(TrackedState.FINISHED, stored.state)
        assertEquals(Outcome.FAILED, stored.outcome)
        assertTrue(stored.retryable)
    }

    @Test
    fun `a finished record that is still on its way is not read in a loop`() {
        // An older agent that ended a boot on "rebooting" has nothing more to say about it. Reading
        // the same finished record again answers at once, so the machine itself is watched instead.
        agent.op = { id -> Result.success(finished(id, "rebooting")) }
        agent.reading = { Result.success(AgentStatus(ok = true, contract = 3, system = SystemInfo(id = "windows"))) }
        val record = press(kind = OpKind.BOOT) { Result.failure(AgentFailure.Unreachable("Tower", "reset", stage = DispatchStage.AMBIGUOUS)) }

        val stored = store.get(record.id)!!
        assertEquals(listOf(record.id), agent.reads)
        assertEquals(TrackedState.FINISHED, stored.state)
        assertEquals(true, stored.succeeded)
    }

    @Test
    fun `every press is a new id, a second press at something that ended included`() {
        val first = press { Result.success(AgentActionResult(ok = true, contract = 3, op = finished("a", "conflict", "operation-in-progress"))) }
        val second = press { Result.success(AgentActionResult(ok = true, contract = 3, op = finished("b", "restarted"))) }

        assertNotEquals(first.id, second.id)
        assertEquals(Outcome.DEFERRED, store.get(first.id)!!.outcome)
        assertEquals("the refused one stays as it was", TrackedState.FINISHED, store.get(first.id)!!.state)
        assertEquals(true, store.get(second.id)!!.succeeded)
        assertFalse(store.get(second.id)!!.retryable)
    }
}
