package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.LanPrefix
import com.x1f4r.legioncontrol.net.LocalAddress
import com.x1f4r.legioncontrol.net.onNetwork
import com.x1f4r.legioncontrol.net.placement

/**
 * How to wake one machine from where this device is standing (contract section 10.2).
 *
 * There is no longer one house and no longer one machine that is always on. A magic packet is a
 * link-local broadcast, so waking a machine in another building is not a matter of trying harder: it
 * needs somebody already on that network to send it. That somebody is a helper, helpers can
 * themselves be asleep, and so there is an order and there are failures worth explaining rather
 * than retrying.
 *
 * Nothing here wakes a helper. The user asked not to be pushed towards leaving machines on, and a
 * cascade that wakes the tower to wake the laptop turns an always-off tower into an always-on one by
 * accident. A helper that is asleep and has a wake of its own is offered as a separate press,
 * [offerWakeFirst], and nothing else is.
 *
 * A port of `planWake` in `contract/tools/wake-plan.mjs`, held to `contract/vectors/wake.json`. The
 * three clients used to disagree on the broadcast, the ports, the packets and how long to wait;
 * this is the one plan they all make now.
 */
data class WakePlan(
    /** Every site whose prefixes hold one of this device's addresses, in document order. */
    val matching: List<String>,
    /** The one site this device is on, or null when it is on none or cannot tell which. */
    val effective: String?,
    /** What to do, in order. */
    val steps: List<WakeStep>,
    /** Why the direct step and each helper entry that is not a step are not, in the same order. */
    val skipped: List<WakeSkip>,
    /** Helpers skipped as asleep that have a wake of their own: "wake it first", a separate press. */
    val offerWakeFirst: List<String>,
)

/** One step of a wake. */
sealed interface WakeStep {
    /**
     * Send the magic packet from this device: [repeats] packets to every one of [broadcast] on every
     * one of [ports], from a socket bound to the network [placing] is on.
     */
    data class Direct(
        val broadcast: List<String>,
        val ports: List<Int>,
        val repeats: Int,
        /** The address that placed the machine on this device's network, or null. */
        val placing: LocalAddress?,
    ) : WakeStep

    /**
     * Ask [machine] to run [action]. [readFirst] when this session has not read the helper yet, or
     * its last read ended in a way that settles nothing: it is read once just before its step.
     */
    data class Helper(val machine: String, val action: String, val readFirst: Boolean) : WakeStep
}

/** Why the direct step, or one helper entry, is not a step. */
data class WakeSkip(
    val kind: Kind,
    val machine: String?,
    val action: String?,
    val reason: Reason,
) {
    enum class Kind(val wire: String) { DIRECT("direct"), HELPER("helper"), HELPERS("helpers") }

    enum class Reason(val wire: String) {
        /** The machine has no hardware address to wake. */
        NO_MAC("no-mac"),

        /** Its site is one of several this device's address matches. */
        AMBIGUOUS("ambiguous"),

        /** It names no site and has no valid `wake.lanPrefix`. */
        UNPLACED("unplaced"),

        /** This device is not on its network. */
        ELSEWHERE("elsewhere"),
        MISSING("missing"),
        SELF("self"),
        DUPLICATE("duplicate"),
        ASLEEP("asleep"),
        UNUSABLE("unusable"),
        NO_ACTION("no-action"),

        /** A wake hold for the target exists and this action is not a declared `wol`. */
        HELD("held"),

        /** There is no direct step and no helper at all. */
        NONE("none"),
    }
}

/**
 * What the last status read of a helper this session said (contract section 10.2, and the
 * helper-state table of the S5.4 mapping).
 *
 * [actions] maps each action id the status listed to its kind, when the status listed them; a
 * status that lists none says nothing about which actions there are.
 */
data class HelperState(val state: Kind, val actions: Map<String, String?>? = null) {
    enum class Kind(val wire: String) {
        /** Its last status read answered. */
        READY("ready"),

        /** That read failed with `noRoute` or `hostUnreachable`. */
        ASLEEP("asleep"),

        /** That read failed in a way no wake can cure: the key, the host key, the agent, the request. */
        UNUSABLE("unusable"),

