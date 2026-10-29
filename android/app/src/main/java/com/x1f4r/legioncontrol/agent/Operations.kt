package com.x1f4r.legioncontrol.agent

import kotlinx.serialization.Serializable

/**
 * A change this app asked a machine to make, from the moment it was asked for to the moment it is
 * known to be over.
 *
 * An action used to be one ssh round trip. It returned, or it did not, and if it did not there was
 * nothing left: no id to ask about, no record of how far it got, and a screen that had to choose
 * between claiming a reboot that may never have happened and claiming a failure that may have
 * succeeded. Worse, a link that dropped mid-command looked exactly like a link that never opened,
 * so the app would happily send the same restart down a second route.
 *
 * The contract's half of the fix is `--op ID`: the agent keeps the record, and a retry with the same
 * id returns what happened rather than doing it again. This file is the phone's half. It holds the
 * intent that id binds, survives the app being closed, and never lets an unresolved outcome be drawn
 * as either success or failure.
 */

/**
 * What an operation id means, fixed at the moment the id is minted.
 *
 * The same id with a different kind, service, target, action, force or queue intent is a conflict and
 * not a replay. Writing the intent down beside the id is what lets this app detect
 * that locally instead of finding out from the far side, and it is what makes a retry safe: the
 * retry is provably the same request.
 */
@Serializable
data class OperationIntent(
    val kind: OpKind,
    val service: String? = null,
    val target: String? = null,
    val actionId: String? = null,
    /** Skips the busy gate and nothing else. Never the lock, never a missing postcondition. */
    val force: Boolean = false,
    val whenIdle: Boolean = false,
    /** The `DUR` form, e.g. `4h`. Only meaningful with [whenIdle]. */
    val expires: String? = null,
    /**
     * Whether these fields are the whole request, so that "Try again" can send it once more, exactly,
     * under a new id. Set by the update, restart, boot, sleep and action buttons and by nothing else.
     *
     * False by default, because a wrong true sends a change nobody asked for and a wrong false only
     * costs a button. A write of the update policy is recorded as an update ([policyWrite]) and keeps
     * no patch, so rebuilding one would stop and reinstall a service instead; a wake helper's action
     * would run without the hold the wake walk keeps; and an intent written before this field was,
     * by a build that recorded policy writes the same way, cannot be told apart from an update.
     */
    val repeatable: Boolean = false,
) {
    /** What this operation is about, for a heading. */
    val subject: String?
        get() = service?.takeIf { it.isNotBlank() }
            ?: target?.takeIf { it.isNotBlank() }
            ?: actionId?.takeIf { it.isNotBlank() }

    /** `manual`, `force` or `queued`, which is the mode the agent will record. */
    val mode: OpMode
        get() = when {
            whenIdle -> OpMode.QUEUED
            force -> OpMode.FORCE
            else -> OpMode.MANUAL
        }

    companion object {
        /**
         * A write of the update schedule, pause or policy for [service], or for the machine when null.
         *
         * Recorded under [OpKind.UPDATE], which is what it governs, and never [repeatable]: the switch,
         * the pause or the patch it sent is not kept here, and the fields that are describe an update.
         */
        fun policyWrite(service: String?) = OperationIntent(kind = OpKind.UPDATE, service = service)
    }
}

/**
 * Where the phone thinks an operation has got to.
 *
 * Deliberately not the same enum as the agent's `state`. The agent has three values and none of them
 * can express "I sent this and I do not know whether it arrived", which is the state that matters
 * most here and the one the old code did not have.
 */
enum class TrackedState {
    /** The id exists and nothing has been sent. */
    REQUESTED,

    /** Handed to the transport. From here on a failure may be ambiguous. */
    DISPATCHED,

    /** The agent has it and is working on it. */
    RUNNING,

    /** The agent accepted it to run at the next idle moment. */
    QUEUED,

    /**
     * The agent held it back, by design, and the user is being asked whether to insist.
     *
     * Over, as far as following it goes: a deferred change is finished on the agent and reading it
     * again only reads the same refusal, which is how this app used to ask a machine the same
     * question in a tight loop for half an hour. Insisting, or asking again later, is a new request
     * with a new id.
     */
    AWAITING_DECISION,

