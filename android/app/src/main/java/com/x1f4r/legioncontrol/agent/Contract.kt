package com.x1f4r.legioncontrol.agent

import java.util.Locale
import java.util.UUID

/**
 * The agent contract, version 3, as this app speaks it.
 *
 * Everything in this file mirrors the shared contract definitions under `contract/`, written in
 * Kotlin so the compiler can hold the app to them. Nothing here is invented: a name that
 * does not appear in the contract does not appear here either, and a value this app cannot recognise
 * is carried through as text rather than flattened into the nearest thing it does know.
 */
object Contract {
    /** What this build of the app needs from an agent before it will use any v3 feature. */
    const val REQUIRED = 3

    /**
     * The agent version this app carries a signed bundle of, and offers to install.
     *
     * Kept beside the required contract because the two move together: an app that needs contract 3
     * has to be able to put a contract 3 agent on a machine that has an older one.
     */
    const val BUNDLED_AGENT_VERSION = "3.0.0"

    /**
     * The argv grammar from rule 4.
     *
     * Every token this app sends and every value it accepts back matches it: no spaces, no quotes,
     * no dollar, no semicolon. Every argv item is also quoted for the shell on the far side, and the
     * two rules are not alternatives. Quoting is what makes a path with a space work; this grammar
     * is what makes sure an id out of a document can never become a second command whatever the
     * quoting does.
     */
    private val TOKEN = Regex("^[A-Za-z0-9][A-Za-z0-9._:@/\\\\~=+-]*$")

    /** A lowercase UUIDv4, or any lowercase id of 8 to 64 characters from [a-z0-9-]. */
    private val OP_ID = Regex("^[a-z0-9-]{8,64}$")

    /** `30m`, `4h`, `2d`. */
    private val DURATION = Regex("^[0-9]+(m|h|d)$")

    /** `02:00`, in the machine's local time. */
    private val CLOCK = Regex("^(?:[01][0-9]|2[0-3]):[0-5][0-9]$")

    fun isToken(value: String?): Boolean = value != null && TOKEN.matches(value)

    fun isOpId(value: String?): Boolean = value != null && OP_ID.matches(value)

    fun isDuration(value: String?): Boolean = value != null && DURATION.matches(value)

    fun isClockTime(value: String?): Boolean = value != null && CLOCK.matches(value)

    /** A fresh operation id. Lowercase, because the grammar says so and a UUID is not by default. */
    fun newOpId(): String = UUID.randomUUID().toString().lowercase(Locale.ROOT)

    /**
     * Checks a value this app is about to send as a token, or explains why it cannot be.
     *
     * Called on every id that came out of a document or off a machine, before it goes anywhere near
     * a command line. The quoting layer would make a hostile id harmless anyway; this is the second
     * lock on the same door, and it is the one that also catches an id that is merely wrong.
     */
    fun requireToken(value: String, what: String): String {
        require(TOKEN.matches(value)) {
            "$what \"$value\" is not a valid identifier. Letters, digits and . _ : @ / \\ ~ = + - " +
                "only, starting with a letter or a digit."
        }
        return value
    }

    /** A duration in seconds as the `DUR` form the agent takes, rounded up to whole minutes. */
    fun durationOf(seconds: Long): String = when {
        seconds <= 0L -> "1m"
        seconds % 86_400L == 0L -> "${seconds / 86_400L}d"
        seconds % 3_600L == 0L -> "${seconds / 3_600L}h"
        else -> "${((seconds + 59L) / 60L)}m"
    }
}

/**
 * Why the agent did not do the happy-path thing, as a closed enum.
 *
 * This is section 2.2 verbatim. Clients render by code and fall back to `message`; a code this build
 * has never heard of is treated as a failure and the agent's own sentence is shown, which is the one
 * behaviour that stays correct as the enum grows.
 */
