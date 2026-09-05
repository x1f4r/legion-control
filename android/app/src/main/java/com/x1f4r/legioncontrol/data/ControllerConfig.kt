package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.ControllerSource
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.agent.UnquotableArgument
import com.x1f4r.legioncontrol.agent.buildRemoteCommand
import com.x1f4r.legioncontrol.net.parseMacAddress
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/**
 * The controller config, exactly as docs/configuration.md describes it.
 *
 * Every controller edits and shares the same setup document. Device-specific settings live in
 * private bindings. Deprecated shared ssh/local fields remain accepted for older clients, while
 * edits preserve unknown fields through ControllerDocument rather than this decoded view.
 *
 * Every field has a default, so a document missing a key decodes instead of failing, and the
 * checking is done afterwards by [validate] where a failure can be a sentence rather than a
 * serialization exception nobody can act on.
 */
@Serializable
data class ControllerConfig(
    val version: Int? = null,
    /** Where the LANs are. Optional; a document without sites behaves exactly as it always did. */
    val sites: List<SiteConfig> = emptyList(),
    /**
     * The stable setup identity, revision and ancestor hashes shared by all peers. Lineage proves
     * descent; a revision number alone cannot distinguish newer content from concurrent edits.
     */
    val controller: ControllerIdentity? = null,
    val machines: List<MachineConfig> = emptyList(),
    val appUpdates: AppUpdatesConfig = AppUpdatesConfig(),
)

/**
 * The setup this document is, and where it came from.
 *
 * [id] is the SETUP id, shared by every peer, and it is stable for the life of the setup. It used to
 * name the one device allowed to write; there is no such device, so it
 * names the fleet instead. [revision] is still monotonic within one id, and it is still not what
 * decides whether a copy may be taken.
 *
 * [lineage] is what decides that. Revision numbers cannot tell "newer" from "diverged": two people
 * editing revision 5 offline both produce a revision 6, and a number comparison would let the second
 * one silently erase the first. The hashes of the ancestors can tell, because descent is a fact
 * rather than a count, and a copy is only ever taken when one side is provably an ancestor of the
 * other.
 */
@Serializable
data class ControllerIdentity(
    val id: String = "",
    val revision: Long? = null,
    val updatedAt: String? = null,
    /** What to call the setup on screen. Falls back to the id. */
    val name: String? = null,
    /** mac | desktop | phone | cli | legacy: which kind of client last wrote it. */
    val source: String? = null,
    /** Free text: which device wrote it, for the divergence screen. */
    val device: String? = null,
    /** Canonical hashes of the ancestors, newest first, at most 32. */
    val lineage: List<String> = emptyList(),
)

/**
 * One place the machines live, so a controller can tell whether a broadcast could reach them.
 *
 * A prefix match is a hint and never a proof: two houses on the same router defaults have the same
 * private subnet, and an address starting with 192.168.178. says nothing about which of them this
 * phone is standing in. Everything that consumes this treats several matches as "not sure", and
 * what actually authenticates a machine is its pinned host key, never its address.
 */
@Serializable
data class SiteConfig(
    val id: String = "",
    val name: String? = null,
    /** A controller holding an address starting with one of these may be on this site. */
    val lanPrefixes: List<String> = emptyList(),
    /** The default broadcast addresses for machines at this site. */
    val broadcast: List<String> = emptyList(),
) {
    val displayName: String get() = name?.takeIf { it.isNotBlank() } ?: id
}

@Serializable
data class MachineConfig(
    val id: String = "",
    val name: String? = null,
    /** Which site this machine sits on. Must name an entry in [ControllerConfig.sites]. */
    val site: String? = null,
    /**
     * A hint that this machine is always powered.
     *
     * Text only: it changes what a warning says and never what the app does. The user asked not to
     * be pushed towards keeping a tower on, so a helper that is not always on is called out in the
     * editor rather than quietly relied upon.
     */
    val alwaysOn: Boolean = false,
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
    /**
     * This phone counts as at home when one of its addresses starts with this.
     *
     * The pre-sites way of saying where a machine is. Still read, and used for a machine that names
     * no site, so a 1.2 document keeps working unchanged.
     */
    val lanPrefix: String? = null,
    /**
     * The first helper, kept so a 1.2 client can still read one.
     *
     * An editor that writes [helpers] writes this as well, set to the first of them, because a
     * published document has to stay readable by whatever is still on the old build.
     */
    val helper: WakeHelperConfig? = null,
    /**
     * Helpers in the order to try them.
     *
     * One machine cannot wake two different broadcast domains, so a fleet across two houses needs
     * one helper in each, and a helper that is itself asleep needs a second behind it. They are
     * tried in order and never woken automatically: a cascade would quietly turn the always-off
     * tower into an always-on one.
     */
    val helpers: List<WakeHelperConfig> = emptyList(),
) {
    /** The helpers to walk, in order, with the singular alias folded in as the first of them. */
    val effectiveHelpers: List<WakeHelperConfig>
        get() = when {
            helpers.isNotEmpty() -> helpers
            helper != null -> listOf(helper)
            else -> emptyList()
        }
}

