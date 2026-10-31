package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.SetupJson
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.CommandTransport
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.LanPrefix
import com.x1f4r.legioncontrol.net.untunnelled
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.longOrNull
import java.util.Base64

/**
 * The setup, as the machines carry it.
 *
 * Writing the same document once on the Mac and once on every phone is one time too many, so every
 * agent keeps a copy of the controller config beside its own and serves it to whoever asks. This
 * file is the reading half of that: one reply from `config`, turned into the one thing the phone
 * can do about it.
 *
 * The document itself is never interpreted here. It is handed on as text and validated by the same
 * code that validates a paste, because a document that came off a machine deserves exactly as much
 * suspicion as one that came off a clipboard. What is read here beside the text is its provenance:
 * who wrote it and which revision it is, because a copy that differs is not a copy that is newer.
 */
sealed interface ControllerFetch {
    /**
     * A document, and everything the machine that served it said about where it came from.
     *
     * [text] is the agent's stored bytes when it sent them as `raw`, and otherwise `controller`
     * printed again in the order it arrived, which only an agent before 3.1.0 makes necessary and
     * which the hash check then confirms or refutes. [replyHash] is the reply's own `hash`: what the
     * machine says it holds, and null on a reply that names none, which is never adopted.
     */
    data class Document(
        val text: String,
        val provenance: SetupProvenance,
        val replyHash: String? = provenance.hash,
    ) : ControllerFetch

    /** The agent knows what a setup is and has none. Nothing has been shared with it yet. */
    data object NothingStored : ControllerFetch

    /**
     * A reply with no `controller` key at all, which is an agent that predates the whole idea:
     * `config` is a command it does not know, so what came back is its error or its help.
     */
    data object TooOld : ControllerFetch

    /** Not a reply at all, so the next command shape is worth trying. */
    data object Unreadable : ControllerFetch
}

/**
 * The install layouts tried in turn, until one of them prints a JSON object.
 *
 * These are the paths the installers write and nothing else. A machine in the configuration is
 * reached with the argv the configuration names; this list exists for the one machine that is not
 * in any configuration yet, because it is the machine the configuration is about to come from.
 *
 * Nothing here comes from outside: no user name is pasted into a path, and the home directory is
 * the far side's own `$HOME`, expanded by a shell that is asked for explicitly. A bare `~` used to
 * do that job, and argv elements are literal now (contract section 15.2), so a tilde would be a
 * directory called `~`. Both Windows shapes run the same encoded script, once for an account that
 * logs in to cmd.exe and once for one that logs in to PowerShell, because which of the two it is
 * cannot be known before anything has answered.
 */
fun setupCommands(): List<SetupCommand> = listOf(
    // Installed wrappers retain the runtime selected by the installer, even without Node on PATH.
    SetupCommand(
        RemoteShell.POSIX,
        listOf("/bin/sh", "-c", "exec \"\$HOME/.legion-control/bin/legionctl\" \"\$@\"", "legionctl", "config"),
    ),
    SetupCommand(RemoteShell.CMD, WINDOWS_SETUP_ARGV),
    SetupCommand(RemoteShell.POWERSHELL, WINDOWS_SETUP_ARGV),
    SetupCommand(
        RemoteShell.POSIX,
        listOf("/bin/sh", "-c", "exec node \"\$HOME/.legion-control/agent/src/index.mjs\" \"\$@\"", "legionctl", "config"),
    ),
)

// Fixed script only: HOME is resolved by PowerShell, never interpolated from the SSH user name.
// Encoding keeps the invocation identical under cmd.exe and PowerShell default SSH shells.
internal const val WINDOWS_SETUP_SCRIPT =
    "\$ErrorActionPreference = 'Stop'; try { " +
        "& (Join-Path \$HOME '.legion-control/bin/legionctl.ps1') config; exit \$LASTEXITCODE " +
        "} catch { [Console]::Error.WriteLine(\$_.Exception.Message); exit 1 }"

