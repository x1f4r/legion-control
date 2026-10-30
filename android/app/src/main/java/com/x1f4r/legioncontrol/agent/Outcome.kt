package com.x1f4r.legioncontrol.agent

/**
 * How a change ended, or that it has not yet: the one field this app decides on (contract section
 * 5.6).
 *
 * An agent from 3.1.0 on states it on every mutating reply, every record and every summary. Older
 * agents do not, and before this the three clients each guessed it from `ok`, `action` and
 * `reasonCode`, and guessed differently: a refusal with a code was unknown on one platform and a
 * failure on the next, and an action none of them knew was a success, unknown and a failure. This
 * app took every unknown action for a failure, and offered to send it again.
 *
 * [reduce] is the one function every client ports line for line, and `contract/vectors/outcome.json`
 * is what proves the copies agree. It never invents optimism: when the evidence does not say, the
 * answer is [UNKNOWN], and nothing [UNKNOWN] is ever sent again on this app's own say.
 */
enum class Outcome(val wire: String) {
    /** Finished, and the requested state holds, including "it already did". */
    SUCCEEDED("succeeded"),

    /** Finished, not in effect, and the state is known. A new attempt is a new `--op`. */
    FAILED("failed"),

    /** Finished without acting, by design. Offered again later, with a new id. */
    DEFERRED("deferred"),

    /** The agent still owns the work. Followed with `op ID --wait 20`, and nothing else changes it. */
    PENDING("pending"),

    /** No evidence either way. Never sent again; the same id is only ever read. */
    UNKNOWN("unknown"),
    ;

    /** Whether the agent is done with it. Polling stops at every outcome but [PENDING]. */
    val isTerminal: Boolean get() = this != PENDING

    companion object {
        /** A stated outcome, exactly as the contract spells it, or null for anything else. */
        fun fromWire(value: String?): Outcome? = entries.firstOrNull { it.wire == value }

        /**
         * The reducer of section 5.6, line for line with `reduceOutcome` in
         * `contract/tools/outcome.mjs`.
         *
         * A reply is reduced from its own fields. A record is reduced from its result, in the light
         * of its state and of the outcome stated on the record or, failing that, on the result: see
         * [OpRecord.effectiveOutcome]. Absent and null are the same thing to every rule here.
         */
        fun reduce(
            outcome: String? = null,
            state: String? = null,
            ok: Boolean? = null,
            action: String? = null,
            reasonCode: String? = null,
        ): Outcome {
            // 1. A stated outcome wins, but only one this version knows. A value from a later
            //    contract is not a reason to believe anything.
            if (outcome != null) return fromWire(outcome) ?: UNKNOWN

            // 2. Work the agent still owns is pending whatever else the object says; a running
            //    record may already carry the action it is aiming at.
            if (state == "queued" || state == "running") return PENDING

            // 3. Otherwise the action decides, with the reason code where one action covers two
            //    different endings.
            if (action == null) {
                return when (ok) {
                    true -> if (reasonCode == null) SUCCEEDED else UNKNOWN
                    false -> when (reasonCode) {
                        "operation-in-progress", "lock-held" -> DEFERRED
                        "timed-out", "interrupted", "internal" -> UNKNOWN
                        else -> FAILED
                    }
                    null -> UNKNOWN
                }
            }
            return when (action) {
                in PENDING_ACTIONS -> PENDING
                in SUCCESS_ACTIONS -> SUCCEEDED
                in DEFERRED_ACTIONS -> DEFERRED
                // A cancelled request was itself deferred, and a cancel that worked succeeded. One the
                // agent could not safely put in place of the request before it may or may not have.
                "cancelled" -> when (reasonCode) {
                    null -> SUCCEEDED
                    "cancelled" -> DEFERRED
                    "internal" -> UNKNOWN
                    else -> FAILED
                }
                // The old version is back and running: success only when nothing went wrong.
                "rolled-back" -> if (reasonCode == null) SUCCEEDED else FAILED
                "interrupted" -> UNKNOWN
                "failed" -> if (reasonCode in UNKNOWN_FAILURES) UNKNOWN else FAILED
                else -> UNKNOWN
            }
        }

        private val PENDING_ACTIONS = setOf("accepted", "already-running", "queued", "rebooting", "sleeping")

        private val SUCCESS_ACTIONS = setOf(
            "updated", "restarted", "ran", "rebooted", "slept", "armed", "installed",
            "cycled", "noop", "checked", "skipped", "stored", "replaced",
        )

        private val DEFERRED_ACTIONS = setOf("deferred", "conflict", "expired")

        /** Codes after which nobody knows whether the work took effect. */
        private val UNKNOWN_FAILURES = setOf("timed-out", "interrupted")
    }
}
