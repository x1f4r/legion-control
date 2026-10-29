package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.OpAction
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.OperationEvent
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.ReasonCode
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.agent.parseInstantMillis
import com.x1f4r.legioncontrol.agent.reduce
import com.x1f4r.legioncontrol.data.OperationStore
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.concurrent.ConcurrentHashMap

/**
 * Every change this app asks a machine to make, from the press to the outcome being known.
 *
 * It exists because an ssh round trip is not an operation. A restart that loses its connection
 * halfway looks exactly like a restart that was never sent, and the old code could only guess which;
 * an update that takes nine minutes outlives the screen it was started from; and a phone that gets
 * put in a pocket takes its whole process with it.
 *
 * Three things fix that, and this file is where they meet. Every change gets a `--op` id before
 * anything is sent, so it can be asked about instead of repeated. The record is written to disk at
 * every transition, so a phone that comes back finds out what happened. And the work runs on a scope
 * that outlives the screen, because a user who presses Update and locks the phone has not changed
 * their mind about updating.
 *
 * The one rule that everything else serves: a change whose outcome is unknown is never sent again.
 */
class OperationCenter(
    private val store: OperationStore,
    /**
     * The scope the work runs on.
     *
     * Application scoped, not screen scoped, and that is the point: polling stops when the screen
     * goes away and a change in flight does not. Android can still kill the process, which is why
     * the record is durable and why coming back reconciles rather than assumes.
     */
    private val scope: CoroutineScope,
    private val notifier: OperationNotifier,
) {
    val records: StateFlow<List<OperationRecord>> get() = store.records

    /**
     * Every record, as state the screen reads, so whatever draws them is drawn again when they
     * change. A value read straight off [records] is a value Compose never hears about: Changes and
     * Recent changes stayed as they were until something else happened to redraw the page.
     *
     * Collected here, once for the process, rather than once per machine model. The models are
     * rebuilt with every change to the setup, and a collector each was a collector nobody stopped.
     */
    var observed: List<OperationRecord> by mutableStateOf(store.records.value)
        private set

    /** The ids currently being worked on here, so nothing is dispatched or reconciled twice. */
    private val inFlight = ConcurrentHashMap.newKeySet<String>()

    init {
        scope.launch { store.records.collect { observed = it } }
    }

    fun pendingFor(machineId: String): List<OperationRecord> =
        store.forMachine(machineId).filter { it.isPending }

    fun get(id: String): OperationRecord? = store.get(id)

    /**
     * Starts a change, and returns the record as it stands the moment the press lands.
     *
     * The record exists before anything is sent, which is what makes the press itself durable: an
     * app killed between the press and the reply comes back to an operation in "outcome unknown"
     * rather than to nothing at all. Every start mints a new id, a second attempt at something that
     * already ended included: re-sending an id only ever replays what the agent stored for it, which
     * is how "Try again" on a refused change used to be refused forever.
     */
    fun start(
        client: MachineClient,
        intent: OperationIntent,
        subjectName: String,
        initiator: String,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ): OperationRecord {
        val record = record(client, intent, subjectName, initiator)
        run(client, record, dispatch)
        return record
    }

    /**
     * Writes down a change about to be sent, under a new id, without sending it.
     *
     * For the one caller that has to wait for its own reply before deciding what comes next: the
     * wake walk, which sends with [send] and reads the outcome itself. [wakeTarget] marks a helper
     * action that is not a declared `wol`, which is what makes an ambiguous result a wake hold.
     */
    fun record(
        client: MachineClient,
        intent: OperationIntent,
        subjectName: String,
        initiator: String,
        wakeTarget: String? = null,
    ): OperationRecord {
        val now = System.currentTimeMillis()
        return store.put(
            OperationRecord(
                id = Contract.newOpId(),
                machineId = client.machine.id,
                machineName = client.machine.name,
                intent = intent,
                subjectName = subjectName,
                initiator = initiator,
                startedAt = now,
                updatedAt = now,
                state = TrackedState.REQUESTED,
                summary = "${intent.kind.label} on $subjectName was asked for.",
                wakeTarget = wakeTarget,
            ),
        )
    }

    private fun run(
        client: MachineClient,
        record: OperationRecord,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ) {
        // Claimed before the launch, so a poll that lands in between cannot start following a change
        // that has not been sent yet and leave it never sent at all.
        if (!inFlight.add(record.id)) return
        scope.launch {
            try {
                dispatchClaimed(client, record.id, dispatch)
            } finally {
                inFlight.remove(record.id)
            }
            // Whatever it ended in, anything still open is now watched rather than assumed.
            follow(client, record.id)
        }
    }

    /**
     * Sends a recorded change once, here and now, and returns once the reply or the failure is on
     * the record. False when it was already being sent or followed, and nothing was sent.
     */
    internal suspend fun send(
        client: MachineClient,
        id: String,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ): Boolean {
        if (!inFlight.add(id)) return false
        try {
            dispatchClaimed(client, id, dispatch)
        } finally {
            inFlight.remove(id)
        }
        return true
    }

    private suspend fun dispatchClaimed(
        client: MachineClient,
        id: String,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ) {
        val trackable = client.abilities.speaksV3
        apply(id, OperationEvent.Dispatched(trackable, System.currentTimeMillis()))

        dispatch(id).fold(
            onSuccess = { result ->
                apply(id, OperationEvent.Replied(result, System.currentTimeMillis()))
            },
            onFailure = { failure ->
                apply(
                    id,
                    OperationEvent.TransportFailed(
                        summary = failure.sentence,
                        detail = failure.rawDetail,
                        stage = failure.dispatchStage,
                        at = System.currentTimeMillis(),
                    ),
                )
            },
        )
    }

    /**
     * Reads a recorded change once, for a caller that cannot wait for [follow]: the wake walk, which
     * reads a helper's ambiguous action once with `--wait` before moving on, and reads each held
     * helper's once before planning (contract section 10.2).
     */
    internal suspend fun readOnce(client: MachineClient, id: String, waitSeconds: Int? = OP_WAIT_SECONDS) {
        val record = store.get(id) ?: return
        if (record.isOver || !record.trackable) return
        client.operation(id, waitSeconds = waitSeconds).fold(
            onSuccess = { apply(id, OperationEvent.Observed(it, System.currentTimeMillis())) },
            onFailure = { failure -> noteReadFailure(id, failure) },
        )
    }

    /**
     * Watches an operation until it settles, or until watching stops being worth it.
     *
     * Two ways to settle it. The agent's own record is the good one: `op ID --wait` long-polls, so a
     * running update costs one connection every twenty seconds rather than one every second, and the
     * answer is authoritative. Where the agent keeps no records, the machine itself is watched for
     * the one kind of evidence that means anything, which is being observably in the state that was
     * asked for.
     */
    fun follow(client: MachineClient, id: String) {
        val record = store.get(id) ?: return
        if (record.isOver) {
            if (record.isNoteworthyEnding) notifier.finished(record)
            return
        }
        if (!inFlight.add(id)) return
        scope.launch {
            try {
                watch(client, id)
            } finally {
                inFlight.remove(id)
            }
            store.get(id)?.takeIf { it.isOver && it.isNoteworthyEnding }?.let(notifier::finished)
        }
    }

    private suspend fun watch(client: MachineClient, id: String) {
        val deadline = System.currentTimeMillis() + WATCH_BUDGET_MS
        // Set once the agent's own record is finished. Reading it again only reads it again, and
        // `op --wait` answers a finished record at once, so asking in a loop would be a tight one.
        var agentDone = false
        while (scope.isActive && System.currentTimeMillis() < deadline) {
            val record = store.get(id) ?: return
            if (record.isOver) return

            // Expiry alone cannot prove that a queued change never started. Only the server's
            // operation record establishes expiration or completion after a disconnected interval.
            if (record.trackable && !agentDone) {
                val outcome = client.operation(id, waitSeconds = OP_WAIT_SECONDS)
                outcome.fold(
                    onSuccess = { agentRecord ->
                        apply(id, OperationEvent.Observed(agentRecord, System.currentTimeMillis()))
                        if (agentRecord.isFinished) agentDone = true
                    },
                    onFailure = { failure ->
                        // A read that failed says nothing about the change. It is worth trying
                        // again, and it is never a reason to conclude anything.
                        if (!noteReadFailure(id, failure)) delay(RETRY_DELAY_MS)
                    },
                )
            } else {
                // No records on the far side, or nothing more in the one it has. The only honest
                // evidence left is the machine being in the state that was asked for, and only a
                // boot, a sleep or an install leaves any.
                if (!observe(client, id)) delay(OBSERVE_DELAY_MS)
            }
        }

        val record = store.get(id) ?: return
        if (record.isPending) {
            apply(
                id,
                OperationEvent.GaveUp(
                    "${record.kind.label} on ${record.subjectName} was sent and " +
                        "${record.machineName} never settled what became of it.",
                    System.currentTimeMillis(),
                ),
            )
        }
    }

    /**
     * Puts a failed `op ID` read on the record. True when the failure was itself the answer: the
     * machine has no such operation, which settles a lost reply as a change that never arrived.
     */
    private fun noteReadFailure(id: String, failure: Throwable): Boolean {
        if (failure is AgentFailure.NoSuchOperation) {
            apply(id, OperationEvent.NotRecorded(System.currentTimeMillis()))
            return true
        }
        apply(
            id,
            OperationEvent.Note(
                "Could not read the operation: ${failure.sentence}",
                isError = true,
                at = System.currentTimeMillis(),
            ),
        )
        return false
    }

    /**
     * Looks at the machine for evidence that a change happened.
     *
     * Deliberately narrow. A boot is settled by the machine answering as the system that was asked
     * for; a sleep by it going quiet and coming back; an update by the installed version being the
     * one that was aimed at. A restart leaves no trace that can be told apart from a service that
     * was already running, so it is never inferred. Nothing here uses uptime either: a machine that
     * rebooted for its own reasons has a short uptime too.
     */
    private suspend fun observe(client: MachineClient, id: String): Boolean {
        val record = store.get(id) ?: return true
        if (!record.kind.isObservable) return false

        val status = client.status().getOrNull() ?: return false
        val now = System.currentTimeMillis()

        return when (record.kind) {
            OpKind.BOOT -> {
                val target = record.intent.target ?: return false
                if (status.systemId == target) {
                    apply(
                        id,
                        OperationEvent.Resolved(
                            OpAction.REBOOTED,
                            "${record.machineName} came back as ${record.subjectName}.",
                            at = now,
                        ),
                    )
                    true
                } else {
                    false
                }
            }

            OpKind.UPDATE -> {
                val service = record.intent.service ?: return false
                val installed = status.effectiveServices
                    .firstOrNull { it.id == service }
                    ?.installed
                val target = record.toVersion
                if (target != null && installed == target) {
                    apply(
                        id,
                        OperationEvent.Resolved(
                            OpAction.UPDATED,
                            "${record.subjectName} is now $installed.",
                            at = now,
                        ),
                    )
                    true
                } else {
                    false
                }
            }

            // Neither an answer nor silence proves that a requested sleep occurred.
            OpKind.SLEEP -> false

            else -> false
        }
    }

    /**
     * Picks up every change that was in flight when the app was last closed.
     *
     * The record store already moved them to "outcome unknown" on the way in, so this does not have
     * to decide anything: it only has to start watching again.
     */
    fun resume(clients: List<MachineClient>) {
        val byId = clients.associateBy { it.machine.id }
        store.pending.forEach { record ->
            byId[record.machineId]?.let { follow(it, record.id) }
        }
    }

    /**
     * Withdraws a queued change.
     *
     * The reply to `cancel` says whether cancelling worked, and only the record it carries says what
     * became of the change. A reply with no record is never applied to the change as if it were one:
     * its outcome is the cancel's, and a refused cancel is not a failed boot. `not-configured` means
     * the machine no longer has the record, which is what an `op` read answering `op: null` means
     * too; any other refusal leaves the change as it was, with the reason on its log.
     */
    fun cancel(client: MachineClient, record: OperationRecord) {
        if (!record.isCancellable) return
        scope.launch {
            client.cancel(record.id).fold(
                onSuccess = { result ->
                    val now = System.currentTimeMillis()
                    val why = result.effectiveMessage ?: ReasonCode.describe(result.reasonCode)
                    val event = when {
                        result.op != null -> OperationEvent.Replied(result, now)
                        result.reasonCode == ReasonCode.NOT_CONFIGURED.wire -> OperationEvent.NotRecorded(now)
                        result.ok == false -> OperationEvent.Note(
                            "Could not cancel it: ${why ?: "the agent gave no reason"}",
                            isError = true,
                            at = now,
                        )

                        // Answered without the record, which only reading it by its id can settle.
                        else -> OperationEvent.Note(
                            "${record.machineName} answered the cancel without a record of it" +
                                why?.let { ": $it" }.orEmpty(),
                            at = now,
                        )
                    }
                    apply(record.id, event)
                },
                onFailure = { failure ->
                    apply(
                        record.id,
                        OperationEvent.Note(
                            "Could not cancel it: ${failure.sentence}",
                            isError = true,
                            at = System.currentTimeMillis(),
                        ),
                    )
                },
            )
        }
    }

    fun clearHistory() = store.clearHistory()

    fun exportText(): String = store.exportText()

    /** Every unresolved wake hold for [target], oldest first (contract section 10.2). */
    fun wakeHolds(target: String): List<OperationRecord> = store.wakeHolds(target)

    /**
     * Releases every hold for [target] recorded before [before]: the target answered an
     * authenticated status sent after them, or a person chose to try the other helpers. The records
     * are still followed; they just hold nothing back any more.
     */
    fun releaseWakeHolds(target: String, before: Long, reason: String) =
        store.releaseWakeHolds(target, before, reason)

    internal fun apply(id: String, event: OperationEvent) {
        store.update(id) { it.reduce(event) }
    }

    private companion object {
        /** How long `op ID --wait` holds the connection. Long enough to be cheap, short enough to cancel. */
        const val OP_WAIT_SECONDS = 20

        /** How long a change is watched before the app admits it does not know. */
        const val WATCH_BUDGET_MS = 30 * 60_000L

        const val RETRY_DELAY_MS = 15_000L

        /** The gap between looks at a machine when the agent keeps no records of its own. */
        const val OBSERVE_DELAY_MS = 10_000L
    }
}

/** Told when a change the user asked for is over. */
interface OperationNotifier {
    fun finished(record: OperationRecord)
}

/** Used when notifications are off or not permitted. Doing nothing quietly is the whole job. */
object SilentNotifier : OperationNotifier {
    override fun finished(record: OperationRecord) = Unit
}

/** The freshest agent record for a machine, for the operations section. */
fun List<OpRecord>.newestFirst(): List<OpRecord> =
    sortedByDescending { parseInstantMillis(it.updatedAt) ?: 0L }
