package com.x1f4r.legioncontrol.agent

/**
 * Whether a command changes anything, which decides what may be done when it fails.
 *
 * A status that fails on one address is retried on the next, because reading twice is reading once.
 * A restart that fails on one address is only retried when it is known not to have started, and
 * never on the strength of "the connection went away", because that is exactly what a restart looks
 * like from here.
 */
enum class CommandSafety(val wire: String) {
    READ("read"),
    MUTATION("mutation"),
}

/** Why a walk stopped making attempts. */
enum class WalkEnding(val wire: String) {
    /** An attempt produced a reply. */
    REPLY("reply"),

    /** A mutation failed in a way that may have run it, so nothing else is tried. */
    STOPPED("stopped"),

    /** Every attempt worth making was made, and none of them produced a reply. */
    EXHAUSTED("exhausted"),
}

/**
 * The rules for going from one attempt to the next (contract section 15.5, steps 1 and 5 to 7).
 *
 * Attempts are routes in order, and on each route the systems that can be driven. A reply ends the
 * walk. A read goes on after any failure. A mutation goes on only after a failure that is
 * [DispatchStage.NOT_STARTED]: anything else may have run, so it stops at the first such attempt and
 * is reconciled with `op <id>`, never sent again. A route-level failure skips the rest of that route's
 * systems, because they share its connection.
 *
 * One instance per command. [AgentClient] drives it with real attempts, and [simulate] drives it with
 * the outcomes the shared vectors list, so the two cannot disagree about the rule.
 */
class AttemptWalk(private val safety: CommandSafety) {
    private val deadRoutes = mutableSetOf<String>()

    /** Whether [route] has already failed in a way that rules out the rest of its systems. */
    fun skips(route: String): Boolean = route in deadRoutes

    /**
     * Records what one attempt on [route] said, and whether the walk ends there.
     *
     * @return the ending, or null to go on to the next attempt.
     */
    fun after(route: String, reading: AttemptReading): WalkEnding? {
        if (reading.isReply) return WalkEnding.REPLY
        if (safety == CommandSafety.MUTATION && reading.dispatch != DispatchStage.NOT_STARTED) {
            return WalkEnding.STOPPED
        }
        if (reading.routeLevel) deadRoutes += route
        return null
    }

    /** One attempt in a simulated walk: where it would go, and what it would end in. */
    data class Planned(val route: String, val system: String, val reading: AttemptReading)

    /** Which attempts a walk makes, by index, and why it stopped. */
    data class Simulation(val sent: List<Int>, val ending: WalkEnding)

    companion object {
        /** The walk over a list of attempts whose outcomes are already known. */
        fun simulate(safety: CommandSafety, attempts: List<Planned>): Simulation {
            val walk = AttemptWalk(safety)
            val sent = mutableListOf<Int>()
            for ((index, attempt) in attempts.withIndex()) {
                if (walk.skips(attempt.route)) continue
                sent += index
                walk.after(attempt.route, attempt.reading)?.let { return Simulation(sent, it) }
            }
            return Simulation(sent, WalkEnding.EXHAUSTED)
        }
    }
}

/**
 * The `--budget-ms` a status attempt sends, or null when no attempt should be made (section 15.5,
 * step 3).
 *
 * Five seconds stay in hand for ssh and the reply, the agent never gets more than 20 s, and it never
 * gets less than 1 s: a first attempt always goes out, a later one only when there is time to use
 * its answer. Before this every attempt sent 20 s whatever was left, so after two dead routes the
 * working one was cut off by this app's own deadline.
 */
fun statusBudget(remainingMillis: Long, attemptsMade: Int): Long? {
    val budget = minOf(STATUS_BUDGET_CAP_MILLIS, remainingMillis - STATUS_BUDGET_MARGIN_MILLIS)
    if (budget >= STATUS_BUDGET_FLOOR_MILLIS) return budget
    return if (attemptsMade == 0) STATUS_BUDGET_FLOOR_MILLIS else null
}

/** Once an attempt has been made, a walk stops when less than this remains (section 15.5, step 2). */
const val MINIMUM_ATTEMPT_MILLIS = 4_000L

private const val STATUS_BUDGET_CAP_MILLIS = 20_000L
private const val STATUS_BUDGET_MARGIN_MILLIS = 5_000L
private const val STATUS_BUDGET_FLOOR_MILLIS = 1_000L
