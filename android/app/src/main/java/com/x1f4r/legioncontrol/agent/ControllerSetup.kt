package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.SshTransport
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.longOrNull

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
    /** A document, and everything the machine that served it said about where it came from. */
    data class Document(val text: String, val provenance: SetupProvenance) : ControllerFetch

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
 * Built through the quoting rules rather than by pasting a user name into a string. A user name with
 * a space in it is unusual and it is not impossible, and it is the one value here that comes from
 * outside.
 */
fun setupCommands(user: String): List<SetupCommand> = listOf(
    SetupCommand(
        RemoteShell.POSIX,
        listOf("/usr/bin/node", "/home/$user/.legion-control/agent/src/index.mjs", "config"),
    ),
    SetupCommand(
        RemoteShell.POSIX,
        listOf("node", "~/.legion-control/agent/src/index.mjs", "config"),
    ),
    SetupCommand(
        RemoteShell.POSIX,
        listOf("/opt/homebrew/bin/node", "~/.legion-control/agent/src/index.mjs", "config"),
    ),
    SetupCommand(
        RemoteShell.CMD,
        listOf("node", "C:\\Users\\$user\\.legion-control\\agent\\src\\index.mjs", "config"),
    ),
    SetupCommand(
        RemoteShell.POWERSHELL,
        listOf("node", "C:\\Users\\$user\\.legion-control\\agent\\src\\index.mjs", "config"),
    ),
)

/** One layout to try: which shell to write it for, and the argv to write. */
data class SetupCommand(val shell: RemoteShell, val arguments: List<String>) {
    /** Null when the user name cannot be written for that shell, which is a reason to skip it. */
    fun line(): String? = try {
        buildRemoteCommand(shell, arguments)
    } catch (_: UnquotableArgument) {
        null
    }
}

/**
 * Runs `config` on one address, with this phone's own key, and reads what came back.
 *
 * Every [AgentFailure] the transport raises is left to travel: a refused key does not get better on
 * the next command shape, and a host key that does not match its pin is a question rather than a
 * fault. Only a shape that produced no reply moves on to the next one, and a shape that produced one
 * ends the search whatever the reply says.
 */
suspend fun fetchController(transport: SshTransport, endpoint: Endpoint): ControllerFetch {
    var detail: String? = null
    // Two shapes can produce the same line: when nothing in a command needs quoting, the cmd.exe and
    // the PowerShell forms are byte for byte the same, and running it twice would only cost a round
    // trip against a machine that has already said no once.
    val tried = mutableSetOf<String>()
    for (command in setupCommands(endpoint.user)) {
        val line = command.line() ?: continue
        if (!tried.add(line)) continue
        val outcome = transport.run(endpoint, line, FETCH_TIMEOUT_MILLIS)
        val reply = readControllerReply(outcome.stdout)
        if (reply != ControllerFetch.Unreadable) return reply
        if (detail == null) {
            val text = listOf(outcome.stderr, outcome.stdout)
                .firstOrNull { it.isNotBlank() }
                ?.trim()
                ?: "exit status ${outcome.exitStatus}"
            detail = if (text.length <= 400) text else text.take(400) + "..."
        }
    }
    // Every shape ran and none of them was the agent. That is the one failure this can end in that
    // is not the transport's, and it means the same thing here as everywhere else: whatever is on
    // the far side, the control agent is not part of it.
    throw AgentFailure.AgentMissing(null, detail)
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
    val document = reply["controller"] ?: return ControllerFetch.TooOld
    if (document is JsonNull) return ControllerFetch.NothingStored
    if (document !is JsonObject) return ControllerFetch.Unreadable
    return ControllerFetch.Document(
        text = SetupJson.encodeToString(JsonObject.serializer(), document),
        provenance = provenanceOf(envelope = reply, document = document, readFrom = source),
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

/**
 * Written back out rather than passed through, because the bytes on the far side are one line of
 * JSON and what lands in the field on screen is meant to be read and edited.
 */
private val SetupJson: Json = Json {
    prettyPrint = true
    prettyPrintIndent = "  "
}

/** One command, on a link that has already been opened. Nothing here waits on the machine to work. */
private const val FETCH_TIMEOUT_MILLIS = 40_000L
