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
        machineName = "Beacon",
        intent = OperationIntent(
            kind = kind,
            service = service,
            target = target,
            actionId = actionId,
            force = force,
            whenIdle = whenIdle,
        ),
        subjectName = service ?: target ?: actionId ?: "Beacon",
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
                    summary = "Beacon did not answer.",
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
                    summary = "Beacon did not answer.",
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
    fun `an ambiguous change against a v3 agent is asked about by its id and never sent again`() {
        // With `--op` the same id is a read: the agent answers with what already happened. That is
        // what the record is followed by, and it is not a second request (contract section 5.6).
        val result = record()
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(
                OperationEvent.TransportFailed("lost", null, DispatchStage.AMBIGUOUS, at),
            )

        assertEquals(TrackedState.OUTCOME_UNKNOWN, result.state)
        assertTrue("it is followed by its id", result.trackable)
        assertFalse("an outcome nobody knows is never offered again", result.retryable)
        assertTrue(result.summary.contains("Asking Beacon"))
    }

    @Test
    fun `a lost reply the agent has no record of never reached it`() {
        val lost = record()
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(OperationEvent.TransportFailed("lost", null, DispatchStage.AMBIGUOUS, at))
            .reduce(OperationEvent.NotRecorded(at + 1))

        assertEquals(TrackedState.FINISHED, lost.state)
        assertEquals(Outcome.FAILED, lost.outcome)
        assertEquals(false, lost.succeeded)
        assertTrue("it never arrived, so asking again cannot do it twice", lost.retryable)
        assertTrue(lost.summary.contains("never reached Beacon"))
    }

    @Test
    fun `a record the agent has pruned since is not known, and not offered again`() {
        val pruned = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(trackable = true, at = at))
            .reduce(OperationEvent.Replied(v3(state = "running", phase = "installing"), at))
            .reduce(OperationEvent.NotRecorded(at + 1))

        assertEquals(TrackedState.FINISHED, pruned.state)
        assertEquals(Outcome.UNKNOWN, pruned.outcome)
        assertNull(pruned.succeeded)
        assertFalse(pruned.retryable)
    }

    @Test
    fun `a change a build before outcomes saw answered is not taken for one that never arrived`() {
        // What a 1.3 build left on disk: the agent's answers are there, and no outcome, because that
        // build wrote none. The machine has since pruned the record.
        val written = record(kind = OpKind.UPDATE).copy(trackable = true, outcome = null)
        val answered = listOf(
            "running" to written.copy(state = TrackedState.RUNNING),
            "queued" to written.copy(state = TrackedState.QUEUED, intent = written.intent.copy(whenIdle = true)),
            "given up on after running" to written.copy(
                state = TrackedState.OUTCOME_UNKNOWN,
                phase = "installing",
                progressStep = 4,
                progressOf = 8,
            ),
            "told it was rebooting" to written.copy(state = TrackedState.OUTCOME_UNKNOWN, action = "rebooting"),
        )

        answered.forEach { (how, legacy) ->
            val pruned = legacy.reduce(OperationEvent.NotRecorded(at + 1))
            assertEquals(how, TrackedState.FINISHED, pruned.state)
            assertEquals("$how: it may have run, so how it ended is not known", Outcome.UNKNOWN, pruned.outcome)
            assertFalse("$how: it may have run, so it is not offered again", pruned.retryable)
            assertTrue(how, pruned.summary.contains("no longer has a record"))
        }

        // One that build only ever sent, and lost, still never arrived.
        val lost = written.copy(state = TrackedState.OUTCOME_UNKNOWN).reduce(OperationEvent.NotRecorded(at + 1))
        assertEquals(Outcome.FAILED, lost.outcome)
        assertTrue(lost.retryable)
    }

    @Test
    fun `a reply that could not be read is reconciled like a dropped link, never resent`() {
        val result = record()
            .reduce(OperationEvent.Dispatched(trackable = false, at = at))
            .reduce(OperationEvent.TransportFailed("unreadable", null, DispatchStage.ACKNOWLEDGED, at))

        assertEquals(TrackedState.OUTCOME_UNKNOWN, result.state)
        assertNull(result.succeeded)
        assertFalse(result.retryable)
        assertTrue(DispatchStage.ACKNOWLEDGED.mayHaveRun)
        assertFalse(DispatchStage.NOT_STARTED.mayHaveRun)
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
        assertEquals(Outcome.DEFERRED, result.outcome)
        // Over: the agent is done with it, so nothing reads it again, and asking later is a new
        // request. It used to stay pending and be read back in a tight loop for half an hour.
        assertTrue(result.isOver)
        assertFalse(result.isPending)
        assertTrue(result.retryable)
        assertNull("held back is neither a success nor a failure", result.succeeded)
        assertFalse("the question is on screen; it is not a notification", result.isNoteworthyEnding)
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
            OperationEvent.Resolved(OpAction.REBOOTED, "Beacon came back as windows.", at = at),
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

    /**
     * Two sleeps queued for an idle moment still read "waiting for idle" on the Mac nine days later:
     * their agents had expired and pruned them. Here such a change is expired once its expiry has
     * passed, or once it has waited longer than any agent holds one, without claiming it never ran.
     * Before then the agent let go of a request it was still meant to hold: a lost record.
     */
    @Test
    fun `a queued change the agent has pruned is expired once its expiry has passed`() {
        val hour = 3_600_000L
        val queued = record(kind = OpKind.SLEEP, service = null, whenIdle = true)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "queued", phase = "queued"), at))
            .copy(expiresAt = at + 4 * hour)
        assertEquals(TrackedState.QUEUED, queued.state)

        assertTrue(queued.reduce(OperationEvent.NotRecorded(at + hour)).summary.endsWith("how it ended is not known."))

        val expired = queued.reduce(OperationEvent.NotRecorded(at + 5 * hour))
        assertEquals(TrackedState.FINISHED, expired.state)
        assertEquals(Outcome.UNKNOWN, expired.outcome)
        assertFalse("it may have run before then, so it is not offered again", expired.retryable)
        assertEquals(
            "Sleep on Beacon expired while it was queued. Beacon no longer has a record of it, so whether it ran before then is not known.",
            expired.summary,
        )

        // One that came back with no expiry is held seven days at the most.
        val open = queued.copy(expiresAt = null)
        assertTrue(open.reduce(OperationEvent.NotRecorded(at + 6 * 24 * hour)).summary.endsWith("how it ended is not known."))
        assertTrue(open.reduce(OperationEvent.NotRecorded(at + 8 * 24 * hour)).summary.contains("expired while it was queued"))
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
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "cancelled", reasonCode = "cancelled"), at))
        assertEquals(TrackedState.FINISHED, cancelled.state)
        assertEquals("a cancelled request was itself deferred", Outcome.DEFERRED, cancelled.outcome)
        assertTrue(cancelled.retryable)
        assertTrue(cancelled.summary.contains("cancelled before it ran"))
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
            .reduce(OperationEvent.GaveUp("Beacon never settled it.", at))

        assertEquals(TrackedState.OUTCOME_UNKNOWN, given.state)
        assertNull(given.succeeded)
        assertFalse(given.retryable)
    }

    // MARK: - Outcomes (contract section 5.6)

    @Test
    fun `an action nobody knows is unknown, never a failure that is offered again`() {
        // This app used to read every action it did not know as a failure and offer to send it
        // again, while the desktop called the same reply a success.
        val result = record()
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "teleported"), at))

        assertEquals(TrackedState.FINISHED, result.state)
        assertEquals(Outcome.UNKNOWN, result.outcome)
        assertNull(result.succeeded)
        assertFalse(result.retryable)
    }

    @Test
    fun `an outcome the agent states beats what its action would say`() {
        // The agent's own producer marks a failure it may have left half done as unknown.
        val reply = AgentActionResult(
            ok = false,
            contract = 3,
            op = OpRecord(
                state = "finished",
                outcome = "unknown",
                result = OpResult(ok = false, action = "failed", reasonCode = "apply-failed", outcome = "unknown"),
            ),
        )
        val result = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(reply, at))

        assertEquals(Outcome.UNKNOWN, result.outcome)
        assertNull(result.succeeded)
        assertFalse("nobody knows whether the first one took effect", result.retryable)
    }

    @Test
    fun `a timed out action may have run, so it is not known and not sent again`() {
        val result = record(kind = OpKind.RUN, service = null, actionId = "nightly")
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "failed", reasonCode = "timed-out"), at))

        assertEquals(TrackedState.FINISHED, result.state)
        assertEquals(Outcome.UNKNOWN, result.outcome)
        assertFalse(result.retryable)
    }

    @Test
    fun `a refusal with a code is a failure, and one that only lacks the lock is held back`() {
        val refused = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(AgentActionResult(ok = false, contract = 3, reasonCode = "not-installed"), at))
        assertEquals(Outcome.FAILED, refused.outcome)
        assertEquals(false, refused.succeeded)
        assertTrue(refused.retryable)

        val locked = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(AgentActionResult(ok = false, contract = 3, reasonCode = "lock-held"), at))
        assertEquals(Outcome.DEFERRED, locked.outcome)
        assertTrue(locked.isOver)
        assertTrue(locked.summary.contains("could not start"))
    }

    @Test
    fun `a reply that is still running is followed, never settled`() {
        val result = record(kind = OpKind.UPDATE)
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(
                OperationEvent.Replied(
                    AgentActionResult(
                        ok = true,
                        contract = 3,
                        action = "already-running",
                        reasonCode = "already-running",
                        outcome = "pending",
                        op = OpRecord(state = "running", phase = "installing", outcome = "pending"),
                    ),
                    at,
                ),
            )
        assertEquals(TrackedState.RUNNING, result.state)
        assertEquals(Outcome.PENDING, result.outcome)
        assertFalse(result.isOver)
    }

    @Test
    fun `a record that says it is running never reopens a change already over`() {
        val finished = record()
            .reduce(OperationEvent.Dispatched(true, at))
            .reduce(OperationEvent.Replied(v3(state = "finished", action = "restarted"), at))
        val late = finished.reduce(OperationEvent.Observed(v3(state = "running", phase = "restarting").op!!, at + 1))
        assertEquals(TrackedState.FINISHED, late.state)
        assertEquals(true, late.succeeded)
    }

    @Test
    fun `whether an action settles anything is the reducer's answer`() {
        for (action in OpAction.entries) {
            assertEquals(action.wire, Outcome.reduce(action = action.wire) != Outcome.PENDING, action.isSettled)
        }
        assertFalse(OpAction.ALREADY_RUNNING.isSettled)
        assertFalse(OpAction.REBOOTING.isSettled)
        assertTrue(OpAction.ARMED.isSettled)
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
        assertTrue(result.log.first().text.contains("Sent to Beacon"))
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