private val WINDOWS_SETUP_ARGV: List<String> = listOf(
    "powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
    "-EncodedCommand", Base64.getEncoder().encodeToString(WINDOWS_SETUP_SCRIPT.toByteArray(Charsets.UTF_16LE)),
)

/** One layout to try: which grammar to write it in, and the argv to write. */
data class SetupCommand(val grammar: RemoteShell, val arguments: List<String>) {
    /** Null when the argv cannot be written in that grammar, which is a reason to skip it. */
    fun line(): String? = try {
        buildRemoteCommand(grammar, arguments)
    } catch (_: CommandLineRefusal) {
        null
    }
}

/**
 * Runs `config` on one address, with this phone's own key, and reads what came back.
 *
 * Every [AgentFailure] the transport raises is left to travel: a refused key does not get better on
 * the next command shape, and a host key that does not match its pin is a question rather than a
 * fault. A shape that produced a reply ends the search whatever the reply says, and so does a
 * failure that rules out the whole address (section 15.5, step 7): with one address there is
 * nowhere else to go. Anything else moves on to the next shape, which is safe because `config` is a
 * read.
 *
 * [tunnels] are the prefixes this phone's tunnels carry, null before anything has been said about
 * them. A tailnet address typed here that does not answer while none of them carries it is said to
 * be the phone off the tailnet ([untunnelled]), as it is of a machine in the setup.
 */
suspend fun fetchController(transport: CommandTransport, endpoint: Endpoint, tunnels: List<LanPrefix>? = null): ControllerFetch =
    try {
        fetchController { line -> transport.run(endpoint, line, FETCH_TIMEOUT_MILLIS) }
    } catch (failure: AgentFailure) {
        throw untunnelled(endpoint, failure, tunnels)
    }

internal suspend fun fetchController(run: suspend (String) -> CommandOutcome): ControllerFetch {
    var detail: String? = null
    for (command in setupCommands()) {
        val line = command.line() ?: continue
        val outcome = run(line)
        val reading = AttemptClassifier.classify(outcome)
        if (reading.isReply) {
            val reply = readControllerReply(outcome.stdout)
            if (reply != ControllerFetch.Unreadable) return reply
        }
        val text = listOf(outcome.stderr, outcome.stdout)
            .firstOrNull { it.isNotBlank() }
            ?.trim()
            ?: "exit status ${outcome.exitStatus}"
        val condensed = if (text.length <= 400) text else text.take(400) + "..."
        when (reading.kind) {
            FailureKind.TIMED_OUT -> throw AgentFailure.TimedOut((FETCH_TIMEOUT_MILLIS / 1000L).toInt(), condensed)
            FailureKind.LINK_LOST -> throw AgentFailure.Unreachable(
                null,
                "The link closed before the setup arrived: $condensed",
                stage = reading.dispatch,
                reason = FailureKind.LINK_LOST,
            )

            else -> if (detail == null) detail = condensed
        }
    }
    // Exhausting known layouts cannot distinguish an absent agent from one that cannot start.
    throw AgentFailure.DiscoveryFailed(detail)
}

/** One `config` reply, out of whatever else the shell put around it. */
fun readControllerReply(stdout: String): ControllerFetch {
    val json = sliceJsonObject(stdout) ?: return ControllerFetch.Unreadable
    val reply = try {
        AgentJson.parseToJsonElement(json) as? JsonObject
    } catch (_: Exception) {
        null
    } ?: return ControllerFetch.Unreadable
    return readControllerReply(reply)
}

/**
 * The same reading, from a reply that has already been parsed.
 *
 * The three answers are told apart by the key and not by its value, which is the whole reason this
 * is done against the raw object rather than against a decoded class. A missing `controller` and a
 * `controller` of null decode to the same null, and they mean opposite things: one is an agent too
 * old to have anything to say, the other is a current agent saying it holds nothing.
 */