enum class ReasonCode(val wire: String, val sentence: String) {
    BUSY("busy", "the machine was working"),
    BUSY_UNKNOWN("busy-unknown", "the agent could not tell whether the machine was working"),
    POLICY_OFF("policy-off", "the schedule is off for this service"),
    POLICY_PAUSED("policy-paused", "updates are paused on this service"),
    OUTSIDE_WINDOW("outside-window", "it is outside this service's maintenance window"),
    OPERATION_IN_PROGRESS("operation-in-progress", "another change was already running"),
    LOCK_HELD("lock-held", "another change holds the machine's lock"),
    NO_UPDATE("no-update", "there is nothing newer to install"),
    LATEST_UNKNOWN("latest-unknown", "the newest version could not be looked up"),
    NOT_INSTALLED("not-installed", "it is not installed on this system"),
    NOT_RUNNING("not-running", "it is not running"),
    APP_CLOSED("app-closed", "the app it belongs to is not open"),
    APPLY_ATTEMPTS_EXHAUSTED("apply-attempts-exhausted", "the install was retried and kept failing"),
    APPLY_FAILED("apply-failed", "the install failed"),
    POSTCONDITION_FAILED(
        "postcondition-failed",
        "the command finished without leaving the version it promised",
    ),
    ROLLED_BACK("rolled-back", "the install failed and the previous version was put back"),
    NOT_CONFIGURED("not-configured", "the agent has nothing configured to do this"),
    UNSUPPORTED_PLATFORM("unsupported-platform", "this system cannot do that"),
    UNKNOWN_SERVICE("unknown-service", "that service is not configured on this system"),
    UNKNOWN_TARGET("unknown-target", "that boot target is not configured on this system"),
    UNKNOWN_ACTION("unknown-action", "that action is not configured on this system"),
    BAD_ARGUMENT("bad-argument", "the agent refused one of the arguments this app sent"),
    CONFIG_INVALID("config-invalid", "the agent's own configuration cannot be read"),
    CONFIG_MISSING("config-missing", "the agent has no configuration"),
    INTERRUPTED("interrupted", "it was interrupted before it finished"),
    EXPIRED("expired", "it waited for an idle moment and gave up"),
    CANCELLED("cancelled", "it was cancelled"),
    ALREADY_RUNNING("already-running", "that same request is already running"),
    ALREADY_ON_TARGET("already-on-target", "it is already in that state"),
    STALE_REVISION("stale-revision", "that copy of the setup is older than the one already there"),
    CONTROLLER_CONFLICT("controller-conflict", "that setup came from a different controller"),
    SIGNATURE_INVALID("signature-invalid", "the signature on that artefact did not check out"),
    RESTRICTED("restricted", "the key this app uses is not allowed to do that"),
    TIMED_OUT("timed-out", "it took too long"),
    INTERNAL("internal", "the agent hit a problem of its own"),
    ;

    /** Whether this is a policy decision rather than a fault. It reads differently on screen. */
    val isPolicy: Boolean
        get() = this == POLICY_OFF || this == POLICY_PAUSED || this == OUTSIDE_WINDOW

    /** Whether offering to force past it is meaningful. Force overrides the busy gate and nothing else. */
    val isForceable: Boolean get() = this == BUSY || this == BUSY_UNKNOWN

    companion object {
        fun fromWire(value: String?): ReasonCode? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }

        /**
         * The sentence for a code, known or not.
         *
         * An unknown code is rendered as itself with the hyphens opened out, and the caller shows the
         * agent's `message` beside it. Guessing at a meaning would be worse than saying the word.
         */
        fun describe(value: String?): String? {
            val trimmed = value?.trim()?.takeIf { it.isNotEmpty() } ?: return null
            return fromWire(trimmed)?.sentence ?: trimmed.replace('-', ' ')
        }
    }
}

/** `update | restart | boot | sleep | run | cycle | self-update`. */
enum class OpKind(val wire: String, val label: String) {
    UPDATE("update", "Update"),
    RESTART("restart", "Restart"),
    BOOT("boot", "Boot"),
    SLEEP("sleep", "Sleep"),
    RUN("run", "Action"),
    CYCLE("cycle", "Maintenance cycle"),
    SELF_UPDATE("self-update", "Agent install"),
    ;

    /**
     * Whether an outcome can be read off the machine afterwards.
     *
     * A boot and a sleep can: the system that answers next, and whether it answers at all. An update
     * can: the installed version. A restart cannot be told from a service that was already running,
     * which is exactly why an interrupted restart stays unknown until the agent's own record says
     * otherwise rather than being guessed from uptime.
     */
    val isObservable: Boolean get() = this == BOOT || this == UPDATE || this == SLEEP

    companion object {
        fun fromWire(value: String?): OpKind? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }
    }
}

/** `scheduled | manual | force | queued`. */
enum class OpMode(val wire: String) {
    SCHEDULED("scheduled"),
    MANUAL("manual"),
    FORCE("force"),
    QUEUED("queued"),
    ;

    companion object {
        fun fromWire(value: String?): OpMode? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }
    }
}

/** `queued | running | finished`. The whole of an operation's life, as the agent sees it. */
enum class OpState(val wire: String) {
    QUEUED("queued"),
    RUNNING("running"),
    FINISHED("finished"),
    ;

