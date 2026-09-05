package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * How a change moves from a press to an outcome.
 *
 * This is the part of the app that decides whether a restart may be sent a second time, and that is
 * not a decision to check by running it against a real machine. Everything here is the pure reducer:
 * events in, one record out.
 *
 * The rule the whole file exists to protect: a change whose outcome is not known is never repeated.
 */
class OperationReducerTest {

    private val at = 1_700_000_000_000L

    private fun record(
        kind: OpKind = OpKind.RESTART,
        service: String? = "t3",
        force: Boolean = false,
        whenIdle: Boolean = false,
        target: String? = null,
        actionId: String? = null,
    ) = OperationRecord(
        id = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
        machineId = "pi",
        machineName = "Atlas",
        intent = OperationIntent(
            kind = kind,
            service = service,
            target = target,
            actionId = actionId,
            force = force,
            whenIdle = whenIdle,
        ),
        subjectName = service ?: target ?: actionId ?: "Atlas",
        initiator = "this phone",
        startedAt = at,
        updatedAt = at,
        state = TrackedState.REQUESTED,
        summary = "asked for",
    )

    private fun v3(
        state: String,
        action: String? = null,
        reasonCode: String? = null,
        phase: String = "done",
        message: String? = null,
        from: String? = null,
        to: String? = null,
        output: String? = null,
        expiresAt: String? = null,
        verified: Boolean? = null,
    ) = AgentActionResult(
        ok = action != "failed",
        contract = 3,
        op = OpRecord(
            id = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
            state = state,
            phase = phase,
            expiresAt = expiresAt,
            result = action?.let {
                OpResult(
                    ok = it != "failed",
                    action = it,
                    reasonCode = reasonCode,
                    message = message,
                    from = from,
                    to = to,
                    output = output,
                    verified = verified,
                )
            },
        ),
    )

    @Test fun `restricted mutation denial finishes as refused without unknown outcome`() {
        val result = record(kind = OpKind.SELF_UPDATE)
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(OperationEvent.Replied(AgentActionResult(ok = false, reasonCode = "restricted-command", message = "Command refused"), at + 1))
        assertEquals(TrackedState.FINISHED, result.state)
        assertEquals(OpAction.FAILED, result.opAction)
        assertEquals(false, result.succeeded)
        assertTrue(result.retryable)
    }

    @Test fun `refused followup read leaves installation unresolved until same ID reports installed`() {
        val pending = record(kind = OpKind.SELF_UPDATE)
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(OperationEvent.TransportFailed("Link lost", null, DispatchStage.AMBIGUOUS, at + 1))
            .reduce(OperationEvent.Note("Operation read refused", isError = true, at = at + 2))
        assertEquals(TrackedState.OUTCOME_UNKNOWN, pending.state)
        assertNull(pending.succeeded)
        val installed = pending.reduce(OperationEvent.Observed(v3("finished", "installed", to = "3.0.0").op!!, at + 3))
        assertEquals(pending.id, installed.id)
        assertEquals(TrackedState.FINISHED, installed.state)
        assertEquals(OpAction.INSTALLED, installed.opAction)
        assertEquals(true, installed.succeeded)
    }

    // MARK: - The safety rule

    @Test
    fun `a failure before dispatch may be sent again`() {
        val result = record()
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(
                OperationEvent.TransportFailed(
                    summary = "Atlas did not answer.",
                    detail = "connection refused",
                    stage = DispatchStage.NOT_STARTED,
                    at = at,
                ),
            )

        assertEquals(TrackedState.FINISHED, result.state)
        assertEquals(OpAction.FAILED, result.opAction)
        assertTrue("nothing reached the machine, so it is safe to try again", result.retryable)
    }

