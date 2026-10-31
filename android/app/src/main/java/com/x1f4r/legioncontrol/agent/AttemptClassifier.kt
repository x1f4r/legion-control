package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.CommandOutcome
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive

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
     *
     * "Parses" means what `JSON.parse` takes, because that is what the reference and the other
     * clients read the slice with. kotlinx is strict about structure and not about everything else:
     * it reads a bare word where a value goes as a literal of that spelling, so `{"ok":tru}` and
     * `{"a":01}` parse, and it keeps a control character left raw inside a string. Both are held
     * to the JSON rule here, before and after the parse, so that output the other clients call
     * unreadable is not a reply on this one, and a lenient decoder never gets to make a success of it.
     */
    fun jsonObjectSlice(stdout: String): JsonObject? {
        val text = sliceJsonObject(stdout) ?: return null
        if (!scansAsJsonText(text)) return null
        val parsed = try {
            StrictJson.parseToJsonElement(text) as? JsonObject
        } catch (_: Exception) {
            null
        }
        return parsed?.takeIf(::holdsOnlyJsonValues)
    }

    /**
     * What a scan of the raw text can tell that the parse cannot: no control character is left raw
     * inside a string, and nothing is nested deeper than [MAX_NESTING].
     *
     * The first is a rule of JSON text that kotlinx does not keep, and the parsed string cannot show
     * it any more, because an escaped newline and a raw one read back the same. The second is this
     * app's own limit, and the one place it reads differently from the reference: kotlinx reads
     * arrays by recursion and runs out of stack on nesting a far side is free to print, which is an
     * error no catch here would see. No reply the agent writes comes near it.
     *
     * Quotes are told apart the way kotlinx tells them apart, so the depth counted here is the depth
     * the parse would reach.
     */
    private fun scansAsJsonText(text: String): Boolean {
        var depth = 0
        var inString = false
        var escaped = false
        for (char in text) {
            if (inString) {
                when {
                    escaped -> escaped = false
                    char == '\\' -> escaped = true
                    char == '"' -> inString = false
                    char < ' ' -> return false
                }
            } else {
                when (char) {
                    '"' -> inString = true
                    '{', '[' -> if (++depth > MAX_NESTING) return false
                    '}', ']' -> depth--
                }
            }
        }
        return true
    }

    /**
     * Whether every value that is not a string is one JSON has: `true`, `false`, `null`, or a number
     * as RFC 8259 writes it. Recursive, which the depth limit above makes safe.
     */
    private fun holdsOnlyJsonValues(element: JsonElement): Boolean = when (element) {
        is JsonObject -> element.values.all(::holdsOnlyJsonValues)
        is JsonArray -> element.all(::holdsOnlyJsonValues)
        is JsonPrimitive -> element.isString || element.content in JSON_WORDS || JSON_NUMBER.matches(element.content)
    }

    /** Exit statuses only a shell that could not start the command produces. The agent exits 0 or 1. */
    private val INTERPRETER_NOT_STARTED = setOf(126, 127, 9009)

    private val AGENT_MISSING_MARKERS = listOf("cannot find module", "err_module_not_found")

    private val INTERPRETER_MISSING_MARKERS =
        listOf("is not recognized as", "commandnotfoundexception", "cannot find the path specified")

    /**
     * The default configuration, which refuses unquoted keys, comments and trailing commas. The
     * decoders are lenient about all of those; this is not. What it lets through is checked by hand
     * in [jsonObjectSlice].
     *
     * Only the nesting limit is not the default. kotlinx has had one of its own since 1.12, and it is
     * shallower than [MAX_NESTING]. The scan has already held the text to this app's limit, so the
     * parse takes everything up to it rather than drawing a second line of its own.
     */
    @OptIn(ExperimentalSerializationApi::class)
    private val StrictJson = Json { maxNestingDepth = MAX_NESTING }

    /** The only bare words JSON has. */
    private val JSON_WORDS = setOf("true", "false", "null")

    /** A JSON number: no leading zero, no bare point, no sign but a leading minus, no NaN or Infinity. */
    private val JSON_NUMBER = Regex("-?(0|[1-9][0-9]*)(\\.[0-9]+)?([eE][+-]?[0-9]+)?")

    /** Deeper than any reply the agent writes by far, and shallow enough for kotlinx's recursion. */
    private const val MAX_NESTING = 512

    /** ASCII case folding only, so no locale and no other alphabet can make a marker match. */
    private fun String.asciiLowercase(): String =
        String(CharArray(length) { index -> this[index].let { if (it in 'A'..'Z') it + 32 else it } })
}
