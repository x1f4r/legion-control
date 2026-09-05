package com.x1f4r.legioncontrol.agent

import android.content.Context
import android.os.Build
import com.x1f4r.legioncontrol.data.BindingsStore
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.DeviceIdentity
import com.x1f4r.legioncontrol.data.HostKeyStore
import com.x1f4r.legioncontrol.data.OperationStore
import com.x1f4r.legioncontrol.data.RevisionCache
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

    /** Host keys the user explicitly approved, kept per address. */
    val hostKeys: HostKeyStore = HostKeyStore(appContext)

    /** The last few documents this device held, so a merge has a base to work from. */
    val revisions: RevisionCache = RevisionCache(appContext)

    /** What is true about this phone in particular and is never published. */
    val bindings: BindingsStore = BindingsStore(appContext)

    /** The setup in force. Every device is a peer, so this one edits and publishes like the rest. */
    val config: ConfigStore = ConfigStore(appContext, revisions)

    /**
     * What to call this device in a revision it writes.
     *
     * The user's own name for it when they have given one, and the hardware name otherwise. It goes
     * into `controller.device` so a divergence screen on another device can say who made the edit,
     * which is the only reason it exists.
     */
    fun deviceName(): String = bindings.bindings.value.deviceName.takeIf { it.isNotBlank() }
        ?: "${Build.MANUFACTURER} ${Build.MODEL}".trim().ifBlank { "an Android device" }

    /** Every change this app has asked for, kept across launches. */
    val operations: OperationStore = OperationStore(appContext)

    /** Whether wake-on-LAN is possible right now, so the UI can be honest about it. */
    val homeNetwork: HomeNetwork = HomeNetwork(appContext)

    /**
     * The controls built last time, keyed by machine id.
     *
     * Kept so that a configuration change that leaves a machine untouched does not throw away its
     * route memory, its backoff state and its knowledge of what the agent can do. Rebuilding all of
     * them on every emission was one of the ways a poll could end up re-asking questions it had
     * already answered, and it also reset the setup hash each model had remembered, which let the
     * same document be offered and taken over and over.
     */
    private val built = mutableMapOf<String, MachineControl>()

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
        )
        val transport = SshTransport(identity, hostKeys, source.host)
        return fetchController(transport, endpoint)
    }

    /**
     * The machines [configuration] describes, each with its own transport, routes and agent.
     *
     * Route hints and local operation history follow the configured machines. Host identities are
     * private trust decisions and remain intact when shared topology changes.
     */
    fun controls(configuration: ControllerConfig?): List<MachineControl> {
        val machines = configuration?.toMachines().orEmpty()
        settings.retainOnly(machines.map { it.id }.toSet())
        operations.retainOnly(machines.map { it.id }.toSet())

        built.keys.retainAll(machines.map { it.id }.toSet())
        return machines.map { machine ->
            // Only the machines whose definition actually changed are rebuilt. Everything a control
            // has learned about a machine, from which route answered to what its agent can do, is
            // worth more than the tidiness of starting again.
            built[machine.id]?.takeIf { it.machine == machine } ?: build(machine).also {
                built[machine.id] = it
            }
        }
    }

    fun prepareRoute(machineId: String, systemId: String, onSite: Boolean) {
        built[machineId]?.routes?.prepareForSystem(systemId, onSite)
            ?: settings.run {
                forgetRoute(machineId)
                rememberSystem(machineId, systemId)
            }
    }

    private fun build(machine: Machine): MachineControl {
        val routes = RouteSelector(machine, settings)
        val transport = SshTransport(identity, hostKeys, machine.name)
        return MachineControl(
            machine = machine,
            routes = routes,
            agent = AgentClient(machine, transport, routes, settings),
            wake = machine.wake?.let(::WakeOnLan),
        )
    }
}