/**
 * Another machine in this same document, which is always on and shares the target's network.
 *
 * It is asked to send the magic packet when the phone is somewhere a broadcast cannot reach, which
 * is every network except the machine's own.
 */
@Serializable
data class WakeHelperConfig(
    val machine: String = "",
    /**
     * The id of a configured action on that helper which sends the packet.
     *
     * An ordinary `run <action>` and not a verb of its own: a helper is just a machine
     * with an action on it, which may be the agent's own `wol` action or any command the user has
     * configured, including one that talks to a router.
     */
    val action: String = "",
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
    /**
     * The exact argv run over ssh.
     *
     * It no longer has to avoid quoting: an argv is turned into a command line by the rules of the
     * shell the account logs in to. What it does have to do is say which shell that is, in [shell],
     * whenever any of these needs quoting at all.
     */
    val agent: List<String> = emptyList(),
    /**
     * "posix", "cmd" or "powershell". Absent means "nothing here needs quoting", which is true of
     * every path without a space in it and is checked rather than assumed.
     */
    val shell: String? = null,
    /**
     * True when this system's key is restricted to the agent's forced-command dispatcher.
     *
     * The dispatcher parses a POSIX argv grammar itself and no shell ever sees the line, so a
     * PowerShell system behind one must be serialised as POSIX rather than with a leading call
     * operator. The agent reports `restrictedSession` once a request has landed; this flag is how
     * the very first request gets the serialisation right, before anything has answered.
     */
    val restricted: Boolean = false,
)

@Serializable
data class AppUpdatesConfig(
    val githubRepo: String = DEFAULT_GITHUB_REPO,
)

/** Where the app looks for its own releases when the config does not say. */
const val DEFAULT_GITHUB_REPO = "x1f4r/legion-control"

/** The version of the document this app understands. */
private const val SUPPORTED_VERSION = 1

/** How many ancestors a document carries. Beyond this cap, a stale branch reads as divergence. */
internal const val MAX_LINEAGE = 32