    /**
     * The reply was lost, or the command overran, and what happened is not known.
     *
     * Nothing is sent again from here except a read of the same id. This is the state that exists so
     * that nothing is. Disconnection does not prove a transition, even when the last reading was
     * idle and even when the request was forced.
     */
    OUTCOME_UNKNOWN,

    /** Over, with an [OpAction] and possibly a [ReasonCode]. */
    FINISHED,
    ;

    /** Whether there is nothing left to follow: the agent is done with it, one way or another. */
    val isOver: Boolean get() = this == FINISHED || this == AWAITING_DECISION

    val isPending: Boolean get() = !isOver
}

@Serializable
data class OperationLogLine(
    val at: Long,
    val text: String,
    val isError: Boolean = false,
)

/**
 * One change, durably.
 *
 * [retryable] is the field that keeps a press from becoming two changes. It is only ever true when
 * something is known not to have started, or when the same id can safely be sent again because the
 * agent keeps the record and would replay it.
 */
@Serializable
data class OperationRecord(
    /** The `--op` id. Lowercase, matches the contract's opId grammar. */
    val id: String,
    val machineId: String,
    val machineName: String,
    val intent: OperationIntent,
    /** What to call the subject on screen, which the id is not. */
    val subjectName: String,
    /** Who asked, in this app's own words. */
    val initiator: String,
    val startedAt: Long,
    val updatedAt: Long,
    val state: TrackedState,

    /** The agent's own phase, when it has told us one. */
    val phase: String? = null,
    val progressNote: String? = null,
    val progressStep: Int? = null,
    val progressOf: Int? = null,

    /** The agent's final action, once it has one. */
    val action: String? = null,
    val reasonCode: String? = null,

    /** One sentence, for the screen. */
    val summary: String,
    /** Raw text underneath: the agent's message, its error, ssh output. */
    val detail: String? = null,
    /** What a configured action printed. */
    val output: String? = null,
    val fromVersion: String? = null,
    val toVersion: String? = null,
    val expiresAt: Long? = null,
    /** Whether the agent said it checked that the thing it set out to do actually happened. */
    val verified: Boolean? = null,
    /**
     * True when asking for it again is safe: nothing reached the machine, or the agent finished it in
     * a way that leaves nothing done twice by a second request. A second request always has an id of
     * its own (contract section 5.6), so a stored refusal is never replayed back at it. Whether it is
     * offered also depends on the intent being the whole request; see [canTryAgain].
     */
    val retryable: Boolean = false,
    /** True when the agent accepted `--op` and can therefore be asked what happened. */
    val trackable: Boolean = false,
    val log: List<OperationLogLine> = emptyList(),
    /**
     * How it stands by contract section 5.6, from the last reply or record the agent gave, or from
     * what was seen here. Null until there is anything to go on, and on a record written by a build
     * that predates outcomes.
     */
    val outcome: Outcome? = null,
    /**
     * The machine this helper action was run to wake, for an action the helper does not declare
     * `wol` (contract section 10.2). While such a record is unresolved it is a wake hold: no other
     * command helper is run for that machine, so an ambiguous script never runs on two helpers.
     */
    val wakeTarget: String? = null,
    /**
     * Set when the hold was released without the record being resolved: the target answered, or a
     * person chose to try the other helpers. The record is still followed; it just holds nothing.
     */
    val wakeReleased: Boolean = false,
) {
    val isOver: Boolean get() = state.isOver
    val isPending: Boolean get() = state.isPending

    val kind: OpKind get() = intent.kind

    val opAction: OpAction? get() = OpAction.fromWire(action)
    val reason: ReasonCode? get() = ReasonCode.fromWire(reasonCode)

    /**
     * Whether this is a wake hold on [wakeTarget]: a non-`wol` helper action whose record has not
     * been read finished, and that nobody has released. It is never released by time.
     */
    val holdsWake: Boolean get() = wakeTarget != null && !isOver && !wakeReleased

    /** The reason in words: the known sentence, or the agent's own code opened out. */
    val reasonSentence: String? get() = ReasonCode.describe(reasonCode)

    /** The agent's phase in words. */
    val phaseSentence: String? get() = OpPhase.describe(phase)

    /** Whether the user should be offered the forcing version of this. */
    val invitesForce: Boolean
        get() = state == TrackedState.AWAITING_DECISION &&
            !intent.force &&
            (reason?.isForceable ?: true)

    /** Whether it can still be withdrawn. */
    val isCancellable: Boolean get() = state == TrackedState.QUEUED

    /**
     * Whether "Try again" is offered: exactly when [retryRequest] has a request to send, so the button
     * on screen and the press behind it can never disagree.
     */
    val canTryAgain: Boolean get() = retryRequest(detach = false) != null

    /**
     * Whether it ended well. Null while it is not over, while the outcome is not known, and for a
     * change the agent held back, which neither worked nor failed.
     */
    val succeeded: Boolean?
        get() {
            if (state != TrackedState.FINISHED) return null
            return when (outcome) {
                Outcome.SUCCEEDED -> true
                Outcome.FAILED -> false
                // Written by a build that predates outcomes, which read it off the action.
                null -> opAction?.isSuccess ?: (reasonCode == null)
                else -> null
            }
        }

    /**
     * Whether it ended in a way worth telling the user about when they were not looking. A change
     * held back for a decision is not: the question is on screen, and it was never one before.
     */
    val isNoteworthyEnding: Boolean
        get() = state == TrackedState.FINISHED && opAction != OpAction.NOOP

    /** Progress as a fraction, when the agent counted steps. */
    val progressFraction: Float?
        get() {
            val step = progressStep ?: return null
            val total = progressOf?.takeIf { it > 0 } ?: return null
            return (step.toFloat() / total.toFloat()).coerceIn(0f, 1f)
        }
}

