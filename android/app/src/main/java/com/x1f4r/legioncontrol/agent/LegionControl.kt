package com.x1f4r.legioncontrol.agent

import android.content.Context
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.DeviceIdentity
import com.x1f4r.legioncontrol.data.HostKeyStore
import com.x1f4r.legioncontrol.data.SetupSource
import com.x1f4r.legioncontrol.data.Settings
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.HomeNetwork
import com.x1f4r.legioncontrol.net.RouteKind
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.SshTransport
import com.x1f4r.legioncontrol.net.WakeOnLan

/** One machine, wired up: how to reach it, how to wake it, and how to talk to its agent. */
class MachineControl(
    val machine: Machine,
    val routes: RouteSelector,
    val agent: AgentClient,
    /** Null for a machine the configuration says nothing about waking. */
    val wake: WakeOnLan?,
)

/**
 * Everything below the UI, wired up once.
 *
 * There is no dependency injection framework here and there does not need to be one: a handful of
 * objects with one wiring, all of them cheap, none of them with a lifecycle beyond the process.
 * Build one of these per process and hand it to whatever holds the screen state.
 *
 * The machines are not part of that wiring, because they are not known at build time. They come out
 * of [config] and are rebuilt with [controls] whenever it changes, which is also the moment the
 * stored host keys and route hints are pruned back to what the configuration still names.
 */
class LegionControl(context: Context) {
    private val appContext = context.applicationContext

    /** Remembered route and remembered system, per machine. Hints only, never trusted. */
    val settings: Settings = Settings(appContext)

    /** This phone's ed25519 key. Generated on first use; the public half is what the user pastes. */
    val identity: DeviceIdentity = DeviceIdentity(appContext)

    /** Trusted host keys per address, trust on first use. */
    val hostKeys: HostKeyStore = HostKeyStore(appContext)

    /** The pasted configuration. Everything above this line is the same whatever it says. */
    val config: ConfigStore = ConfigStore(appContext)

    /** Whether wake-on-LAN is possible right now, so the UI can be honest about it. */
    val homeNetwork: HomeNetwork = HomeNetwork(appContext)

    /**
     * The machines [configuration] describes, each with its own transport, routes and agent.
     *
     * Addresses and machines that are no longer configured cannot be asked about and cannot be
     * forgotten from any screen, so their host keys and route hints would sit in storage forever.
     * They are dropped here, at the one moment the app learns which ones still exist.
     */
    /**
     * Reads the setup off a machine that is not in the configuration, because there is none yet.
     *
     * The one call in this app that talks to an address nobody has been told about, and it is what
     * makes the first launch a host, a port and a user instead of a document. Everything else about
     * it is ordinary: this phone's own key, the same host key pinning as any other address, and the
     * same agent on the far side. One key, because an address given by hand is one system.
     */
    suspend fun fetchSetup(source: SetupSource): ControllerFetch {
        val endpoint = Endpoint(
            id = "setup",
            kind = RouteKind.REMOTE,
            host = source.host,
            port = source.port,
            user = source.user,
            systemHint = null,
            label = source.host,
            trustedKeyCapacity = 1,
        )
        val transport = SshTransport(identity, hostKeys, source.host)
        return fetchController(transport, endpoint)
    }

    fun controls(configuration: ControllerConfig?): List<MachineControl> {
        val machines = configuration?.toMachines().orEmpty()
        hostKeys.retainOnly(machines.flatMap { it.addresses }.toSet())
        settings.retainOnly(machines.map { it.id }.toSet())

        return machines.map { machine ->
            val routes = RouteSelector(machine, settings)
            val transport = SshTransport(identity, hostKeys, machine.name)
            MachineControl(
                machine = machine,
                routes = routes,
                agent = AgentClient(machine, transport, routes, settings),
                wake = machine.wake?.let(::WakeOnLan),
            )
        }
    }
}
