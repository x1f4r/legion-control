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

    /** The agent held it back and the user is being asked whether to insist. */
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

    val isOver: Boolean get() = this == FINISHED

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
    /** True only when sending this again cannot do it twice. */
    val retryable: Boolean = false,
    /** True when the agent accepted `--op` and can therefore be asked what happened. */
    val trackable: Boolean = false,
    val log: List<OperationLogLine> = emptyList(),
) {
    val isOver: Boolean get() = state.isOver
    val isPending: Boolean get() = state.isPending

    val kind: OpKind get() = intent.kind

    val opAction: OpAction? get() = OpAction.fromWire(action)
    val reason: ReasonCode? get() = ReasonCode.fromWire(reasonCode)

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

    /** Whether it ended well. Null while it is not over, and while the outcome is not known. */
    val succeeded: Boolean?
        get() = when (state) {
            TrackedState.FINISHED -> opAction?.isSuccess ?: (reasonCode == null)
            else -> null
        }

    /** Whether it ended in a way worth telling the user about when they were not looking. */
    val isNoteworthyEnding: Boolean
        get() = isOver && opAction != OpAction.NOOP

    /** Progress as a fraction, when the agent counted steps. */
    val progressFraction: Float?
        get() {
            val step = progressStep ?: return null
            val total = progressOf?.takeIf { it > 0 } ?: return null
            return (step.toFloat() / total.toFloat()).coerceIn(0f, 1f)
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
     * The transport gave up. [stage] decides whether this can be sent again.
     *
     * [DispatchStage.NOT_STARTED] finishes it as failed and retryable; [DispatchStage.AMBIGUOUS]
     * moves it to [TrackedState.OUTCOME_UNKNOWN] and never to a verdict.
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

        is OperationEvent.TransportFailed -> when (event.stage) {
            DispatchStage.NOT_STARTED -> copy(
                state = TrackedState.FINISHED,
                action = OpAction.FAILED.wire,
                summary = event.summary,
                detail = event.detail,
                updatedAt = event.at,
                // Nothing reached the machine, so pressing it again cannot do it twice.
                retryable = true,
            ).logged(event.at, event.summary, isError = true)

            DispatchStage.AMBIGUOUS -> copy(
                state = TrackedState.OUTCOME_UNKNOWN,
                summary = unknownSentence(),
                detail = event.detail,
                updatedAt = event.at,
                // The one thing this whole file exists to prevent, unless the agent kept the record,
                // in which case sending the same id again is a read and not a repeat.
                retryable = trackable,
            ).logged(event.at, event.summary, isError = true)
        }

        is OperationEvent.Resolved -> copy(
            state = TrackedState.FINISHED,
            action = event.action.wire,
            summary = event.summary,
            detail = event.detail ?: detail,
            updatedAt = event.at,
            retryable = false,
        ).logged(event.at, event.summary)

        is OperationEvent.GaveUp -> copy(
            state = TrackedState.OUTCOME_UNKNOWN,
            summary = event.summary,
            updatedAt = event.at,
            // Still not retryable unless the agent keeps records. Giving up on finding out is not
            // the same as finding out it did not happen.
            retryable = trackable,
        ).logged(event.at, event.summary, isError = true)

        is OperationEvent.Expired -> copy(
            state = TrackedState.FINISHED,
            action = OpAction.EXPIRED.wire,
            reasonCode = ReasonCode.EXPIRED.wire,
            summary = "${kind.label} on $subjectName waited for an idle moment and gave up.",
            updatedAt = event.at,
            retryable = true,
        ).logged(event.at, "Expired without running.")

        is OperationEvent.Note -> logged(event.at, event.text, event.isError)
    }
}

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

    return when (record.opState) {
        OpState.QUEUED -> base.copy(
            state = TrackedState.QUEUED,
            summary = queuedSentence(base.expiresAt),
            retryable = false,
        )

        OpState.RUNNING -> base.copy(
            state = TrackedState.RUNNING,
            summary = runningSentence(record),
            retryable = false,
        )

        OpState.FINISHED -> base.finish(at)

        // A record with no state this build recognises. Treated as still running rather than as an
        // ending: an unrecognised value must never become a verdict.
        null -> base.copy(
            state = if (state == TrackedState.FINISHED) state else TrackedState.RUNNING,
            summary = runningSentence(record),
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
 * A finished record, turned into the sentence the user reads.
 *
 * The two cases worth the care are `noop` and the boot/sleep pair. A no-op says why, from the
 * reasonCode, and never gets the words "already up to date" put in its mouth. A `rebooting` or
 * `sleeping` result is not an ending at all, whatever `state` says, because the machine was told and
 * being told is not having done it.
 */
private fun OperationRecord.finish(at: Long): OperationRecord {
    val settledAction = opAction
    if (settledAction != null && !settledAction.isSettled) {
        return copy(
            state = TrackedState.OUTCOME_UNKNOWN,
            summary = when (settledAction) {
                OpAction.REBOOTING -> "$machineName was told to reboot into $subjectName. " +
                    "Waiting to see it come back as $subjectName."

                OpAction.SLEEPING -> "$machineName was told to sleep. Waiting to see it stop answering."
                OpAction.QUEUED -> queuedSentence(expiresAt)
                else -> unknownSentence()
            },
            retryable = false,
        )
    }

    return when (settledAction) {
        OpAction.DEFERRED -> copy(
            state = TrackedState.AWAITING_DECISION,
            summary = "${kind.label} on $subjectName was held back: ${whyNot()}.",
            retryable = false,
        )

        OpAction.NOOP -> copy(
            state = TrackedState.FINISHED,
            summary = "${kind.label} on $subjectName changed nothing: ${whyNot()}.",
            retryable = true,
        )

        OpAction.CONFLICT -> copy(
            state = TrackedState.FINISHED,
            summary = "${kind.label} on $subjectName could not start: ${whyNot()}.",
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

        OpAction.INTERRUPTED -> copy(
            state = TrackedState.FINISHED,
            summary = "${kind.label} on $subjectName was interrupted before it finished.",
            retryable = true,
        )

        OpAction.FAILED, null -> copy(
            state = TrackedState.FINISHED,
            action = action ?: OpAction.FAILED.wire,
            summary = "${kind.label} on $subjectName failed: ${whyNot()}.",
            retryable = true,
        )

        else -> copy(
            state = TrackedState.FINISHED,
            summary = successSentence(),
            retryable = false,
        )
    }.logged(at, detail ?: "Finished.", isError = settledAction?.isSuccess == false)
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
 * A 2.x reply, which has no operation record and no reasonCode.
 *
 * Read as generously as the old contract allows and no further. In particular a `noop` from a 2.x
 * agent says nothing about why, and this refuses to invent a reason for it. Rendering
 * "the schedule is off" as "already on the latest build" is worse than saying the agent did not say.
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

    val settled = OpAction.fromWire(result.effectiveAction)
    if (settled == null && result.ok == false) {
        return base.copy(
            state = TrackedState.FINISHED,
            action = OpAction.FAILED.wire,
            summary = "${kind.label} on $subjectName failed: ${base.whyNot()}.",
            retryable = true,
        ).logged(at, base.whyNot(), isError = true)
    }

    return base.finish(at)
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