/** A second request for a change that is over, as "Try again" sends it under a new id. */
sealed interface RetryRequest {
    data class Update(val request: UpdateRequest) : RetryRequest
    data class Restart(val service: String?, val request: InterruptRequest) : RetryRequest
    data class Boot(val target: String, val request: InterruptRequest) : RetryRequest
    data class Sleep(val request: InterruptRequest) : RetryRequest
    data class Run(val actionId: String, val request: InterruptRequest) : RetryRequest
}

/**
 * What "Try again" sends for this change, or null when it offers nothing (contract section 5.6).
 *
 * Only for a change that is over, ended without anything a second request could double, and whose
 * intent is the whole request: an update, a restart, a boot, a sleep or an action, rebuilt with the
 * same force and queueing it was first asked for with, and naming the subject it named. A cycle, an
 * agent install, a policy write and anything written down without [OperationIntent.repeatable] are
 * never rebuilt here, because the fields kept for them would send something else. [detach] and
 * [serviceArgument] are what the machine's agent takes now, as the buttons decide them.
 */
fun OperationRecord.retryRequest(
    detach: Boolean,
    serviceArgument: (String) -> String? = { it },
): RetryRequest? {
    if (!isOver || !retryable || !intent.repeatable) return null
    val force = intent.force
    val whenIdle = intent.whenIdle
    val expires = intent.expires
    return when (intent.kind) {
        OpKind.UPDATE -> intent.service?.takeIf { it.isNotBlank() }?.let { service ->
            RetryRequest.Update(UpdateRequest(serviceArgument(service), force, whenIdle, expires, detach))
        }

        OpKind.RESTART -> intent.service?.takeIf { it.isNotBlank() }?.let { service ->
            RetryRequest.Restart(serviceArgument(service), InterruptRequest(force, whenIdle, expires, detach))
        }

        // The contract gives a boot and a sleep no `--detach`, so neither is ever sent with one.
        OpKind.BOOT -> intent.target?.takeIf { it.isNotBlank() }?.let { target ->
            RetryRequest.Boot(target, InterruptRequest(force, whenIdle, expires))
        }

        OpKind.SLEEP -> RetryRequest.Sleep(InterruptRequest(force, whenIdle, expires))

        OpKind.RUN -> intent.actionId?.takeIf { it.isNotBlank() }?.let { action ->
            RetryRequest.Run(action, InterruptRequest(force, whenIdle, expires, detach))
        }

        OpKind.CYCLE, OpKind.SELF_UPDATE -> null
    }
}

