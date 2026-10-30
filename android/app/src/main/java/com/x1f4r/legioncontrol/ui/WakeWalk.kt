package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.HelperState
import com.x1f4r.legioncontrol.agent.HoldChoice
import com.x1f4r.legioncontrol.agent.InterruptRequest
import com.x1f4r.legioncontrol.agent.OpAction
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.Outcome
import com.x1f4r.legioncontrol.agent.WakeHelper
import com.x1f4r.legioncontrol.agent.WakeHold
import com.x1f4r.legioncontrol.agent.WakePlan
import com.x1f4r.legioncontrol.agent.WakeSkip
import com.x1f4r.legioncontrol.agent.WakeStep
import com.x1f4r.legioncontrol.agent.helperSkip
import com.x1f4r.legioncontrol.agent.isHeld
import com.x1f4r.legioncontrol.agent.otherHelpers
import com.x1f4r.legioncontrol.agent.planWake
import com.x1f4r.legioncontrol.agent.readinessSchedule
import com.x1f4r.legioncontrol.agent.runAgain
import kotlinx.coroutines.delay

/**
 * One press of Wake: the plan of contract section 10.2, walked one step at a time.
 *
 * Steps run in order. A direct step that sent packets, or a helper action that finished `ran`, is
 * followed by readiness: the target's ordinary authenticated status poll, at 5 s and then at
 * intervals growing 1.6 times to at most 20 s, for 120 s. The target answering ends the wake;
 * anything else moves on to the next step, and after the last one the walk says the target did not
 * come up, with a line for every step taken and every entry skipped. Sending a packet is not waking
 * a machine.
 *
 * A helper's action is somebody's script unless the helper declares it `wol`, and a script whose
 * result is lost must not run a second time on another helper. So every helper action is a durable
 * operation record, and one that is not a declared `wol` carries its target: while its result is
 * unknown it is a wake hold, and no command helper runs for that target, on this press or any later
 * one, until the hold is released.
 */
