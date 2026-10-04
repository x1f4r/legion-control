package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.CommandOutcome
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject

/**
 * Why one attempt did not end in a reply, in the contract's own words (section 15.4).
 *
 * [routeLevel] is the part the walk acts on: after one of these, the rest of the systems on that
 * route are skipped, because they share its connection and would fail the same way.
 */
enum class FailureKind(val wire: String, val routeLevel: Boolean) {
    NO_ROUTE("noRoute", true),
    LAUNCH_FAILED("launchFailed", true),
    HOST_UNREACHABLE("hostUnreachable", true),
    CONNECTION_REFUSED("connectionRefused", true),
    AUTHENTICATION_FAILED("authenticationFailed", true),
    HOST_KEY_CHANGED("hostKeyChanged", true),
    HOST_KEY_UNKNOWN("hostKeyUnknown", true),
    TIMED_OUT("timedOut", true),
    LINK_LOST("linkLost", true),
    INTERPRETER_MISSING("interpreterMissing", false),
    AGENT_MISSING("agentMissing", false),
    UNREADABLE_OUTPUT("unreadableOutput", false),
    BAD_REQUEST("badRequest", false),
}

/**
 * What one attempt says: a reply, or a failure with its kind and how far the command may have got.
 *
 * A reply is [DispatchStage.ACKNOWLEDGED] with no kind. Every failure has a kind.
 */
data class AttemptReading(val kind: FailureKind?, val dispatch: DispatchStage) {
    val isReply: Boolean get() = kind == null

    val routeLevel: Boolean get() = kind?.routeLevel == true

    companion object {
        val REPLY = AttemptReading(null, DispatchStage.ACKNOWLEDGED)

        fun failure(kind: FailureKind, dispatch: DispatchStage) = AttemptReading(kind, dispatch)
    }
}

/**
 * Reads one finished channel, as section 15.4 says every client does.
 *
 * A port of `classify` in `contract/tools/transport.mjs`, for a client with no ssh log. This app
 * talks to ssh through a library in its own process, so everything that happens before the exec
 * request is written is an event of that library and is already translated by the transport, with
 * [DispatchStage.NOT_STARTED]. What reaches this file is what the channel produced after the command
 * was handed over: rows 2 to 10 of the table, with an empty log.
 *
 * The order is the rule. A reply wins over everything, even this app's own watchdog, because it is
 * the agent's own account of what happened. [DispatchStage.NOT_STARTED] is claimed only on the one
 * piece of evidence that proves the command cannot have run: a shell exiting 126, 127 or 9009 with
 * nothing on stdout, which only a shell that could not start the interpreter does. Remote stderr is
 * read only to name a failure for a person. The agent writes raw text to stderr and the launcher
 * passes it through, so a remote program can print any sentence it likes, and a mutation that
 * printed "Cannot find module" may have done half of its work first.
 */
object AttemptClassifier {

    /**
     * The verdict on one channel's output.
     *
     * [replyPossible] is false when stdout was cut short at the transport's cap: a truncated reply
     * is not the agent's account of anything, so the rest of the table decides.
     */
    fun classify(
        timedOut: Boolean,
        exitStatus: Int?,
        stdout: String,
        stderr: String,
        replyPossible: Boolean = true,
    ): AttemptReading {
        if (replyPossible && jsonObjectSlice(stdout) != null) return AttemptReading.REPLY
        if (timedOut) return AttemptReading.failure(FailureKind.TIMED_OUT, DispatchStage.AMBIGUOUS)
        if (exitStatus == null) return AttemptReading.failure(FailureKind.LINK_LOST, DispatchStage.AMBIGUOUS)
        val blank = stdout.all { it == '\t' || it == '\n' || it == '\r' || it == ' ' }
        // OpenSSH exits 255 for its own failures, and a client with a log can tell those apart from
        // the remote side's. There is no such log here, and a remote program that exits 255 may have
        // done anything first, so every 255 that reaches this point is a link that may have run it.
        if (exitStatus == 255) return AttemptReading.failure(FailureKind.LINK_LOST, DispatchStage.AMBIGUOUS)
        if (blank && exitStatus in INTERPRETER_NOT_STARTED) {
            return AttemptReading.failure(FailureKind.INTERPRETER_MISSING, DispatchStage.NOT_STARTED)
        }
        val err = stderr.asciiLowercase()
        if (AGENT_MISSING_MARKERS.any { err.contains(it) }) {
            return AttemptReading.failure(FailureKind.AGENT_MISSING, DispatchStage.AMBIGUOUS)
        }
        if (INTERPRETER_MISSING_MARKERS.any { err.contains(it) }) {
            return AttemptReading.failure(FailureKind.INTERPRETER_MISSING, DispatchStage.AMBIGUOUS)
        }
        return AttemptReading.failure(FailureKind.UNREADABLE_OUTPUT, DispatchStage.AMBIGUOUS)
    }

    /** The same verdict, read off what the transport handed back. */
    fun classify(outcome: CommandOutcome): AttemptReading = classify(
        timedOut = outcome.timedOut,
        exitStatus = outcome.exitStatus,
        stdout = outcome.stdout,
        stderr = outcome.stderr,
        replyPossible = !outcome.stdoutTruncated,
    )

    /**
     * The reply in stdout: from its first `{` to its last `}`, when that parses as a JSON object.
     * Windows PowerShell can put a `#< CLIXML` banner in front of anything a native program prints,
     * and a login script can print a line or two; the slice is what survives both.
     */
    fun jsonObjectSlice(stdout: String): JsonObject? {
        val text = sliceJsonObject(stdout) ?: return null
        return try {
            StrictJson.parseToJsonElement(text) as? JsonObject
        } catch (_: Exception) {
            null
        }
    }

    /** Exit statuses only a shell that could not start the command produces. The agent exits 0 or 1. */
    private val INTERPRETER_NOT_STARTED = setOf(126, 127, 9009)

    private val AGENT_MISSING_MARKERS = listOf("cannot find module", "err_module_not_found")

    private val INTERPRETER_MISSING_MARKERS =
        listOf("is not recognized as", "commandnotfoundexception", "cannot find the path specified")

    /** Plain JSON, the way the reference parses the slice. The decoders are lenient; this is not. */
    private val StrictJson = Json

    /** ASCII case folding only, so no locale and no other alphabet can make a marker match. */
    private fun String.asciiLowercase(): String =
        String(CharArray(length) { index -> this[index].let { if (it in 'A'..'Z') it + 32 else it } })
}