/** Everything that can move an operation along. */
sealed interface OperationEvent {
    val at: Long

    /** The command has been handed to the transport. */
    data class Dispatched(val trackable: Boolean, override val at: Long) : OperationEvent

    /** The agent answered a mutating command. */
    data class Replied(val result: AgentActionResult, override val at: Long) : OperationEvent

    /** A record read back with `op ID`, whether or not this phone was listening when it ran. */
    data class Observed(val record: OpRecord, override val at: Long) : OperationEvent

    /**
     * `op ID` answered that the machine has no such operation: `op: null`, under `not-configured`
     * or, from a 3.0.x agent, `bad-argument`. `cancel ID` says the same with `not-configured`.
     *
     * For a change the agent was never heard from about, that settles it: the agent writes the record
     * before it acts, so the request never arrived. For one the agent had already answered about, the
     * record has been pruned since, and what became of it is not known.
     */
    data class NotRecorded(override val at: Long) : OperationEvent

    /**
     * The transport gave up. [stage] decides whether this can be sent again.
     *
     * [DispatchStage.NOT_STARTED] finishes it as failed and retryable; [DispatchStage.AMBIGUOUS] and
     * [DispatchStage.ACKNOWLEDGED] move it to [TrackedState.OUTCOME_UNKNOWN] and never to a verdict.
     */
    data class TransportFailed(
        val summary: String,
        val detail: String?,
        val stage: DispatchStage,
        override val at: Long,
    ) : OperationEvent

    /**
     * The machine was watched afterwards and settled the question by being in the state that was
     * asked for.
     *
     * Only used where the contract says an outcome is observable: a boot target that is now the
     * running system, a version that is now installed. Never for a restart, and never from uptime
     * alone.
     */
    data class Resolved(
        val action: OpAction,
        val summary: String,
        val detail: String? = null,
        override val at: Long,
    ) : OperationEvent

    /** Reconciliation ran out of patience. Stays unknown, which is what it is. */
    data class GaveUp(val summary: String, override val at: Long) : OperationEvent

    /** A queued operation's expiry passed with nothing heard. */
    data class Expired(override val at: Long) : OperationEvent

    /** A line for the log, changing nothing else. */
    data class Note(val text: String, val isError: Boolean = false, override val at: Long) :
        OperationEvent
}

/** How many lines of an operation's log are kept. The contract caps the agent's at 200; match it. */
private const val MAX_LOG_LINES = 200

/**
 * The whole of the state machine, as one pure function.
 *
 * Pure on purpose. This is the part that decides whether a restart may be sent again, and that is
 * not a decision to leave scattered across a view model where it can only be checked by running the
 * app against a real machine.
 */