    companion object {
        fun fromWire(value: String?): OpState? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }
    }
}

/**
 * The union of the per-kind phase lists, which is what a decoder has to accept.
 *
 * Rendered as a short sentence so a running update can say what it is doing rather than showing the
 * user a word out of a state machine.
 */
enum class OpPhase(val wire: String, val sentence: String) {
    RESOLVING("resolving", "working out what to install"),
    RECOVERING("recovering", "finishing an interrupted change first"),
    CHECKING_BUSY("checking-busy", "checking whether the machine is working"),
    WARMING("warming", "warming the package cache"),
    LOCKING("locking", "taking the machine's lock"),
    STOPPING("stopping", "stopping the service"),
    INSTALLING("installing", "installing"),
    VERIFYING("verifying", "verifying"),
    STARTING("starting", "starting the service"),
    HEALTH("health", "waiting for it to answer"),
    ROLLING_BACK("rolling-back", "putting the previous version back"),
    RESTARTING("restarting", "restarting"),
    ARMING("arming", "arming the next boot"),
    REBOOTING("rebooting", "rebooting"),
    PREPARING("preparing", "preparing to suspend"),
    SUSPENDING("suspending", "suspending"),
    RUNNING("running", "running"),
    QUEUED("queued", "waiting for an idle moment"),
    SERVICES("services", "working through the services"),
    STAGING("staging", "staging the new agent"),
    SELF_TEST("self-test", "testing the new agent"),
    SWAPPING("swapping", "swapping the agent into place"),
    DONE("done", "done"),
    ;

    companion object {
        fun fromWire(value: String?): OpPhase? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }

        /** The phase in words, whether or not this build knows it. */
        fun describe(value: String?): String? {
            val trimmed = value?.trim()?.takeIf { it.isNotEmpty() } ?: return null
            return fromWire(trimmed)?.sentence ?: trimmed.replace('-', ' ')
        }
    }
}

/**
 * The union of the per-kind result actions.
 *
 * [isSettled] is the field that matters: it is false for the two that mean "this is not over", and
 * the app must not draw either of them as an ending.
 */
enum class OpAction(val wire: String) {
    ACCEPTED("accepted"),
    ALREADY_RUNNING("already-running"),
    ARMED("armed"),
    CANCELLED("cancelled"),
    CHECKED("checked"),
    CONFLICT("conflict"),
    CYCLED("cycled"),
    DEFERRED("deferred"),
    EXPIRED("expired"),
    FAILED("failed"),
    INSTALLED("installed"),
    INTERRUPTED("interrupted"),
    NOOP("noop"),
    QUEUED("queued"),
    RAN("ran"),
    REBOOTED("rebooted"),
    REBOOTING("rebooting"),
    RESTARTED("restarted"),
    ROLLED_BACK("rolled-back"),
    SKIPPED("skipped"),
    SLEPT("slept"),
    SLEEPING("sleeping"),
    UPDATED("updated"),
    ;

    /**
     * Whether this action is the end of the story.
     *
     * `accepted` and `queued` are not: the work is still to come. `rebooting` and `sleeping` are not
     * either, and that is the correction that matters most here: the machine was told, and being
     * told is not the same as having done it. Only `rebooted` and `slept`, or an observation of the
     * machine in that state, close a boot or a sleep.
     */
    val isSettled: Boolean
        get() = this !in setOf(ACCEPTED, QUEUED, REBOOTING, SLEEPING, ALREADY_RUNNING)

    /** Whether it ended well. Only meaningful once [isSettled]. */
    val isSuccess: Boolean
        get() = this in setOf(UPDATED, RESTARTED, RAN, REBOOTED, SLEPT, CYCLED, INSTALLED, ARMED, CHECKED)

    companion object {
        fun fromWire(value: String?): OpAction? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }
    }
}

/** `mac | desktop | phone | cli | legacy`: which kind of client wrote the copy a machine holds. */
enum class ControllerSource(val wire: String, val label: String) {
    MAC("mac", "the Mac"),
    DESKTOP("desktop", "a desktop"),
    PHONE("phone", "a phone"),
    CLI("cli", "the command line"),
    LEGACY("legacy", "an older client"),
    ;

    companion object {
        fun fromWire(value: String?): ControllerSource? {
            val key = value?.trim()?.lowercase(Locale.ROOT) ?: return null
            return entries.firstOrNull { it.wire == key }
        }

        fun describe(value: String?): String? {
            val trimmed = value?.trim()?.takeIf { it.isNotEmpty() } ?: return null
            return fromWire(trimmed)?.label ?: trimmed
        }
    }
}
