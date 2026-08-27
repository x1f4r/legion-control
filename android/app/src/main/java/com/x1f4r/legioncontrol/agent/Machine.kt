package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.MachineConfig
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteKind

/** What kind of system this is. It picks the glyph and the wording, and nothing else. */
enum class Platform(val wire: String) {
    LINUX("linux"),
    WINDOWS("windows"),
    MAC("mac"),
    ;

    companion object {
        /** Anything unrecognised reads as Linux rather than failing: it only costs a glyph. */
        fun fromWire(value: String?): Platform =
            entries.firstOrNull { it.wire.equals(value, ignoreCase = true) } ?: LINUX
    }
}

/**
 * One operating system installed on a machine.
 *
 * [id] is the whole of the contract with the far side: it is what the agent's own `system.id`
 * reports, what a boot target is named by, and what the interpreter path is chosen with. [agent] is
 * the argv run over ssh, straight out of the configuration and never assembled here.
 */
data class MachineSystem(
    val id: String,
    val name: String,
    val platform: Platform,
    val agent: List<String>,
) {
    /**
     * A system the agent named that the configuration does not have. It can still be reported and
     * still be booted away from; it just has no interpreter path, so nothing can be run on it.
     */
    val isConfigured: Boolean get() = agent.isNotEmpty()
}

/** Everything a magic packet needs, and how to tell when the machine is back. */
data class WakeTarget(
    val mac: String,
    val broadcasts: List<String>,
    val ports: List<Int>,
    val probeHost: String,
    val probePort: Int,
    /** The phone counts as at home when one of its addresses starts with this. */
    val lanPrefix: String?,
)

/**
 * A physical box: one network card, one hardware address, one thing to wake, and the systems
 * installed on it.
 *
 * Built from the configuration and never from anything else. Nothing in this app knows a machine
 * that is not in here, which is the point of the whole exercise.
 */
data class Machine(
    val id: String,
    val name: String,
    val endpoints: List<Endpoint>,
    val systems: List<MachineSystem>,
    val wake: WakeTarget?,
) {
    fun system(id: String?): MachineSystem? = systems.firstOrNull { it.id == id }

    fun endpoint(id: String?): Endpoint? = endpoints.firstOrNull { it.id == id }

    /** Every address this machine is dialled on, which is what a host key is pinned against. */
    val addresses: List<String> get() = endpoints.map { it.address }
}

/** The machines the configuration describes, in the order it lists them. */
fun ControllerConfig.toMachines(): List<Machine> = machines.map { it.toMachine() }

private fun MachineConfig.toMachine(): Machine {
    val systems = systems.map { system ->
        MachineSystem(
            id = system.id,
            name = system.name?.takeIf { it.isNotBlank() } ?: system.id,
            platform = Platform.fromWire(system.platform),
            agent = system.agent,
        )
    }
    val endpoints = endpoints.map { endpoint ->
        val kind = if (endpoint.kind == "lan") RouteKind.LAN else RouteKind.REMOTE
        Endpoint(
            id = endpoint.id,
            kind = kind,
            host = endpoint.host,
            port = endpoint.port,
            user = endpoint.user,
            systemHint = endpoint.system,
            label = endpoint.label?.takeIf { it.isNotBlank() } ?: endpoint.host,
            trustedKeyCapacity = when (kind) {
                RouteKind.LAN -> systems.size.coerceAtLeast(1)
                RouteKind.REMOTE -> 1
            },
        )
    }
    // The address the wake waits on. The config's own probe if it named one, otherwise the LAN
    // address the packet is going to reach the machine on anyway: a machine that has just come out
    // of sleep answers sshd well before a tunnel has re-registered, so waiting on a remote address
    // would report a failure for a machine that is already up.
    val fallbackProbe = endpoints.firstOrNull { it.kind == RouteKind.LAN } ?: endpoints.firstOrNull()
    return Machine(
        id = id,
        name = name?.takeIf { it.isNotBlank() } ?: id,
        endpoints = endpoints,
        systems = systems,
        wake = wake?.let { wake ->
            WakeTarget(
                mac = wake.mac,
                broadcasts = wake.broadcast.filter { it.isNotBlank() },
                ports = wake.ports.filter { it in 1..65535 }.ifEmpty { listOf(9, 7) },
                probeHost = wake.probe?.host?.takeIf { it.isNotBlank() }
                    ?: fallbackProbe?.host.orEmpty(),
                probePort = wake.probe?.port?.takeIf { it in 1..65535 }
                    ?: fallbackProbe?.port ?: 22,
                lanPrefix = wake.lanPrefix?.takeIf { it.isNotBlank() },
            )
        },
    )
}

/**
 * Which of this machine's systems the agent's reply is about.
 *
 * By id first, because the id is the contract. An agent that predates the idea of a system reports
 * only its platform name, so when exactly one configured system runs that platform, that is the one
 * it must be: it is what lets the apps be updated before the machines are. Anything else is reported
 * under the name the agent gave it, with no interpreter path, so it can be seen and booted away from
 * but nothing can be run on it.
 */
fun Machine.resolveSystem(reportedId: String?, reportedName: String?, platformName: String?): MachineSystem? {
    val id = reportedId?.takeIf { it.isNotBlank() } ?: return null
    system(id)?.let { return it }

    val platform = Platform.entries.firstOrNull { it.wire.equals(platformName, ignoreCase = true) }
    if (platform != null) {
        val candidates = systems.filter { it.platform == platform }
        if (candidates.size == 1) return candidates.single()
    }
    return MachineSystem(
        id = id,
        name = reportedName?.takeIf { it.isNotBlank() } ?: id,
        platform = Platform.fromWire(platformName),
        agent = emptyList(),
    )
}