fun OperationRecord.reduce(event: OperationEvent): OperationRecord {
    // A finished operation is finished. A late reply about it can add a log line and nothing else,
    // so a reconciliation that arrives after the user has already been told cannot change the story.
    if (isOver && event !is OperationEvent.Note && event !is OperationEvent.Observed) return this

    return when (event) {
        is OperationEvent.Dispatched -> copy(
            state = TrackedState.DISPATCHED,
            updatedAt = event.at,
            trackable = event.trackable,
            retryable = false,
        ).logged(event.at, "Sent to $machineName.")

        is OperationEvent.Replied -> applyReply(event.result, event.at)

        is OperationEvent.Observed -> applyRecord(event.record, event.at)

        is OperationEvent.NotRecorded -> if (!answered) {
            copy(
                state = TrackedState.FINISHED,
                action = action ?: OpAction.FAILED.wire,
                outcome = Outcome.FAILED,
                summary = "${kind.label} on $subjectName never reached $machineName: it has no " +
                    "record of it.",
                updatedAt = event.at,
                // It never arrived, so asking for it again cannot do it twice.
                retryable = true,
            ).logged(event.at, "$machineName has no record of this operation.", isError = true)
        } else {
            copy(
                state = TrackedState.FINISHED,
                outcome = Outcome.UNKNOWN,
                summary = "$machineName no longer has a record of ${kind.label.lowercase()} on " +
                    "$subjectName, so how it ended is not known.",
                updatedAt = event.at,
                retryable = false,
            ).logged(event.at, "$machineName has no record of this operation any more.", isError = true)
        }

        is OperationEvent.TransportFailed -> when (event.stage) {
            DispatchStage.NOT_STARTED -> copy(
                state = TrackedState.FINISHED,
                action = OpAction.FAILED.wire,
                outcome = Outcome.FAILED,
                summary = event.summary,
                detail = event.detail,
                updatedAt = event.at,
                // Nothing reached the machine, so pressing it again cannot do it twice.
                retryable = true,
            ).logged(event.at, event.summary, isError = true)

            // The agent had it, even when what it said could not be read, so an acknowledged
            // failure is reconciled exactly like an ambiguous one.
            DispatchStage.AMBIGUOUS, DispatchStage.ACKNOWLEDGED -> copy(
                state = TrackedState.OUTCOME_UNKNOWN,
                summary = unknownSentence(),
                detail = event.detail,
                updatedAt = event.at,
                // The one thing this whole file exists to prevent. An agent that kept the record is
                // asked about it by its id, which is a read; nothing is ever sent a second time.
                retryable = false,
            ).logged(event.at, event.summary, isError = true)
        }

        is OperationEvent.Resolved -> copy(
            state = TrackedState.FINISHED,
            action = event.action.wire,
            outcome = Outcome.SUCCEEDED,
            summary = event.summary,
            detail = event.detail ?: detail,
            updatedAt = event.at,
            retryable = false,
        ).logged(event.at, event.summary)

        is OperationEvent.GaveUp -> copy(
            state = TrackedState.OUTCOME_UNKNOWN,
            summary = event.summary,
            updatedAt = event.at,
            // Still not offered again. Giving up on finding out is not the same as finding out it
            // did not happen.
            retryable = false,
        ).logged(event.at, event.summary, isError = true)

        is OperationEvent.Expired -> copy(
            state = TrackedState.FINISHED,
            action = OpAction.EXPIRED.wire,
            reasonCode = ReasonCode.EXPIRED.wire,
            outcome = Outcome.DEFERRED,
            summary = "${kind.label} on $subjectName waited for an idle moment and gave up.",
            updatedAt = event.at,
            retryable = true,
        ).logged(event.at, "Expired without running.")

        is OperationEvent.Note -> logged(event.at, event.text, event.isError)
    }
}

/**
 * Whether the agent has ever answered about this change, with a reply or with a record read back.
 *
 * Read off what only an answer leaves behind: an outcome, being queued or running, a phase, progress
 * or an action. Never off [OperationRecord.outcome] alone, which a build before outcomes did not write,
 * so a change such a build saw running would otherwise be taken for one that never arrived and be
 * offered again. A change with none of these was only ever sent, or lost on the way.
 */
private val OperationRecord.answered: Boolean
    get() = outcome != null ||
        state == TrackedState.RUNNING ||
        state == TrackedState.QUEUED ||
        phase != null ||
        progressNote != null ||
        progressStep != null ||
        progressOf != null ||
        action != null

private fun OperationRecord.unknownSentence(): String =
    if (trackable) {
        "${kind.label} on $subjectName was sent and the reply was lost. Asking $machineName what " +
            "became of it."
    } else {
        "${kind.label} on $subjectName was sent and the reply was lost. That agent keeps no record " +
            "to ask about, so the outcome is not known."
    }

private fun OperationRecord.logged(
    at: Long,
    text: String,
    isError: Boolean = false,
): OperationRecord {
    if (log.lastOrNull()?.text == text) return this
    return copy(log = (log + OperationLogLine(at, text, isError)).takeLast(MAX_LOG_LINES))
}

