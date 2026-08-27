package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.SshTransport
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

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
 * suspicion as one that came off a clipboard.
 */
sealed interface ControllerFetch {
    /** A document, and the hash the machine that served it gave it. */
    data class Document(val text: String, val hash: String?) : ControllerFetch

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
 */
fun setupCommands(user: String): List<String> = listOf(
    "/usr/bin/node /home/$user/.legion-control/agent/src/index.mjs config",
    "node C:\\Users\\$user\\.legion-control\\agent\\src\\index.mjs config",
    "node ~/.legion-control/agent/src/index.mjs config",
    "/opt/homebrew/bin/node ~/.legion-control/agent/src/index.mjs config",
)

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
    for (command in setupCommands(endpoint.user)) {
        val outcome = transport.run(endpoint, command, FETCH_TIMEOUT_MILLIS)
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
fun readControllerReply(reply: JsonObject): ControllerFetch {
    val document = reply["controller"] ?: return ControllerFetch.TooOld
    if (document is JsonNull) return ControllerFetch.NothingStored
    if (document !is JsonObject) return ControllerFetch.Unreadable
    val hash = (reply["hash"] as? JsonPrimitive)?.contentOrNull?.takeIf { it.isNotBlank() }
    return ControllerFetch.Document(
        text = SetupJson.encodeToString(JsonObject.serializer(), document),
        hash = hash,
    )
}

/**
 * Whether a status that reported [reported] is a reason to go and read the document.
 *
 * [applied] is the hash of what the phone is running on, so a machine that agrees with it is
 * carrying nothing new. [lastSeen] is the last hash this machine reported, and it is what keeps a
 * document that will not validate from being fetched again every fifteen seconds: it is refused
 * once, and asking the same machine for the same bytes again would only be refused the same way.
 */
fun shouldFetchSetup(reported: String?, applied: String?, lastSeen: String?): Boolean =
    !reported.isNullOrBlank() && reported != applied && reported != lastSeen

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
