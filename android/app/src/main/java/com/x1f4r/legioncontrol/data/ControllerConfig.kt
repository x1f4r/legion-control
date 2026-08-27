package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.net.parseMacAddress
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/**
 * The controller config, exactly as docs/configuration.md describes it.
 *
 * This app knows nothing about any machine until this document says so. It is the same JSON the Mac
 * reads from ~/.config/legion-control/config.json, pasted into the configuration field on the
 * "This device" page, which is why the keys the Mac owns are accepted and ignored here rather than
 * rejected: "ssh" is how the Mac dials, "local" is the Mac looking after itself, and a user who
 * keeps one document for both should not have to keep two.
 *
 * Every field has a default, so a document missing a key decodes instead of failing, and the
 * checking is done afterwards by [validate] where a failure can be a sentence rather than a
 * serialization exception nobody can act on.
 */
@Serializable
data class ControllerConfig(
    val version: Int? = null,
    val machines: List<MachineConfig> = emptyList(),
    val appUpdates: AppUpdatesConfig = AppUpdatesConfig(),
)

@Serializable
data class MachineConfig(
    val id: String = "",
    val name: String? = null,
    val endpoints: List<EndpointConfig> = emptyList(),
    val wake: WakeConfig? = null,
    val systems: List<SystemConfig> = emptyList(),
)

/**
 * One address the machine might answer on.
 *
 * [system] is a routing hint and nothing more. An address that belongs to one system, the way a
 * tailnet address does when the two systems are separate nodes, saves a round trip guessing which
 * interpreter to ask for. It never decides what the screen reports: that comes out of the agent.
 */
@Serializable
data class EndpointConfig(
    val id: String = "",
    /** "lan" (a broadcast domain shared with the machine, so wake works) or "remote". */
    val kind: String = "remote",
    val host: String = "",
    val port: Int = 22,
    val user: String = "",
    val system: String? = null,
    val label: String? = null,
)

@Serializable
data class WakeConfig(
    val mac: String = "",
    val broadcast: List<String> = emptyList(),
    val ports: List<Int> = listOf(9, 7),
    val probe: WakeProbeConfig? = null,
    /** This phone counts as at home when one of its addresses starts with this. */
    val lanPrefix: String? = null,
)

@Serializable
data class WakeProbeConfig(
    val host: String = "",
    val port: Int = 22,
)

@Serializable
data class SystemConfig(
    val id: String = "",
    val name: String? = null,
    /** "linux", "windows" or "mac". Anything else falls back to linux rather than failing. */
    val platform: String = "linux",
    /** The exact argv run over ssh. It must not need quoting. */
    val agent: List<String> = emptyList(),
)

@Serializable
data class AppUpdatesConfig(
    val githubRepo: String = DEFAULT_GITHUB_REPO,
)

/** Where the app looks for its own releases when the config does not say. */
const val DEFAULT_GITHUB_REPO = "x1f4r/legion-control"

/** The version of the document this app understands. */
private const val SUPPORTED_VERSION = 1

/**
 * Lenient on the way in for the same reason the agent's replies are: the Mac's half of this
 * document is not ours to reject, and a key added by a later release must cost nothing here.
 */
private val ConfigJson: Json = Json {
    ignoreUnknownKeys = true
    isLenient = true
    explicitNulls = false
}

/**
 * Reads a pasted document, or explains in one sentence why it cannot be used.
 *
 * Both halves matter. A document that does not parse and a document that parses into something
 * unusable are the same thing to the person holding the phone, and both have to come back as
 * something they can act on rather than as a stack trace or as an app that quietly does nothing.
 */
fun readControllerConfig(text: String): Result<ControllerConfig> {
    if (text.isBlank()) {
        return failure("There is nothing to apply. Paste a configuration, or insert the example.")
    }
    val parsed = try {
        ConfigJson.decodeFromString(ControllerConfig.serializer(), text)
    } catch (failure: Exception) {
        return failure("That is not a configuration this app can read: ${oneLine(failure)}")
    }
    return parsed.validate()
}

/**
 * The checks that decide whether this document can actually drive the app.
 *
 * Deliberately not "everything that could be wrong": a name, a label or a platform that is missing
 * or unknown falls back, because none of those can strand the app. What is checked is what the app
 * would otherwise fail on silently: an id it cannot tell apart from another, a machine with nowhere
 * to dial or nothing to run, and a hardware address a magic packet cannot be built from.
 */