/**
 * One reply from the agent, turned into a state.
 *
 * A v3 agent embeds the operation record, and the record wins: it is the thing the agent will also
 * return from `op ID` and it is the thing the two sides have to agree about. A 2.x agent sends the
 * flat `action` and this reads that instead.
 */
private fun OperationRecord.applyReply(result: AgentActionResult, at: Long): OperationRecord {
    result.op?.let { record ->
        val withNotes = result.notes.orEmpty().fold(this) { carried, note ->
            carried.logged(at, note)
        }
        return withNotes.applyRecord(record, at)
    }
    return applyFlatReply(result, at)
}

/** A v3 reply, read from the operation record the agent embedded. */
private fun OperationRecord.applyRecord(record: OpRecord, at: Long): OperationRecord {
    val result = record.result
    val base = copy(
        updatedAt = at,
        trackable = true,
        phase = record.phase ?: phase,
        progressNote = record.progress?.note ?: progressNote,
        progressStep = record.progress?.step ?: progressStep,
        progressOf = record.progress?.outOf ?: progressOf,
        fromVersion = result?.from ?: record.from ?: fromVersion,
        toVersion = result?.to ?: record.to ?: toVersion,
        detail = result?.message?.takeIf { it.isNotBlank() } ?: detail,
        output = result?.output?.takeIf { it.isNotBlank() } ?: output,
        verified = result?.verified ?: verified,
        reasonCode = result?.reasonCode ?: reasonCode,
        action = result?.action ?: action,
        expiresAt = parseInstantMillis(record.expiresAt) ?: expiresAt,
    ).appendLog(record, at)

    val outcome = record.effectiveOutcome
    if (outcome.isTerminal) return base.settle(outcome, at)

    // Pending: the agent still owns it, and it is followed with `op ID --wait` until it is not. A
    // finished record never reduces to pending from an agent that states outcomes; one from an older
    // agent that ended on `rebooting` or `sleeping` has nothing more to say when read again, so it
    // waits for the machine to be seen instead.
    return when (record.opState) {
        OpState.QUEUED -> base.copy(
            state = TrackedState.QUEUED,
            outcome = outcome,
            summary = queuedSentence(base.expiresAt),
            retryable = false,
        )

        OpState.FINISHED -> base.copy(outcome = outcome).unsettled()

        // Running, or a state this build does not know, which the reducer has already called pending
        // only because the agent said so. A record never reopens a change that is already over.
        else -> base.copy(
            state = if (isOver) state else TrackedState.RUNNING,
            outcome = if (isOver) this.outcome else outcome,
            summary = if (isOver) summary else runningSentence(record),
            retryable = if (isOver) retryable else false,
        )
    }
}

private fun OperationRecord.appendLog(record: OpRecord, at: Long): OperationRecord {
    val lines = record.log.orEmpty().mapNotNull { entry ->
        entry.line?.takeIf { it.isNotBlank() }?.let { line ->
            OperationLogLine(parseInstantMillis(entry.at) ?: at, line)
        }
    }
    if (lines.isEmpty()) return this
    val known = log.map { it.text }.toSet()
    val fresh = lines.filterNot { it.text in known }
    if (fresh.isEmpty()) return this
    return copy(log = (log + fresh).takeLast(MAX_LOG_LINES))
}

/**
 * A change the agent was told about and has not been seen to finish: a 2.x `rebooting` or
 * `sleeping`, which keeps no record to read, or a record that ended on one.
 *
 * Being told is not having done it, so this is not an ending. It waits for the machine to be seen in
 * the state that was asked for, and nothing sends it again.
 */
private fun OperationRecord.unsettled(): OperationRecord = copy(
    state = TrackedState.OUTCOME_UNKNOWN,
    summary = when (opAction) {
        OpAction.REBOOTING -> "$machineName was told to reboot into $subjectName. " +
            "Waiting to see it come back as $subjectName."

        OpAction.SLEEPING -> "$machineName was told to sleep. Waiting to see it stop answering."
        OpAction.QUEUED -> queuedSentence(expiresAt)
        else -> unknownSentence()
    },
    retryable = false,
)