internal class WakeWalk(
    private val target: MachineClient,
    private val fleet: FleetContext,
    private val operations: OperationCenter,
    /** One ordinary authenticated status poll of the target: true when it answered. */
    private val poll: suspend () -> Boolean,
    /** The line to show while the walk is under way. */
    private val progress: (String) -> Unit,
    /** Milliseconds, for the readiness schedule. Monotonic, so a clock change cannot stretch it. */
    private val clock: () -> Long = { System.nanoTime() / 1_000_000L },
    private val sleep: suspend (Long) -> Unit = { delay(it) },
) {
    /** How it ended: whether the target answered, and what was tried on the way. */
    data class Ending(
        val answered: Boolean,
        /** One line per step taken and per entry skipped, for the detail under "did not come up". */
        val lines: List<String>,
        /** Helpers skipped as asleep that could themselves be woken, as a separate press. */
        val offerWakeFirst: List<String>,
        /**
         * Whether anything was tried at all: a packet this device tried to send, or a helper it
         * asked. False when every step was skipped, which is "cannot wake it from here" rather than
         * "it did not come up".
         */
        val tried: Boolean = true,
        /**
         * Why a person's way past the holds was not taken, when it would have asked nobody: nothing
         * was tried and nothing was released.
         */
        val refusal: String? = null,
    )

    private val machine get() = target.machine

    /** Whether a step got as far as sending a packet or asking a helper. */
    private var tried = false

    /** The holds a person chose to go past by asking the other helpers: their steps are not taken. */
    private var passed: List<WakeHold> = emptyList()

    /**
     * Walks the plan. [choice] is a person's decision about the holds still standing once their
     * records were read, and nothing else ever makes one on their behalf. Either way past releases
     * those holds. [HoldChoice.OTHER_HELPERS] does not ask the helpers the holds were on in this walk,
     * and with no other helper to ask it walks nothing and the holds stand: releasing them would ask
     * nobody new, and leave the held command free to run again on the next press without anyone
     * having chosen that. [HoldChoice.RUN_AGAIN] is choosing exactly that, and is taken only when no
     * other helper could be asked instead.
     */
    suspend fun run(choice: HoldChoice = HoldChoice.STAND): Ending {
        settleHolds()
        var plan = plan()
        if (choice != HoldChoice.STAND && plan.isHeld) {
            val holds = holds()
            val past = if (choice == HoldChoice.RUN_AGAIN) runAgain(plan, plan(emptyList()), holds) else otherHelpers(plan, plan(emptyList()), holds)
            if (past == null) {
                val refusal = if (choice == HoldChoice.RUN_AGAIN) otherHelperFirst(machine.name) else noOtherHelper(machine.name)
                return Ending(false, listOf(refusal), emptyList(), tried = false, refusal = refusal)
            }
            operations.releaseWakeHolds(
                machine.id,
                before = Long.MAX_VALUE,
                reason = if (choice == HoldChoice.RUN_AGAIN) {
                    "Released by hand, to run the held command for ${machine.name} again."
                } else {
                    "Released by hand, to try the other helpers for ${machine.name}."
                },
            )
            if (choice == HoldChoice.OTHER_HELPERS) passed = holds
            plan = past
        }
        val lines = mutableListOf<String>()
        val offer = plan.offerWakeFirst.toMutableList()

        for (step in plan.steps) {
            val answered = when (step) {
                is WakeStep.Direct -> direct(step, lines)
                is WakeStep.Helper -> helper(step, lines, offer)
            }
            if (answered) return Ending(true, lines, emptyList())
        }
        lines += plan.skipped.map(::describe)
        return Ending(false, lines, offer, tried)
    }

    /**
     * The plan as things stand now, with the holds this device recorded for the target, or with
     * [holds] when the caller already has them, as a screen does that has to see them change.
     */
    fun plan(holds: List<WakeHold> = holds()): WakePlan = planWake(
        target = machine,
        machines = fleet.machines,
        sites = fleet.sites,
        addresses = fleet.localAddresses,
        currentSite = fleet.currentSite,
        helpers = machine.wake?.helpers.orEmpty().associate { it.machineId to fleet.helperState(it.machineId) },
        holds = holds,
    )

    private fun holds(): List<WakeHold> =
        operations.wakeHolds(machine.id).map { WakeHold(it.machineId, it.intent.actionId.orEmpty()) }

    /**
     * Before planning, reads `op ID` once on each held helper that keeps records and was not last
     * seen asleep. A record read finished, or an answer that there is no such operation, releases
     * the hold; anything else leaves it, because a hold is never released by time or by a helper
     * being out of reach.
     */
    private suspend fun settleHolds() {
        for (hold in operations.wakeHolds(machine.id)) {
            val helper = fleet.client(hold.machineId) ?: continue
            if (!hold.trackable) continue
            if (fleet.helperState(hold.machineId).state == HelperState.Kind.ASLEEP) continue
            operations.readOnce(helper, hold.id, waitSeconds = null)
        }
    }

    private suspend fun direct(step: WakeStep.Direct, lines: MutableList<String>): Boolean {
        progress("Sending the wake packet to ${machine.name}.")
        tried = true
        val send = target.sendWakePacket(step.broadcast, step.ports, step.placing?.network)
        if (send.sent == 0) {
            lines += "No wake packet left this device: ${send.problem ?: "nothing could be sent"}."
            return false
        }
        progress("Wake packet sent. Waiting for ${machine.name} to answer.")
        if (awaitReadiness()) return true
        lines += "This device sent ${send.sent} wake packets to ${step.broadcast.joinToString(", ")}, and " +
            "${machine.name} did not answer."
        return false
    }

    private suspend fun helper(step: WakeStep.Helper, lines: MutableList<String>, offer: MutableList<String>): Boolean {
        val helper = fleet.client(step.machine)
        val name = helper?.machine?.name ?: step.machine
        val entry = WakeHelper(step.machine, step.action)
        var state = fleet.helperState(step.machine)
        if (step.readFirst || state.state == HelperState.Kind.UNKNOWN) {
            // Read once, just before its step. A helper that is still not ready after that read is
            // asleep when the read could not reach it, and of no use for anything else.
            progress("Checking $name first.")
            state = fleet.readHelper(step.machine).let {
                if (it.state == HelperState.Kind.UNKNOWN) HelperState(HelperState.Kind.UNUSABLE) else it
            }
        }
        // Asked again here, with the state just read and the holds as they are now: a hold recorded
        // by an earlier step of this same press stops every command helper after it.
        val reason = helperSkip(machine.id, fleet.machines, entry, emptyList(), state, held = holds().isNotEmpty())
        if (helper == null || reason != null) {
            val skip = WakeSkip(WakeSkip.Kind.HELPER, step.machine, step.action, reason ?: WakeSkip.Reason.MISSING)
            lines += describe(skip)
            val ownWake = fleet.machines.firstOrNull { it.id == step.machine }?.wake != null
            if (skip.reason == WakeSkip.Reason.ASLEEP && ownWake && step.machine !in offer) offer += step.machine
            return false
        }

        val wol = state.actions?.get(step.action) == "wol"
        tried = true
        val record = operations.record(
            client = helper,
            intent = OperationIntent(kind = OpKind.RUN, actionId = step.action),
            subjectName = step.action,
            initiator = fleet.deviceName,
            wakeTarget = machine.id.takeUnless { wol },
        )
        progress("Asking $name to wake ${machine.name}.")
        operations.send(helper, record.id) { id -> helper.run(step.action, InterruptRequest(), id) }
        // A reply still pending when the walk stops watching, or one that was lost, is read once
        // more on a helper that keeps records before anything is concluded from it.
        operations.get(record.id)?.takeIf { !it.isOver && it.trackable }?.let {
            operations.readOnce(helper, record.id)
        }
        val current = operations.get(record.id) ?: return false

        return when {
            // Only `ran` says the helper did what it was asked. Anything else it finished with, a
            // skip included, sent nothing worth waiting for.
            current.outcome == Outcome.SUCCEEDED && current.opAction == OpAction.RAN -> {
                progress("$name ran \"${step.action}\". Waiting for ${machine.name} to answer.")
                if (awaitReadiness()) {
                    true
                } else {
                    lines += "$name ran \"${step.action}\", and ${machine.name} did not answer."
                    false
                }
            }

            current.isOver -> {
                lines += "$name: ${current.summary}"
                false
            }

            wol -> {
                lines += "$name may have run its wol action \"${step.action}\"; its result is not known."
                false
            }

            else -> {
                // A wake hold now: the record carries the target and is unresolved. It is followed
                // in the background, and its being read finished is what releases it.
                operations.follow(helper, record.id)
                lines += "$name may still be running \"${step.action}\" (operation ${record.id}). No other " +
                    "command helper is run for ${machine.name} until that is known."
                false
            }
        }
    }

    /** Whether the target answered within the readiness window of a send that just happened. */
    private suspend fun awaitReadiness(): Boolean {
        val sentAt = clock()
        for (start in readinessSchedule(sentAnything = true)) {
            val wait = sentAt + start - clock()
            if (wait > 0) sleep(wait)
            if (poll()) return true
        }
        return false
    }

    /** One skipped entry in a sentence. */
    private fun describe(skip: WakeSkip): String {
        val name = skip.machine?.let { id -> fleet.machines.firstOrNull { it.id == id }?.name ?: id }
        return when (skip.reason) {
            WakeSkip.Reason.NO_MAC -> "${machine.name} has no hardware address to wake it by."
            WakeSkip.Reason.AMBIGUOUS -> "This device's address matches more than one site, so it cannot " +
                "tell whether it is on ${machine.name}'s network."
            WakeSkip.Reason.UNPLACED -> "${machine.name} names no site and no network prefix, so this device " +
                "cannot tell whether it is on its network."
            WakeSkip.Reason.ELSEWHERE -> "This device is not on ${machine.name}'s network, and a magic packet is " +
                "a LAN broadcast that a tunnel cannot carry."
            WakeSkip.Reason.MISSING -> "$name is not a machine in this setup."
            WakeSkip.Reason.SELF -> "${machine.name} is listed as its own helper, and a machine that is asleep " +
                "cannot send its own packet."
            WakeSkip.Reason.DUPLICATE -> "$name and \"${skip.action}\" are listed twice; it is asked once."
            WakeSkip.Reason.ASLEEP -> "$name is asleep or out of reach."
            WakeSkip.Reason.UNUSABLE -> "$name cannot be asked from this device."
            WakeSkip.Reason.NO_ACTION -> "$name has no action \"${skip.action}\"."
            WakeSkip.Reason.HELD -> if (WakeHold(skip.machine.orEmpty(), skip.action.orEmpty()) in passed) {
                "$name was not asked again: its \"${skip.action}\" was run to wake ${machine.name} earlier and may " +
                    "still be going, so only the other helpers were asked."
            } else {
                "$name was not asked: a wake command for ${machine.name} may still be running, and only a declared " +
                    "wol action runs until that is known."
            }
            WakeSkip.Reason.NONE -> "No wake helper is configured for ${machine.name}."
        }
    }
}

/** What to say when a person asks for the other helpers and there is none to ask. */
internal fun noOtherHelper(targetName: String): String =
    "No other helper could be asked to wake $targetName. The helper command run for it earlier may still be going, so " +
        "it is not run again, and its hold stands."

/** What to say when a person asks to run a held command again while another helper could be asked instead. */
internal fun otherHelperFirst(targetName: String): String =
    "Another helper could be asked to wake $targetName, so the command held for it is not run again. Trying the other " +
        "helpers releases its hold, and the next wake asks every helper."

/** What running the held commands of [helpers] again is called where a person chooses it. */
internal fun runAgainLabel(helpers: List<String>): String =
    helpers.singleOrNull()?.let { "Run $it's command again" } ?: "Run the held commands again"

/** What a person is told before running the held commands again: that a run may still be going, and this can make it two. */
internal fun runAgainWarning(helpers: List<String>, targetName: String): String =
    helpers.singleOrNull()?.let {
        "$it's command to wake $targetName has not reported how it ended, so it may still be running, and no other " +
            "helper could be asked instead. Running it again can run it a second time beside the first."
    } ?: ("The commands run to wake $targetName have not reported how they ended, so they may still be running, and " +
        "no other helper could be asked instead. Running them again can run each a second time beside the first.")