fun ControllerConfig.validate(): Result<ControllerConfig> {
    if (version != null && version != SUPPORTED_VERSION) {
        return failure(
            "This configuration says version $version, and this app understands version " +
                "$SUPPORTED_VERSION.",
        )
    }
    if (machines.isEmpty()) return failure("The configuration lists no machines.")

    val seenMachines = mutableSetOf<String>()
    for (machine in machines) {
        val where = machine.id.ifBlank { machine.name.orEmpty() }.ifBlank { "a machine" }
        if (machine.id.isBlank()) return failure("Every machine needs an id. $where has none.")
        if (!seenMachines.add(machine.id)) {
            return failure("Two machines share the id \"${machine.id}\". Ids have to be unique.")
        }

        if (machine.systems.isEmpty()) {
            return failure("$where has no systems. Every machine needs at least one.")
        }
        val seenSystems = mutableSetOf<String>()
        for (system in machine.systems) {
            if (system.id.isBlank()) return failure("A system on $where has no id.")
            if (!seenSystems.add(system.id)) {
                return failure("$where lists the system \"${system.id}\" twice.")
            }
            if (system.agent.isEmpty() || system.agent.any { it.isBlank() }) {
                return failure(
                    "The system \"${system.id}\" on $where has no agent command. That is the argv " +
                        "run over ssh, for example [\"/usr/bin/node\", \"…/agent/src/index.mjs\"].",
                )
            }
        }

        if (machine.endpoints.isEmpty()) {
            return failure("$where has no endpoints. Every machine needs at least one address.")
        }
        val seenEndpoints = mutableSetOf<String>()
        for (endpoint in machine.endpoints) {
            if (endpoint.id.isBlank()) return failure("An endpoint on $where has no id.")
            if (!seenEndpoints.add(endpoint.id)) {
                return failure("$where lists the endpoint \"${endpoint.id}\" twice.")
            }
            if (endpoint.kind != "lan" && endpoint.kind != "remote") {
                return failure(
                    "The endpoint \"${endpoint.id}\" on $where has kind \"${endpoint.kind}\". " +
                        "It has to be \"lan\" or \"remote\".",
                )
            }
            if (endpoint.host.isBlank()) {
                return failure("The endpoint \"${endpoint.id}\" on $where has no host.")
            }
            if (endpoint.port !in 1..65535) {
                return failure(
                    "The endpoint \"${endpoint.id}\" on $where has port ${endpoint.port}.",
                )
            }
            if (endpoint.user.isBlank()) {
                return failure(
                    "The endpoint \"${endpoint.id}\" on $where has no user to log in as.",
                )
            }
            val hint = endpoint.system
            if (hint != null && hint !in seenSystems) {
                return failure(
                    "The endpoint \"${endpoint.id}\" on $where points at the system \"$hint\", " +
                        "which $where does not have.",
                )
            }
        }

        val wake = machine.wake
        if (wake != null) {
            if (parseMacAddress(wake.mac) == null) {
                return failure(
                    "The wake address \"${wake.mac}\" on $where is not a hardware address. Six " +
                        "hex pairs, separated by colons.",
                )
            }
            if (wake.broadcast.none { it.isNotBlank() }) {
                return failure(
                    "$where can be woken but has no broadcast address to send the magic packet to.",
                )
            }
        }
    }
    return Result.success(this)
}

private fun failure(message: String): Result<ControllerConfig> =
    Result.failure(IllegalArgumentException(message))

private fun oneLine(failure: Throwable): String {
    val text = failure.message?.takeIf { it.isNotBlank() } ?: failure::class.java.simpleName
    val first = text.lineSequence().first().trim()
    return if (first.length <= 200) first else first.take(200) + "..."
}

/**
 * The document from docs/configuration.md, with the two keys only the Mac reads left out and the
 * addresses replaced by ones that cannot belong to anybody.
 *
 * It is a template rather than a working config, and the hardware address says so: it is the one
 * field the app refuses on sight, so inserting this and pressing Apply says exactly what has to be
 * filled in rather than quietly storing a configuration that points at nothing.
 */
val EXAMPLE_CONTROLLER_CONFIG: String = """
{
  "version": 1,
  "machines": [
    {
      "id": "workstation",
      "name": "Workstation",
      "endpoints": [
        { "id": "tailnet-linux", "kind": "remote", "host": "100.64.0.10", "port": 22, "user": "me", "system": "linux", "label": "Tailnet, Linux" },
        { "id": "tailnet-windows", "kind": "remote", "host": "100.64.0.11", "port": 22, "user": "me", "system": "windows", "label": "Tailnet, Windows" },
        { "id": "lan", "kind": "lan", "host": "10.0.0.40", "port": 22, "user": "me", "label": "Home LAN" }
      ],
      "wake": {
        "mac": "XX:XX:XX:XX:XX:XX",
        "broadcast": ["10.0.0.255"],
        "ports": [9, 7],
        "probe": { "host": "10.0.0.40", "port": 22 },
        "lanPrefix": "10.0.0."
      },
      "systems": [
        { "id": "linux", "name": "Linux", "platform": "linux", "agent": ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"] },
        { "id": "windows", "name": "Windows", "platform": "windows", "agent": ["node", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"] }
      ]
    }
  ],
  "appUpdates": { "githubRepo": "$DEFAULT_GITHUB_REPO" }
}
""".trimIndent()