        /** No system of the helper is approved on this device. */
        NOT_DRIVEN("not-driven"),

        /** Not read this session, or the read ended some other way. Read once before its step. */
        UNKNOWN("unknown"),
    }

    companion object {
        val UNKNOWN = HelperState(Kind.UNKNOWN)

        /**
         * A helper's state from this session's reads of it: not driven when no system of it can be
         * run from this device, whatever was read; unknown before the first read; ready with the
         * kinds of the actions it listed after an answer; and after a failure, whatever [after]
         * makes of it. An answer that refused, `ok: false`, is not ready and fails in no way a
         * wake can be told about, so it is unknown and read again before its step.
         */
        fun of(driven: Boolean, read: Boolean, answer: AgentStatus?, failure: FailureKind?): HelperState = when {
            !driven -> HelperState(Kind.NOT_DRIVEN)
            !read -> UNKNOWN
            answer != null && answer.ok != false -> HelperState(
                Kind.READY,
                answer.actions?.mapNotNull { action -> action.id?.let { it to action.kind } }?.toMap(),
            )

            else -> after(failure)
        }

        /** The state a failed status read leaves, by the kind of failure. */
        fun after(failure: FailureKind?): HelperState = HelperState(
            when (failure) {
                FailureKind.NO_ROUTE, FailureKind.HOST_UNREACHABLE -> Kind.ASLEEP
                FailureKind.AUTHENTICATION_FAILED, FailureKind.HOST_KEY_CHANGED, FailureKind.HOST_KEY_UNKNOWN,
                FailureKind.INTERPRETER_MISSING, FailureKind.AGENT_MISSING, FailureKind.BAD_REQUEST,
                -> Kind.UNUSABLE

                else -> Kind.UNKNOWN
            },
        )
    }
}

/** One wake hold for the target: the helper and the action whose result is not known. */
data class WakeHold(val helper: String, val action: String)

/** The limited broadcast address, the last resort of direct wake. */
const val LIMITED_BROADCAST = "255.255.255.255"

/** The ports direct wake sends to when the wake block names none it can use. */
val DEFAULT_WAKE_PORTS = listOf(9, 7)

/** Magic packets per broadcast address and port. Wi-Fi drops UDP without telling anyone. */
const val WAKE_REPEATS = 3

/**
 * The plan for waking [target] from this device.
 *
 * [machines] is every machine of the setup, so a helper can be found and its own wake seen;
 * [addresses] and [currentSite] say where this device is; [helpers] is each helper's last status
 * read this session, a missing one being [HelperState.UNKNOWN]; [holds] are the wake holds this
 * device recorded for [target].
 */
fun planWake(
    target: Machine,
    machines: List<Machine>,
    sites: List<SiteConfig>,
    addresses: List<LocalAddress>,
    currentSite: String?,
    helpers: Map<String, HelperState>,
    holds: List<WakeHold>,
): WakePlan {
    val wake = target.wake
    val place = placement(sites, addresses, currentSite)
    val steps = mutableListOf<WakeStep>()
    val skipped = mutableListOf<WakeSkip>()
    val offerWakeFirst = mutableListOf<String>()

    if (wake == null || wake.mac.isBlank()) {
        skipped += WakeSkip(WakeSkip.Kind.DIRECT, null, null, WakeSkip.Reason.NO_MAC)
    } else {
        val network = onNetwork(target.siteId, wake.lanPrefix, sites, addresses, currentSite)
        if (!network.on) {
            skipped += WakeSkip(WakeSkip.Kind.DIRECT, null, null, WakeSkip.Reason.entries.single { it.wire == network.reason })
        } else {
            val site = target.siteId?.let { id -> sites.firstOrNull { it.id == id } }
            steps += WakeStep.Direct(broadcastsFor(wake, site, network.placing), portsFor(wake), WAKE_REPEATS, network.placing)
        }
    }

    val list = wake?.helpers.orEmpty()
    val earlier = mutableListOf<WakeHelper>()
    for (entry in list) {
        val state = helpers[entry.machineId] ?: HelperState.UNKNOWN
        val reason = helperSkip(target.id, machines, entry, earlier, state, holds.isNotEmpty())
        earlier += entry
        if (reason == null) {
            steps += WakeStep.Helper(entry.machineId, entry.actionId, readFirst = state.state == HelperState.Kind.UNKNOWN)
            continue
        }
        skipped += WakeSkip(WakeSkip.Kind.HELPER, entry.machineId, entry.actionId, reason)
        val ownWake = machines.firstOrNull { it.id == entry.machineId }?.wake != null
        if (reason == WakeSkip.Reason.ASLEEP && ownWake && entry.machineId !in offerWakeFirst) {
            offerWakeFirst += entry.machineId
        }
    }
    if (list.isEmpty() && steps.none { it is WakeStep.Direct }) {
        skipped += WakeSkip(WakeSkip.Kind.HELPERS, null, null, WakeSkip.Reason.NONE)
    }

    return WakePlan(place.matching, place.effective, steps, skipped, offerWakeFirst)
}

