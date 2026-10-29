package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.OpAction
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.OpResult
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.Outcome
import com.x1f4r.legioncontrol.agent.SystemInfo
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.contract.ContractCorpus
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
        var cancelling: (String) -> Result<AgentActionResult> = { error("no cancel was expected") }

        override suspend fun operation(operationId: String, waitSeconds: Int?): Result<OpRecord> {
            reads += operationId
            return op(operationId)
        }

        override suspend fun status(): Result<AgentStatus> = reading()

        override suspend fun cancel(operationId: String): Result<AgentActionResult> = cancelling(operationId)
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

    /** A boot queued for the next idle moment, as the agent accepted it, waiting to be withdrawn. */
    private fun queued(): OperationRecord = store.put(
        OperationRecord(
            id = "8c3a5f60-1d29-4b74-a8e3-5f0c7b91d642",
            machineId = agent.machine.id,
            machineName = agent.machine.name,
            intent = OperationIntent(kind = OpKind.BOOT, target = "windows", whenIdle = true, expires = "4h", repeatable = true),
            subjectName = "Windows",
            initiator = "this phone",
            startedAt = 1_700_000_000_000L,
            updatedAt = 1_700_000_000_000L,
            state = TrackedState.QUEUED,
            phase = "queued",
            summary = "Boot on Windows is waiting for Tower to be idle.",
            trackable = true,
            outcome = Outcome.PENDING,
        ),
    )

    private fun fixture(name: String): Result<AgentActionResult> = Result.success(
        AgentJson.decodeFromString(AgentActionResult.serializer(), ContractCorpus.file("fixtures/$name").readText()),
    )

    @Test
    fun `a cancel the machine has no record for leaves how the change ended unknown`() {
        // The refusal describes the cancel. Its outcome, failed, is not the boot's: the machine had
        // the boot queued and has since let the record go, so whether it ran is not known.
        agent.cancelling = { fixture("cancel.not-found.json") }
        val record = queued()

        center.cancel(agent, record)

        val stored = store.get(record.id)!!
        assertEquals(TrackedState.FINISHED, stored.state)
        assertEquals(Outcome.UNKNOWN, stored.outcome)
        assertFalse("it may have run, so it is not asked for again", stored.retryable)
        assertFalse(stored.canTryAgain)
        assertNotEquals(OpAction.FAILED, stored.opAction)
    }

    @Test
    fun `a cancel refused without a record leaves the change as it was`() {
        agent.cancelling = {
            Result.success(AgentActionResult(ok = false, contract = 3, reasonCode = "restricted-command", outcome = "failed"))
        }
        val record = queued()

        center.cancel(agent, record)

        val stored = store.get(record.id)!!
        assertEquals("a refused cancel is not a failed boot", TrackedState.QUEUED, stored.state)
        assertEquals(Outcome.PENDING, stored.outcome)
        assertTrue(stored.isCancellable)
        assertTrue(stored.log.last().isError)
        assertTrue(stored.log.last().text.startsWith("Could not cancel it"))
    }

    @Test
    fun `a cancel that carries the record settles the change from the record`() {
        agent.cancelling = { fixture("cancel.cancelled.json") }
        val record = queued()

        center.cancel(agent, record)

        val stored = store.get(record.id)!!
        assertEquals(TrackedState.FINISHED, stored.state)
        assertEquals(OpAction.CANCELLED, stored.opAction)
        assertEquals("the record's outcome, not the reply's succeeded", Outcome.DEFERRED, stored.outcome)
        assertTrue("nothing ran, so it can be asked for again", stored.canTryAgain)
    }
}