    @Test
    fun `a link that dropped with the command running is never sent again on its own`() {
        // The whole point. This looks identical to "the machine was never reachable" from here, and
        // treating them the same is how one press becomes two restarts.
        val result = record()
            .reduce(OperationEvent.Dispatched(trackable = false, at = at))
            .reduce(
                OperationEvent.TransportFailed(
                    summary = "Atlas did not answer.",
                    detail = "connection reset",
                    stage = DispatchStage.AMBIGUOUS,
                    at = at,
                ),
            )

        assertEquals(TrackedState.OUTCOME_UNKNOWN, result.state)
        assertNull("no verdict may be drawn from silence", result.succeeded)
        assertFalse(
            "an agent that keeps no record cannot be asked, so this must not be repeated",
            result.retryable,
        )
        assertTrue(result.summary.contains("outcome is not known"))
    }

    @Test
    fun `an ambiguous change against a v3 agent may be sent again, because the id is a question`() {
        // With `--op` the same id is a read: the agent answers with what already happened rather
        // than doing it twice, so retrying is safe in a way it is not against a 2 x agent.
        val result = record()
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(
                OperationEvent.TransportFailed("lost", null, DispatchStage.AMBIGUOUS, at),
            )

        assertEquals(TrackedState.OUTCOME_UNKNOWN, result.state)
        assertTrue(result.retryable)
        assertTrue(result.summary.contains("Asking Atlas"))
    }

    @Test
    fun `a timeout is ambiguous, not a failure`() {
        val failure = AgentFailure.TimedOut(240, "still going")
        assertEquals(DispatchStage.AMBIGUOUS, failure.dispatch)
    }

    // MARK: - Replies