/**
 * Why helper [entry] is skipped, checked in this order, or null when it is a step: `missing`,
 * `self`, `duplicate`, then from the helper's last status read `asleep`, `unusable`, `no-action`,
 * and `held`.
 *
 * An unknown helper is read once just before its step, so only what needs no status can skip it,
 * and a hold, which no unread helper can be shown to be exempt from. The walk asks again at each
 * step with the helper's state and the holds as they are then, which is how a hold recorded part
 * way through a wake stops the command helpers after it.
 */
fun helperSkip(
    targetId: String,
    machines: List<Machine>,
    entry: WakeHelper,
    earlier: List<WakeHelper>,
    state: HelperState,
    held: Boolean,
): WakeSkip.Reason? {
    if (machines.none { it.id == entry.machineId }) return WakeSkip.Reason.MISSING
    if (entry.machineId == targetId) return WakeSkip.Reason.SELF
    if (earlier.any { it == entry }) return WakeSkip.Reason.DUPLICATE
    when (state.state) {
        HelperState.Kind.ASLEEP -> return WakeSkip.Reason.ASLEEP
        HelperState.Kind.UNUSABLE, HelperState.Kind.NOT_DRIVEN -> return WakeSkip.Reason.UNUSABLE
        else -> Unit
    }
    val actions = state.actions.takeIf { state.state == HelperState.Kind.READY }
    if (actions != null && entry.actionId !in actions) return WakeSkip.Reason.NO_ACTION
    // While any hold for the target exists, only a declared wol action runs, the held helper's
    // included: a command whose first run may still be going must not get a second one elsewhere.
    if (held && actions?.get(entry.actionId) != "wol") return WakeSkip.Reason.HELD
    return null
}

/**
 * What a person chose to do about the wake holds on a target (contract section 10.2). Only a person
 * ever chooses anything but [STAND]: a hold is what keeps a command from running twice, and every
 * other release is evidence.
 */
enum class HoldChoice {
    /** The holds stand, and the wake walks around them. */
    STAND,

    /**
     * Ask the helpers a hold keeps back other than the held ones. The held commands are not run
     * again in that walk, but their holds are released, so the next wake asks every helper.
     */
    OTHER_HELPERS,

    /**
     * With no other helper to ask, run the held commands again, knowing the first run may still be
     * going. A helper that keeps no records would otherwise hold until the target answers.
     */
    RUN_AGAIN,
}

/** Whether a wake hold is what keeps a helper from running. */
val WakePlan.isHeld: Boolean get() = skipped.any { it.reason == WakeSkip.Reason.HELD }

/**
 * This plan without the helper steps [holds] are on: what a person asking for the other helpers
 * walks. Each step taken out is skipped as held. Releasing a hold is not a reason to run its own
 * command a second time while the first run may still be going.
 */
fun WakePlan.without(holds: List<WakeHold>): WakePlan {
    if (holds.isEmpty()) return this
    val passed = steps.filterIsInstance<WakeStep.Helper>().filter { WakeHold(it.machine, it.action) in holds }
    return copy(
        steps = steps.filterNot { it in passed },
        skipped = skipped + passed.map { WakeSkip(WakeSkip.Kind.HELPER, it.machine, it.action, WakeSkip.Reason.HELD) },
    )
}