private val SHA256_HEX = Regex("^[0-9a-f]{64}$")

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

    // V1. The setup block, when there is one.
    controller?.let { identity ->
        if (identity.id.isBlank()) {
            return failure(
                "The \"controller\" block has no id. It names the setup, and revisions are only " +
                    "ever compared within one setup.",
            )
        }
        if (!Contract.isToken(identity.id)) {
            return failure(
                "The setup id \"${identity.id}\" is not a valid identifier. Letters, digits " +
                    "and . _ : @ / \\ ~ = + - only, starting with a letter or a digit.",
            )
        }
        if (identity.revision != null && identity.revision < 0) {
            return failure("The setup revision is ${identity.revision}. It has to count up from zero.")
        }
        if (identity.lineage.size > MAX_LINEAGE) {
            return failure(
                "The setup carries ${identity.lineage.size} ancestor hashes and at most " +
                    "$MAX_LINEAGE are allowed.",
            )
        }
        identity.lineage.firstOrNull { !SHA256_HEX.matches(it) }?.let {
            return failure("\"$it\" in the setup's ancestry is not a sha256 hash.")
        }
        if (identity.lineage.size != identity.lineage.toSet().size) {
            return failure("The setup's ancestry lists the same hash twice.")
        }
        identity.source?.takeIf { it.isNotBlank() }?.let { kind ->
            if (ControllerSource.fromWire(kind) == null) {
                return failure(
                    "The setup says it was written by \"$kind\". That has to be one of mac, " +
                        "desktop, phone, cli or legacy.",
                )
            }
        }
    }

    // V2. Sites, and the machines that name them.
    val seenSites = mutableSetOf<String>()
    for (site in sites) {
        if (site.id.isBlank()) return failure("Every site needs an id.")
        if (!Contract.isToken(site.id)) {
            return failure("The site id \"${site.id}\" is not a valid identifier.")
        }
        if (!seenSites.add(site.id)) {
            return failure("Two sites share the id \"${site.id}\". Ids have to be unique.")
        }
    }

    val machineIds = machines.map { it.id }.toSet()
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
            val declared = system.shell
            if (declared != null && RemoteShell.fromWire(declared) == RemoteShell.AUTO &&
                !declared.equals("auto", ignoreCase = true)
            ) {
                return failure(
                    "The system \"${system.id}\" on $where says its shell is \"$declared\". " +
                        "It has to be \"posix\", \"cmd\" or \"powershell\".",
                )
            }
            // Checked here rather than at the moment a button is pressed. An agent path with a space
            // in it and no declared shell is a configuration that cannot be used, and finding that
            // out when Apply is pressed is much better than finding it out during a restart.
            try {
                buildRemoteCommand(RemoteShell.fromWire(system.shell), system.agent + listOf("status"))
            } catch (unquotable: UnquotableArgument) {
                return failure("The system \"${system.id}\" on $where cannot be run: ${unquotable.reason}")
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

        machine.site?.takeIf { it.isNotBlank() }?.let { site ->
            if (site !in seenSites) {
                return failure(
                    "$where says it is at the site \"$site\", and this configuration has no site " +
                        "with that id.",
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
            // V3. Every helper has to exist, not be the machine itself, and name a runnable action.
            for (entry in wake.effectiveHelpers) {
                val helper = entry.machine.takeIf { it.isNotBlank() }
                    ?: return failure("$where lists a wake helper with no machine id.")
                if (helper == machine.id) {
                    return failure(
                        "$where names itself as its own wake helper. A machine that is asleep " +
                            "cannot send its own magic packet.",
                    )
                }
                if (helper !in machineIds) {
                    return failure(
                        "$where names \"$helper\" as a wake helper, and there is no machine " +
                            "with that id in this configuration.",
                    )
                }
                if (entry.action.isBlank()) {
                    return failure(
                        "$where's wake helper \"$helper\" does not say which action on it sends " +
                            "the packet.",
                    )
                }
                if (!Contract.isToken(entry.action)) {
                    return failure(
                        "The wake action \"${entry.action}\" on $where's helper \"$helper\" is " +
                            "not a valid identifier.",
                    )
                }
            }
            if (wake.helpers.isNotEmpty() && wake.helper != null &&
                wake.helper.machine != wake.helpers.first().machine
            ) {
                return failure(
                    "$where lists wake helpers and also a single \"helper\" that is not the first " +
                        "of them. The single one exists so an older client can still read the list, " +
                        "so it has to be the first.",
                )
            }
        }
    }

    // V3, the other half: a helper that needs waking by a helper that needs waking by the first one
    // would sit there forever. Cycles are refused rather than broken at run time.
    helperCycle()?.let { return failure(it) }

    return Result.success(this)
}

/** The names of the machines in a helper cycle, or null when there is none. */
private fun ControllerConfig.helperCycle(): String? {
    val edges = machines.associate { machine ->
        machine.id to machine.wake?.effectiveHelpers.orEmpty().map { it.machine }
    }
    val visiting = mutableSetOf<String>()
    val settled = mutableSetOf<String>()

    fun walk(id: String, path: List<String>): String? {
        if (id in settled) return null
        if (!visiting.add(id)) {
            val cycle = (path.dropWhile { it != id } + id).joinToString(" wakes ")
            return "These machines wake each other in a circle: $cycle. One of them has to be " +
                "reachable without a helper."
        }
        for (next in edges[id].orEmpty()) {
            walk(next, path + id)?.let { return it }
        }
        visiting.remove(id)
        settled.add(id)
        return null
    }

    for (machine in machines) walk(machine.id, emptyList())?.let { return it }
    return null
}

/** Warnings that are worth showing beside a field and are never a reason to refuse a document. */
fun ControllerConfig.advisories(): List<String> = buildList {
    val byId = machines.associateBy { it.id }
    for (machine in machines) {
        val wake = machine.wake ?: continue
        val where = machine.name?.takeIf { it.isNotBlank() } ?: machine.id
        val helpers = wake.effectiveHelpers

        // V5. Nothing on its LAN and nobody to ask.
        val placed = machine.site != null || !wake.lanPrefix.isNullOrBlank()
        if (!placed && helpers.isEmpty()) {
            add(
                "$where can be woken, and nothing says which network it is on and no helper is " +
                    "configured, so only a device already on its LAN could wake it.",
            )
        }

        for (entry in helpers) {
            val helper = byId[entry.machine] ?: continue
            val helperWhere = helper.name?.takeIf { it.isNotBlank() } ?: helper.id
            // V4. A helper somewhere else is legitimate, and only if the action reaches across.
            if (machine.site != null && helper.site != null && machine.site != helper.site) {
                add(
                    "$helperWhere is at a different site than $where, so \"${entry.action}\" only " +
                        "wakes it if that action reaches the other network another way, through a " +
                        "router or a VPN.",
                )
            }
        }

        // W5. A wake path whose helpers are all machines that may themselves be asleep.
        if (helpers.isNotEmpty() && helpers.none { byId[it.machine]?.alwaysOn == true }) {
            val site = machine.site?.let { id -> sites.firstOrNull { it.id == id }?.displayName ?: id }
            add(
                "None of the helpers for $where is marked as always on" +
                    (site?.let { ", so there is no always-on helper for $it" } ?: "") +
                    ". Waking it may need one of them woken first.",
            )
        }
    }
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
  "controller": { "id": "my-mac", "name": "My Mac", "revision": 1 },
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
        { "id": "linux", "name": "Linux", "platform": "linux", "shell": "posix", "agent": ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"] },
        { "id": "windows", "name": "Windows", "platform": "windows", "shell": "powershell", "agent": ["node", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"] }
      ]
    }
  ],
  "appUpdates": { "githubRepo": "$DEFAULT_GITHUB_REPO" }
}
""".trimIndent()
