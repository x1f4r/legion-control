package com.x1f4r.legioncontrol.ui

import android.content.Context
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentReply
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.LegionControl
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineControl
import com.x1f4r.legioncontrol.data.ControllerConfig
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.map

/**
 * The one seam between the screen and the transport.
 *
 * The transport throws and the screen wants results; the transport hands back the endpoint it used
 * and the screen wants to know how to name it. Both differences are settled here, in one file, so
 * that neither side has to be written around the other.
 */
fun createControlServices(context: Context): ControlServices = WiredServices(LegionControl(context))

private class WiredServices(private val control: LegionControl) : ControlServices {

    override suspend fun publicKey(): String = control.identity.identity().authorizedKeysLine

    override val config: StateFlow<ControllerConfig?> get() = control.config.config

    override val configText: StateFlow<String> get() = control.config.text

    override fun applyConfig(text: String): String? = control.config.apply(text)

    override fun clients(configuration: ControllerConfig?): List<MachineClient> =
        control.controls(configuration).map { WiredMachineClient(control, it) }
}

private class WiredMachineClient(
    private val control: LegionControl,
    private val wiring: MachineControl,
) : MachineClient {

    override val machine: Machine get() = wiring.machine

    override fun wakeReadiness(): Flow<WakeReadiness> =
        control.homeNetwork.observe(machine.wake?.lanPrefix).map { state ->
            WakeReadiness(possible = state.onHomeNetwork, explanation = state.explanation)
        }

    @Volatile
    private var lastRoute: Route? = null

    override val route: Route? get() = lastRoute

    override suspend fun status(): Result<AgentStatus> = attempt { wiring.agent.status() }

    override suspend fun update(serviceId: String?, force: Boolean): Result<AgentActionResult> =
        attempt { wiring.agent.update(serviceId, force) }

    override suspend fun restart(serviceId: String?, force: Boolean): Result<AgentActionResult> =
        attempt { wiring.agent.restart(serviceId, force) }

    override suspend fun setAutoUpdate(on: Boolean): Result<AgentActionResult> =
        attempt { wiring.agent.setAutoUpdate(on) }

    override suspend fun boot(targetId: String, force: Boolean): Result<AgentActionResult> =
        attempt { wiring.agent.boot(targetId, force) }

    override suspend fun sleep(force: Boolean): Result<AgentActionResult> =
        attempt { wiring.agent.sleep(force) }

    override suspend fun run(actionId: String, force: Boolean): Result<AgentActionResult> =
        attempt { wiring.agent.run(actionId, force) }

    override suspend fun wake(): Result<Unit> {
        val wake = wiring.wake
            ?: return Result.failure(
                AgentFailure.Unreachable(machine.name, "This machine has no wake configuration."),
            )
        // The machine's own network, not whatever the default route happens to be. With a tunnel up
        // the default is the tunnel, and a broadcast put on that reaches nothing.
        val problem = wake.send(via = control.homeNetwork.homeNetwork(machine.wake?.lanPrefix))
        if (problem != null) {
            return Result.failure(AgentFailure.Unreachable(machine.name, problem))
        }
        // The packet leaving is not the same as the machine waking, and only one of those is worth
        // reporting as success. Sit on the probe address until it answers or the cap runs out.
        return if (wake.waitForSsh()) {
            Result.success(Unit)
        } else {
            Result.failure(
                AgentFailure.Unreachable(
                    machine.name,
                    "No answer on ${wake.probeAddress} within 45 seconds. " +
                        "It may still be starting up.",
                ),
            )
        }
    }

    override suspend fun trustHostKey(address: String, keyBlobBase64: String) {
        // An address the app no longer dials cannot raise the question, so the fallback is moot;
        // one key is simply the conservative answer for an address nothing knows anything about.
        val capacity = machine.endpoints.firstOrNull { it.address == address }?.trustedKeyCapacity ?: 1
        control.hostKeys.trust(address, keyBlobBase64, capacity)
    }

    /**
     * Runs one call and remembers which way it went.
     *
     * Only [AgentFailure] is caught. Anything else is a defect in this app rather than a fact about
     * the machine, and swallowing it into a status line at the bottom of the screen would hide it.
     */
    private suspend fun <T : Any> attempt(call: suspend () -> AgentReply<T>): Result<T> = try {
        val reply = call()
        lastRoute = Route(reply.route.label, reply.route.kind)
        Result.success(reply.value)
    } catch (failure: AgentFailure) {
        Result.failure(failure)
    }
}