/**
 * What trying the other helpers walks: [unheld], the plan as if nothing were held, without the
 * helper steps of [holds]. Null unless that asks a helper [held], the plan as it stands, skips for a
 * hold. With the held helper the only one, going past its hold would ask nobody new, and the next
 * ordinary press would run the held command a second time.
 */
fun otherHelpers(held: WakePlan, unheld: WakePlan, holds: List<WakeHold>): WakePlan? {
    if (holds.isEmpty()) return null
    val others = unheld.without(holds)
    val heldBack = held.skipped.filter { it.reason == WakeSkip.Reason.HELD }
        .map { WakeHold(it.machine.orEmpty(), it.action.orEmpty()) }
        .toSet()
    return others.takeIf { plan ->
        plan.steps.any { it is WakeStep.Helper && WakeHold(it.machine, it.action) in heldBack }
    }
}

/**
 * What running the held commands again walks: [unheld], the plan as if nothing were held. Null
 * unless [held] skips a helper for a hold and no other helper could be asked instead: with one,
 * asking it is the way past, and that frees the held command for the next press without running it
 * twice now.
 */
fun runAgain(held: WakePlan, unheld: WakePlan, holds: List<WakeHold>): WakePlan? =
    unheld.takeIf { held.isHeld && otherHelpers(held, unheld, holds) == null }

/** The helpers whose command this plan runs a second time while the first run may still be going, by id, in plan order. */
fun WakePlan.runsAgain(holds: List<WakeHold>): List<String> =
    steps.filterIsInstance<WakeStep.Helper>().filter { WakeHold(it.machine, it.action) in holds }.map { it.machine }.distinct()

/**
 * Where direct wake sends: the dotted entries of `wake.broadcast`; else those of the machine's site;
 * else the directed broadcast of the placing address when its prefix length is known and at most
 * 30; else 255.255.255.255. A device that guesses the limited broadcast first wakes nothing behind a
 * router that drops it, which is why a site's own broadcast comes before it. Nothing is resolved as
 * a name.
 */
private fun broadcastsFor(wake: WakeTarget, site: SiteConfig?, placing: LocalAddress?): List<String> {
    val listed = wake.broadcasts.filter { LanPrefix.parseIPv4(it) != null }
    if (listed.isNotEmpty()) return listed
    val shared = site?.broadcast.orEmpty().filter { LanPrefix.parseIPv4(it) != null }
    if (shared.isNotEmpty()) return shared
    val address = placing?.value
    val length = placing?.prefixLength
    if (address != null && length != null) {
        LanPrefix.directedBroadcast(address, length)?.let { return listOf(LanPrefix.formatIPv4(it)) }
    }
    return listOf(LIMITED_BROADCAST)
}

/** The ports of a wake block from 1 to 65535, or 9 and 7 when it names none of those. */
private fun portsFor(wake: WakeTarget): List<Int> = wake.ports.filter { it in 1..65535 }.ifEmpty { DEFAULT_WAKE_PORTS }

/** How long after a send the target is polled for. */
const val READINESS_WINDOW_MS = 120_000L

private const val FIRST_POLL_MS = 5_000L
private const val LONGEST_INTERVAL_MS = 20_000L

/**
 * When the readiness polls start, in milliseconds after a send: the first after 5 s, then at
 * intervals growing by 8/5 up to 20 s, for as long as a poll starts within 120 s (contract section
 * 10.2, "The walk"). A step that sent nothing waits for nothing.
 *
 * Only an authenticated answer from the target counts. Sending a packet is not waking a machine.
 */
fun readinessSchedule(sentAnything: Boolean): List<Long> {
    if (!sentAnything) return emptyList()
    val starts = mutableListOf<Long>()
    var wait = FIRST_POLL_MS
    var at = FIRST_POLL_MS
    while (at <= READINESS_WINDOW_MS) {
        starts += at
        wait = minOf(wait * 8 / 5, LONGEST_INTERVAL_MS)
        at += wait
    }
    return starts
}