/**
 * A terminal outcome (contract section 5.6), turned into a state and the sentence the user reads.
 *
 * The outcome decides the state and whether a second request is offered; the action only decides the
 * words. A no-op says why, from the reasonCode, and never gets the words "already up to date" put in
 * its mouth. A change the agent held back is over: insisting, or asking later, is a new request with
 * a new id, and only a busy refusal asks first whether to force it. An outcome nobody knows is never
 * offered again, because the first request may well have done it.
 */
private fun OperationRecord.settle(outcome: Outcome, at: Long): OperationRecord {
    val ending = opAction
    val settled = when (outcome) {
        Outcome.PENDING -> return unsettled()

        Outcome.SUCCEEDED -> when (ending) {
            // What was asked for already holds, so asking again would only be told so again.
            OpAction.NOOP -> copy(
                state = TrackedState.FINISHED,
                summary = "${kind.label} on $subjectName changed nothing: ${whyNot()}.",
                retryable = false,
            )

            OpAction.CANCELLED -> copy(
                state = TrackedState.FINISHED,
                summary = "${kind.label} on $subjectName was cancelled before it ran.",
                retryable = true,
            )

            else -> copy(
                state = TrackedState.FINISHED,
                summary = successSentence(),
                retryable = false,
            )
        }

        Outcome.DEFERRED -> when (ending) {
            OpAction.DEFERRED -> copy(
                state = TrackedState.AWAITING_DECISION,
                summary = "${kind.label} on $subjectName was held back: ${whyNot()}.",
                retryable = true,
            )

            OpAction.CANCELLED -> copy(
                state = TrackedState.FINISHED,
                summary = "${kind.label} on $subjectName was cancelled before it ran.",
                retryable = true,
            )

            OpAction.EXPIRED -> copy(
                state = TrackedState.FINISHED,
                summary = "${kind.label} on $subjectName waited for an idle moment and gave up.",
                retryable = true,
            )

            else -> copy(
                state = TrackedState.FINISHED,
                summary = "${kind.label} on $subjectName could not start: ${whyNot()}.",
                retryable = true,
            )
        }

        Outcome.FAILED -> copy(
            state = TrackedState.FINISHED,
            action = action ?: OpAction.FAILED.wire,
            summary = "${kind.label} on $subjectName failed: ${whyNot()}.",
            retryable = true,
        )

        Outcome.UNKNOWN -> copy(
            state = TrackedState.FINISHED,
            summary = when (ending) {
                OpAction.INTERRUPTED -> "${kind.label} on $subjectName was interrupted before it " +
                    "finished, and whether it took effect is not known."

                else -> "${kind.label} on $subjectName ended without saying whether it took effect: " +
                    "${whyNot()}."
            },
            retryable = false,
        )
    }
    return settled.copy(outcome = outcome)
        .logged(at, detail ?: "Finished.", isError = outcome == Outcome.FAILED || outcome == Outcome.UNKNOWN)
}

/**
 * Why nothing happened, in words.
 *
 * The reasonCode first, because it is the field that exists precisely so this sentence does not have
 * to be guessed; the agent's own message second; and a flat statement that it said nothing last.
 * Nothing here ever substitutes a cheerful default.
 */
private fun OperationRecord.whyNot(): String =
    reasonSentence ?: detail?.takeIf { it.isNotBlank() } ?: "the agent gave no reason"

private fun OperationRecord.queuedSentence(expires: Long?): String {
    val tail = expires?.let { ", expiring in ${humanDuration(it - System.currentTimeMillis())}" }.orEmpty()
    return "${kind.label} on $subjectName is waiting for $machineName to be idle$tail."
}

private fun OperationRecord.runningSentence(record: OpRecord): String {
    val doing = OpPhase.describe(record.phase)
    val note = record.progress?.note?.takeIf { it.isNotBlank() }
    val where = listOfNotNull(doing, note).joinToString(": ")
    return if (where.isBlank()) {
        "${kind.label} on $subjectName is running on $machineName."
    } else {
        "${kind.label} on $subjectName: $where."
    }
}

