package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.OpAction
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.OperationEvent
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
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

    /** The ids currently being worked on here, so nothing is dispatched or reconciled twice. */
    private val inFlight = ConcurrentHashMap.newKeySet<String>()

    fun forMachine(machineId: String): List<OperationRecord> = store.forMachine(machineId)

    fun pendingFor(machineId: String): List<OperationRecord> =
        store.forMachine(machineId).filter { it.isPending }

    fun get(id: String): OperationRecord? = store.get(id)

    /**
     * Starts a change, and returns the record as it stands the moment the press lands.
     *
     * The record exists before anything is sent, which is what makes the press itself durable: an
     * app killed between the press and the reply comes back to an operation in "outcome unknown"
     * rather than to nothing at all.
     */
    fun start(
        client: MachineClient,
        intent: OperationIntent,
        subjectName: String,
        initiator: String,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ): OperationRecord {
        val id = Contract.newOpId()
        val now = System.currentTimeMillis()
        val record = store.put(
            OperationRecord(
                id = id,
                machineId = client.machine.id,
                machineName = client.machine.name,
                intent = intent,
                subjectName = subjectName,
                initiator = initiator,
                startedAt = now,
                updatedAt = now,
                state = TrackedState.REQUESTED,
                summary = "${intent.kind.label} on $subjectName was asked for.",
            ),
        )
        run(client, record, dispatch)
        return record
    }

    /**
     * Sends a change again, reusing its id.
     *
     * Only ever offered for a record that says it is safe: either nothing reached the machine, or
     * the machine keeps operation records and will answer the same id with what already happened
     * rather than doing it twice. The intent is the record's own, unchanged, because an id binds its
     * intent and the same id with a different request is a conflict rather than a retry.
     */
    fun retry(
        client: MachineClient,
        record: OperationRecord,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ): Boolean {
        if (!record.retryable) return false
        val revived = store.put(
            record.copy(
                state = TrackedState.REQUESTED,
                updatedAt = System.currentTimeMillis(),
                retryable = false,
            ),
        )
        run(client, revived, dispatch)
        return true
    }

    private fun run(
        client: MachineClient,
        record: OperationRecord,
        dispatch: suspend (operationId: String) -> Result<AgentActionResult>,
    ) {
        if (!inFlight.add(record.id)) return
        scope.launch {
            try {
                val trackable = client.abilities.speaksV3
                apply(record.id, OperationEvent.Dispatched(trackable, System.currentTimeMillis()))

                dispatch(record.id).fold(
                    onSuccess = { result ->
                        apply(record.id, OperationEvent.Replied(result, System.currentTimeMillis()))
                    },
                    onFailure = { failure ->
                        apply(
                            record.id,
                            OperationEvent.TransportFailed(
                                summary = failure.sentence,
                                detail = failure.rawDetail,
                                stage = failure.dispatchStage,
                                at = System.currentTimeMillis(),
                            ),
                        )
                    },
                )
            } finally {
                inFlight.remove(record.id)
            }
            // Whatever it ended in, anything still open is now watched rather than assumed.
            follow(client, record.id)
        }
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
        while (scope.isActive && System.currentTimeMillis() < deadline) {
            val record = store.get(id) ?: return
            if (record.isOver) return

            // Expiry alone cannot prove that a queued change never started. Only the server's
            // operation record establishes expiration or completion after a disconnected interval.
            if (record.trackable) {
                val outcome = client.operation(id, waitSeconds = OP_WAIT_SECONDS)
                outcome.fold(
                    onSuccess = { agentRecord ->
                        apply(id, OperationEvent.Observed(agentRecord, System.currentTimeMillis()))
                    },
                    onFailure = { failure ->
                        // A read that failed says nothing about the change. It is worth trying
                        // again, and it is never a reason to conclude anything.
                        apply(
                            id,
                            OperationEvent.Note(
                                "Could not read the operation: ${failure.sentence}",
                                isError = true,
                                at = System.currentTimeMillis(),
                            ),
                        )
                        delay(RETRY_DELAY_MS)
                    },
                )
            } else {
                // No records on the far side. The only honest evidence left is the machine being in
                // the state that was asked for, and only a boot, a sleep or an install leaves any.
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

    /** Withdraws a queued change. */
    fun cancel(client: MachineClient, record: OperationRecord) {
        if (!record.isCancellable) return
        scope.launch {
            client.cancel(record.id).fold(
                onSuccess = { result ->
                    apply(record.id, OperationEvent.Replied(result, System.currentTimeMillis()))
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

    private fun apply(id: String, event: OperationEvent) {
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