fun readControllerReply(reply: JsonObject, source: String? = null): ControllerFetch {
    if ((reply["ok"] as? JsonPrimitive)?.booleanOrNull == false) {
        val error = (reply["error"] as? JsonPrimitive)?.contentOrNull
        if (error?.trim()?.equals("unknown command: config", ignoreCase = true) == true) {
            return ControllerFetch.TooOld
        }
        throw AgentFailure.Reported(error?.take(400) ?: "The setup request was refused.")
    }
    val document = reply["controller"] ?: return ControllerFetch.TooOld
    if (document is JsonNull) return ControllerFetch.NothingStored
    if (document !is JsonObject) return ControllerFetch.Unreadable
    // The stored bytes, exactly: hashing anything rebuilt from the parsed object reproduces the
    // machine's hash only when this app happens to print it the way its writer did (C1).
    val raw = (reply["raw"] as? JsonPrimitive)?.takeIf { it.isString }?.content
    return ControllerFetch.Document(
        text = raw ?: SetupJson.of(document).print(),
        provenance = provenanceOf(envelope = reply, document = document, readFrom = source),
        replyHash = (reply["hash"] as? JsonPrimitive)?.takeIf { it.isString }?.content,
    )
}

/**
 * The identity a machine reports in `status`, without fetching the document.
 *
 * This is the cheap half of the freshness check: every status carries the mark, and only a mark that
 * says something genuinely newer is worth a second round trip for the bytes.
 */
fun provenanceOf(mark: ControllerMark?, readFrom: String?): SetupProvenance? {
    if (mark == null) return null
    if (mark.hash.isNullOrBlank() && mark.id.isNullOrBlank()) return null
    return SetupProvenance(
        authority = mark.id?.takeIf { it.isNotBlank() },
        revision = mark.revision,
        hash = mark.hash?.takeIf { it.isNotBlank() },
        updatedAt = mark.updatedAt,
        sourceKind = mark.source,
        readFrom = readFrom,
    )
}

/**
 * Who wrote this document and which revision it is, from the envelope first and the document second.
 *
 * The envelope is the agent's own statement about the copy it holds and is preferred. The document
 * carries the same two fields for the case the envelope does not, which is every agent that predates
 * the idea, and for a document pasted in by hand where there is no envelope at all.
 */
fun provenanceOf(
    envelope: JsonObject?,
    document: JsonObject?,
    readFrom: String? = null,
    hashOverride: String? = null,
): SetupProvenance {
    // `config` returns { controller, hash, meta } and `status` returns a controllerMark under
    // `controller`. Both are read, meta first, because meta is what the agent writes in the same
    // transaction as the document itself and is therefore the one that cannot be half true.
    val meta = envelope?.get("meta") as? JsonObject
    val mark = envelope?.get("controller") as? JsonObject

    fun text(vararg keys: String): String? = keys.firstNotNullOfOrNull { key ->
        listOfNotNull(meta, envelope).firstNotNullOfOrNull { holder ->
            (holder[key] as? JsonPrimitive)?.contentOrNull?.takeIf { it.isNotBlank() }
        }
    }

    fun revision(): Long? = listOfNotNull(meta, envelope, mark).firstNotNullOfOrNull { holder ->
        (holder["revision"] as? JsonPrimitive)?.let { it.longOrNull ?: it.contentOrNull?.toLongOrNull() }
    }

    // A document may also carry its own identity, which is how a hand-pasted one gets ordered at
    // all. The agent's metadata is preferred: it is a statement about the copy this machine holds.
    val inner = document?.get("controller") as? JsonObject

    fun documentText(key: String): String? =
        (inner?.get(key) as? JsonPrimitive)?.contentOrNull?.takeIf { it.isNotBlank() }

    fun documentRevision(): Long? = (inner?.get("revision") as? JsonPrimitive)
        ?.let { it.longOrNull ?: it.contentOrNull?.toLongOrNull() }

    return SetupProvenance(
        authority = text("id", "controllerId", "authority") ?: documentText("id"),
        revision = revision() ?: documentRevision(),
        hash = hashOverride ?: text("hash"),
        updatedAt = text("updatedAt") ?: documentText("updatedAt"),
        sourceKind = text("source"),
        readFrom = readFrom,
    )
}

/** One command, on a link that has already been opened. Nothing here waits on the machine to work. */
private const val FETCH_TIMEOUT_MILLIS = 40_000L