private fun OperationRecord.successSentence(): String = when (kind) {
    OpKind.UPDATE -> {
        val from = fromVersion
        val to = toVersion
        val verifiedNote = if (verified == false) " The agent could not verify it." else ""
        if (from != null && to != null && from != to) {
            "$subjectName updated from $from to $to.$verifiedNote"
        } else {
            "$subjectName updated.$verifiedNote"
        }
    }

    OpKind.RESTART -> "$subjectName restarted."
    OpKind.RUN -> "$subjectName ran."
    OpKind.BOOT ->
        if (opAction == OpAction.ARMED) {
            "$machineName will boot into $subjectName next time. Nothing has rebooted yet."
        } else {
            "$machineName came back as $subjectName."
        }

    OpKind.SLEEP -> "$machineName went to sleep."
    OpKind.CYCLE -> "The maintenance cycle on $machineName finished."
    OpKind.SELF_UPDATE -> "The control agent on $machineName is now ${toVersion ?: "installed"}."
}

/**
 * A reply with no operation record: every 2.x reply, and a refusal from a 3.x agent.
 *
 * Read as generously as the old contract allows and no further. In particular a `noop` from a 2.x
 * agent says nothing about why, and this refuses to invent a reason for it. Rendering
 * "the schedule is off" as "already on the latest build" is worse than saying the agent did not say.
 * The outcome is reduced from the reply's own fields, as every client reduces one (section 5.6): a
 * refusal with a code is a failure, an action nobody knows is unknown, and neither is a success.
 */
private fun OperationRecord.applyFlatReply(result: AgentActionResult, at: Long): OperationRecord {
    val message = result.effectiveMessage
    val base = copy(
        updatedAt = at,
        detail = message ?: detail,
        output = result.effectiveOutput ?: output,
        fromVersion = result.effectiveFrom ?: fromVersion,
        toVersion = result.effectiveTo ?: toVersion,
        verified = result.effectiveVerified ?: verified,
        reasonCode = result.effectiveReasonCode ?: reasonCode,
        action = result.effectiveAction ?: action,
    )

    val outcome = result.effectiveOutcome
    if (outcome != Outcome.PENDING) return base.settle(outcome, at)

    // A pending reply from an agent that keeps records is followed by its id. One from an agent that
    // keeps none can only be settled by seeing the machine in the state that was asked for.
    return if (trackable) {
        base.copy(
            state = TrackedState.RUNNING,
            outcome = outcome,
            summary = "${kind.label} on $subjectName is running on $machineName.",
            retryable = false,
        )
    } else {
        base.copy(outcome = outcome).unsettled()
    }
}

/**
 * An ISO instant as epoch millis, or null.
 *
 * Null rather than an exception on anything unexpected: this runs on a background thread inside a
 * reducer, and a field the agent formatted slightly differently must cost one blank line rather than
 * the whole record.
 */
internal fun parseInstantMillis(iso: String?): Long? {
    val text = iso?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    return runCatching { java.time.Instant.parse(text).toEpochMilli() }.getOrNull()
        ?: runCatching { java.time.OffsetDateTime.parse(text).toInstant().toEpochMilli() }.getOrNull()
}

/** "4 hours", "35 minutes", "in a moment". For an expiry the user has to be able to judge. */
internal fun humanDuration(millis: Long): String {
    if (millis <= 0L) return "a moment"
    val minutes = millis / 60_000L
    return when {
        minutes < 1L -> "under a minute"
        minutes < 90L -> "$minutes minute${if (minutes == 1L) "" else "s"}"
        minutes < 60L * 36L -> {
            val hours = (minutes + 30L) / 60L
            "$hours hour${if (hours == 1L) "" else "s"}"
        }

        else -> {
            val days = (minutes + 720L) / 1440L
            "$days day${if (days == 1L) "" else "s"}"
        }
    }
}
