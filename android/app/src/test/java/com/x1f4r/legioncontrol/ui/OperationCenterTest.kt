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
import com.x1f4r.legioncontrol.agent.OperationEvent
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.Outcome
import com.x1f4r.legioncontrol.agent.SystemInfo
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.OperationStore
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
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

    /** How many times the service that keeps a change running was asked for. */
    private var keptAlive = 0
    private val keepAlive = KeepAlive { keptAlive++ }

    /** Milliseconds on the clock the wall clock cannot move. Still, unless a test moves it. */
    private var monotonic = 0L
    private val center = OperationCenter(store, scope, SilentNotifier, keepAlive, clock = { monotonic })

    @After
    fun tearDown() {
        scope.cancel()
        directory.deleteRecursively()
    }

    /** A machine whose agent keeps records, answering `op` and `status` as the test says, counted. */
    private inner class Agent : MachineClient by unusable<MachineClient>("following a change") {
        override val machine = Machine(id = "tower", name = "Tower", endpoints = emptyList(), systems = emptyList(), wake = null)
        override var abilities = AgentAbilities(contract = 3)
        val reads = mutableListOf<String>()
        var op: suspend (String) -> Result<OpRecord> = { error("no op read was expected") }
        var reading: () -> Result<AgentStatus> = { error("no status was expected") }
        var cancelling: suspend (String) -> Result<AgentActionResult> = { error("no cancel was expected") }

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

    // MARK: - Keeping the process running while a change is in flight (A1)

    private fun ambiguous(): Result<AgentActionResult> =
        Result.failure(AgentFailure.Unreachable("Tower", "reset", stage = DispatchStage.AMBIGUOUS))

    @Test
    fun `a change is held from the press until its reply is on the record`() {
        val reply = CompletableDeferred<Result<AgentActionResult>>()
        val record = center.start(
            client = agent,
            intent = OperationIntent(kind = OpKind.RESTART, service = "demo"),
            subjectName = "Demo",
            initiator = "this phone",
        ) { reply.await() }

        assertEquals("held while the reply is on its way", 1, keepAlive.holds.value)
        assertEquals(1, keptAlive)

        reply.complete(Result.success(AgentActionResult(ok = true, contract = 3, op = finished(record.id, "restarted"))))

        assertEquals(true, store.get(record.id)!!.succeeded)
        assertEquals("settled: nothing keeps the process running", 0, keepAlive.holds.value)
    }

    @Test
    fun `a lost reply is held through the watch that settles it, with no gap between`() {
        val answer = CompletableDeferred<Result<OpRecord>>()
        agent.op = { answer.await() }
        val record = press { ambiguous() }

        assertEquals("the watch holds once the send has let go", 1, keepAlive.holds.value)
        assertEquals(listOf(record.id), agent.reads)

        answer.complete(Result.success(finished(record.id, "restarted")))

        assertEquals(TrackedState.FINISHED, store.get(record.id)!!.state)
        assertEquals(0, keepAlive.holds.value)
        // Asked for again would mean the count touched nothing between the send and the watch, and
        // the service could have stopped while the reply was still unknown.
        assertEquals("asked for once, for the send and the watch together", 1, keptAlive)
    }

    /** A restart left pending by an earlier run, asked for [age] milliseconds ago. */
    private fun leftPending(
        age: Long,
        state: TrackedState = TrackedState.OUTCOME_UNKNOWN,
        id: String = "3f6b2a9e-0c41-4d7e-b8a5-9e2c1f7d4b60",
    ): OperationRecord {
        val at = System.currentTimeMillis() - age
        return store.put(
            OperationRecord(
                id = id,
                machineId = agent.machine.id,
                machineName = agent.machine.name,
                intent = OperationIntent(kind = OpKind.RESTART, service = "demo"),
                subjectName = "Demo",
                initiator = "this phone",
                startedAt = at,
                updatedAt = at,
                state = state,
                summary = "Restart on Demo was asked for.",
                trackable = true,
            ),
        )
    }

    @Test
    fun `a change whose watch gave up is followed again without being held`() {
        // Sent hours ago to a machine that has not answered since. Every return to the app follows
        // it again; held, each would keep the phone awake for another half hour asking for nothing.
        val old = leftPending(age = 3 * 60 * 60_000L, state = TrackedState.DISPATCHED)
        // And one whose press reads as an hour from now, because the clock was set back since. It is
        // no younger for that: counted as young, it would be held again on every return until the
        // clock caught up with it.
        val ahead = leftPending(
            age = -60 * 60_000L,
            state = TrackedState.DISPATCHED,
            id = "9b1c7e40-5d2a-4f86-a3e9-0c6d8b2f1a57",
        )
        for (record in listOf(old, ahead)) {
            center.apply(record.id, OperationEvent.GaveUp("Tower never settled it.", System.currentTimeMillis()))
            assertEquals(TrackedState.OUTCOME_UNKNOWN, store.get(record.id)!!.state)
        }
        agent.op = { CompletableDeferred<Result<OpRecord>>().await() }

        center.resume(listOf(agent))

        assertEquals(
            "still followed: the machine may yet say how they ended",
            setOf(old.id, ahead.id),
            agent.reads.toSet(),
        )
        assertEquals(0, keepAlive.holds.value)
        assertEquals("the service is never asked for", 0, keptAlive)
    }

    @Test
    fun `a change sent here is held through its watch however the clock is set while it is sent`() {
        val answer = CompletableDeferred<Result<OpRecord>>()
        agent.op = { answer.await() }
        val record = press {
            // The network sets the clock back an hour while the reply is on its way, so the press
            // reads as an hour from now. The change is no less under way for that.
            store.records.value.single().let { store.put(it.copy(startedAt = it.startedAt + 60 * 60_000L)) }
            ambiguous()
        }

        assertEquals(listOf(record.id), agent.reads)
        assertEquals("timed from the press on a clock nothing sets", 1, keepAlive.holds.value)

        answer.complete(Result.success(finished(record.id, "restarted")))
        assertEquals(true, store.get(record.id)!!.succeeded)
        assertEquals(0, keepAlive.holds.value)
    }

    @Test
    fun `a change asked for here and given up on is not held again after the clock is set back`() {
        // Asked for in this process three hours ago, sent, and its watch gave up. The clock has
        // since been set back to read five minutes after the press, which would make the change
        // young again if the time on its record were all there was to go on.
        val record = center.record(agent, OperationIntent(kind = OpKind.RESTART, service = "demo"), "Demo", "this phone")
        center.apply(record.id, OperationEvent.Dispatched(trackable = true, at = record.startedAt))
        center.apply(record.id, OperationEvent.GaveUp("Tower never settled it.", record.startedAt))
        monotonic += 3 * 60 * 60_000L
        store.get(record.id)!!.let { store.put(it.copy(startedAt = System.currentTimeMillis() - 5 * 60_000L)) }
        agent.op = { CompletableDeferred<Result<OpRecord>>().await() }

        center.resume(listOf(agent))

        assertEquals("still followed", listOf(record.id), agent.reads)
        assertEquals(0, keepAlive.holds.value)
        assertEquals("the service is never asked for", 0, keptAlive)
    }

    @Test
    fun `a change a killed process left halfway is held through the watch that settles it`() {
        val answer = CompletableDeferred<Result<OpRecord>>()
        agent.op = { answer.await() }
        val record = leftPending(age = 60_000L)

        center.resume(listOf(agent))

        assertEquals(listOf(record.id), agent.reads)
        assertEquals("asked for a minute ago, so it may still be under way", 1, keepAlive.holds.value)
        assertEquals(1, keptAlive)

        answer.complete(Result.success(finished(record.id, "restarted")))
        assertEquals(true, store.get(record.id)!!.succeeded)
        assertEquals(0, keepAlive.holds.value)
    }

    @Test
    fun `a watch that can learn nothing is not held`() {
        // An agent that keeps no records and a restart, which leaves nothing on the machine to see:
        // still watched, as before, but not worth keeping the phone awake for.
        agent.abilities = AgentAbilities(contract = 2)
        val record = press { ambiguous() }

        assertTrue("still followed", store.get(record.id)!!.isPending)
        assertEquals(0, keepAlive.holds.value)
    }

    @Test
    fun `a cancel is held until its reply is on the record`() {
        val reply = CompletableDeferred<Result<AgentActionResult>>()
        agent.cancelling = { reply.await() }
        val record = queued()

        center.cancel(agent, record)
        assertEquals(1, keepAlive.holds.value)

        reply.complete(fixture("cancel.cancelled.json"))
        assertEquals(OpAction.CANCELLED, store.get(record.id)!!.opAction)
        assertEquals(0, keepAlive.holds.value)
    }

    @Test
    fun `a change sent and read by the wake walk is held while it runs`() {
        val reply = CompletableDeferred<Result<AgentActionResult>>()
        val answer = CompletableDeferred<Result<OpRecord>>()
        agent.op = { answer.await() }
        val record = center.record(agent, OperationIntent(kind = OpKind.RUN, actionId = "wake-tower"), "wake-tower", "this phone")

        scope.launch {
            center.send(agent, record.id) { reply.await() }
            center.readOnce(agent, record.id)
        }
        assertEquals("held while sending", 1, keepAlive.holds.value)
        reply.complete(ambiguous())
        assertEquals("held while reading", 1, keepAlive.holds.value)
        answer.complete(Result.success(finished(record.id, "ran")))

        assertEquals(TrackedState.FINISHED, store.get(record.id)!!.state)
        assertEquals(0, keepAlive.holds.value)
    }

    @Test
    fun `a change started on a scope that is gone gives its hold back`() {
        scope.cancel()
        press { error("never sent: the scope is gone") }
        assertEquals(0, keepAlive.holds.value)
    }
}