    @Test
    fun `an accepted change is running, not finished`() {
        val result = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "running", phase = "installing"), at))

        assertEquals(TrackedState.RUNNING, result.state)
        assertFalse(result.isOver)
        assertTrue(result.summary.contains("installing"))
    }

    @Test
    fun `a deferred change asks rather than deciding, and says why`() {
        val result = record()
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(state = "finished", action = "deferred", reasonCode = "busy"),
                    at,
                ),
            )

        assertEquals(TrackedState.AWAITING_DECISION, result.state)
        assertTrue("a busy refusal is what force is for", result.invitesForce)
        assertTrue(result.summary.contains("the machine was working"))
    }

    @Test
    fun `a change deferred by policy never offers force`() {
        // Force bypasses the busy gate only. Offering it here would be offering to
        // override a decision the user made, using a flag that cannot do that.
        val result = record()
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(state = "finished", action = "deferred", reasonCode = "policy-paused"),
                    at,
                ),
            )

        assertEquals(TrackedState.AWAITING_DECISION, result.state)
        assertFalse(result.invitesForce)
        assertTrue(result.summary.contains("updates are paused"))
    }

    @Test
    fun `a forced change that was still deferred does not ask to force again`() {
        val result = record(force = true)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(state = "finished", action = "deferred", reasonCode = "busy"),
                    at,
                ),
            )
        assertFalse(result.invitesForce)
    }

    @Test
    fun `a no-op says the reason it was given and never invents a cheerful one`() {
        // The review found "auto-update is off" being rendered as "already on the latest build".
        val off = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(state = "finished", action = "noop", reasonCode = "policy-off"),
                    at,
                ),
            )
        assertEquals(TrackedState.FINISHED, off.state)
        assertTrue(off.summary.contains("the schedule is off"))
        assertFalse(off.summary.contains("up to date"))

        val nothing = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "noop"), at))
        assertTrue(
            "an agent that said nothing gets \"it reported no reason\", not a guess",
            nothing.summary.contains("gave no reason"),
        )
    }

    @Test
    fun `an update that worked names both versions and says when it was not verified`() {
        val verified = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(state = "finished", action = "updated", from = "1.0.0", to = "1.1.0"),
                    at,
                ),
            )
        assertEquals(true, verified.succeeded)
        assertTrue(verified.summary.contains("1.0.0 to 1.1.0"))

        val unverified = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(
                        state = "finished",
                        action = "updated",
                        from = "1.0.0",
                        to = "1.1.0",
                        verified = false,
                    ),
                    at,
                ),
            )
        assertTrue(
            "a forced install with no way to check has to say so",
            unverified.summary.contains("could not verify"),
        )
    }

    @Test
    fun `an action carries what it printed`() {
        val result = record(kind = OpKind.RUN, service = null, actionId = "sunshine-restart")
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(state = "finished", action = "ran", output = "Stopping sunshine.service"),
                    at,
                ),
            )
        assertEquals(true, result.succeeded)
        assertEquals("Stopping sunshine.service", result.output)
    }

    // MARK: - Boot and sleep

    @Test
    fun `being told to reboot leaves the outcome unknown until the machine is seen`() {
        // The agent accepting a reboot is not the machine having rebooted, and the
        // old app drew the second from the first.
        val told = record(kind = OpKind.BOOT, service = null, target = "windows")
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "rebooting"), at))

        assertEquals(TrackedState.OUTCOME_UNKNOWN, told.state)
        assertNull(told.succeeded)
        assertTrue(told.summary.contains("Waiting to see it come back"))

        val seen = told.reduce(
            OperationEvent.Resolved(OpAction.REBOOTED, "Atlas came back as windows.", at = at),
        )
        assertEquals(TrackedState.FINISHED, seen.state)
        assertEquals(true, seen.succeeded)
    }

    @Test
    fun `a lost sleep acknowledgement leaves the outcome unknown`() {
        val result = record(kind = OpKind.SLEEP, service = null)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.TransportFailed("No response", null, DispatchStage.AMBIGUOUS, at + 1))
        assertEquals(TrackedState.OUTCOME_UNKNOWN, result.state)
        assertNull(result.succeeded)
    }

    @Test
    fun `arming the next boot is not rebooting`() {
        val armed = record(kind = OpKind.BOOT, service = null, target = "windows")
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "armed"), at))
        assertEquals(TrackedState.FINISHED, armed.state)
        assertTrue(armed.summary.contains("Nothing has rebooted yet"))
    }

    @Test
    fun `being told to sleep waits to see the machine go quiet`() {
        val told = record(kind = OpKind.SLEEP, service = null)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "sleeping"), at))
        assertEquals(TrackedState.OUTCOME_UNKNOWN, told.state)
        assertTrue(told.summary.contains("Waiting to see it stop answering"))
    }

    // MARK: - Queued work

    @Test
    fun `a queued change shows its expiry and can be cancelled`() {
        val queued = record(whenIdle = true)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(
                        state = "queued",
                        phase = "queued",
                        expiresAt = "2026-09-05T18:00:00.000Z",
                    ),
                    at,
                ),
            )

        assertEquals(TrackedState.QUEUED, queued.state)
        assertTrue(queued.isCancellable)
        assertEquals(1_788_631_200_000L, queued.expiresAt)

        val expired = queued.reduce(OperationEvent.Expired(at))
        assertEquals(TrackedState.FINISHED, expired.state)
        assertEquals(OpAction.EXPIRED, expired.opAction)
        assertTrue("nothing ran, so asking again is safe", expired.retryable)
    }

    @Test
    fun `queued footer follows cancellation and ignores a late queued reply`() {
        val queued = record(whenIdle = true)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "queued", phase = "queued"), at))
        val footer = com.x1f4r.legioncontrol.ui.MachineMessage.forOperation(queued)
        assertTrue(footer.line.contains("waiting"))

        val cancelled = queued.reduce(
            OperationEvent.Replied(v3(state = "finished", action = "cancelled"), at + 1),
        )
        val late = cancelled.reduce(
            OperationEvent.Replied(v3(state = "queued", phase = "queued"), at + 2),
        )
        assertEquals(TrackedState.FINISHED, late.state)
        assertEquals(OpAction.CANCELLED, late.opAction)
        val resolved = footer.resolve(listOf(late))
        assertEquals(late.summary, resolved.line)
        assertTrue(resolved.line.contains("cancelled"))
        assertFalse(resolved.line.contains("waiting"))

        val newerMessage = com.x1f4r.legioncontrol.ui.MachineMessage(line = "Diagnostics ready.")
        assertEquals("Diagnostics ready.", newerMessage.resolve(listOf(late)).line)
    }

    @Test
    fun `a cancelled change is over and safe to ask for again`() {
        val cancelled = record(whenIdle = true)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "cancelled"), at))
        assertEquals(TrackedState.FINISHED, cancelled.state)
        assertTrue(cancelled.retryable)
    }

    // MARK: - Conflicts and endings

    @Test
    fun `a conflict names what is in the way rather than saying busy`() {
        val result = record()
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    v3(
                        state = "finished",
                        action = "conflict",
                        reasonCode = "operation-in-progress",
                    ),
                    at,
                ),
            )
        assertTrue(result.summary.contains("another change was already running"))
        assertFalse("force never breaks the machine's lock", result.invitesForce)
    }

    @Test
    fun `a finished change is not reopened by a late reply`() {
        val finished = record()
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "restarted"), at))
        assertEquals(TrackedState.FINISHED, finished.state)

        val late = finished.reduce(
            OperationEvent.TransportFailed("lost", null, DispatchStage.AMBIGUOUS, at + 1),
        )
        assertEquals("a late failure must not undo a known outcome", TrackedState.FINISHED, late.state)
        assertEquals(true, late.succeeded)
    }

    @Test
    fun `giving up on finding out is not finding out that it failed`() {
        val given = record()
            .reduce(OperationEvent.Dispatched(false, at))
            .reduce(OperationEvent.TransportFailed("lost", null, DispatchStage.AMBIGUOUS, at))
            .reduce(OperationEvent.GaveUp("Atlas never settled it.", at))

        assertEquals(TrackedState.OUTCOME_UNKNOWN, given.state)
        assertNull(given.succeeded)
        assertFalse(given.retryable)
    }

    // MARK: - Legacy replies

    @Test
    fun `a 2 x reply with no operation record still produces an outcome`() {
        val result = record()
            .reduce(OperationEvent.Dispatched(false, at))
            .reduce(
                OperationEvent.Replied(
                    AgentActionResult(ok = true, action = "restarted", message = "t3 restarted"),
                    at,
                ),
            )
        assertEquals(TrackedState.FINISHED, result.state)
        assertEquals(true, result.succeeded)
    }

    @Test
    fun `a 2 x no-op does not have a reason invented for it`() {
        val result = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(false, at))
            .reduce(OperationEvent.Replied(AgentActionResult(ok = true, action = "noop"), at))
        assertTrue(result.summary.contains("gave no reason"))
    }

    // MARK: - Log

    @Test
    fun `the log keeps what happened without repeating itself`() {
        var result = record().reduce(OperationEvent.Dispatched(true, at))
        repeat(5) { result = result.reduce(OperationEvent.Note("same line", at = at)) }
        assertEquals(2, result.log.size)

        result = result.reduce(OperationEvent.Note("another", at = at))
        assertEquals(3, result.log.size)
        assertTrue(result.log.first().text.contains("Sent to Atlas"))
    }

    @Test
    fun `an operation id binds its intent`() {
        // The same id with a different request is a conflict, not a replay. The intent
        // is stored beside the id so a retry is provably the same request.
        val original = record(kind = OpKind.RESTART, service = "t3")
        assertEquals(OpKind.RESTART, original.intent.kind)
        assertEquals("t3", original.intent.service)
        assertEquals(OpMode.MANUAL, original.intent.mode)
        assertEquals(OpMode.FORCE, record(force = true).intent.mode)
        assertEquals(OpMode.QUEUED, record(whenIdle = true).intent.mode)
    }
}
