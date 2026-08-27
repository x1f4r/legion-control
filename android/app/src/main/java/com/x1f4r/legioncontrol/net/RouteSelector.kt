package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.data.Settings
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope

/**
 * Decides which of a machine's addresses to talk to, cheaply.
 *
 * The configuration's own order is the preference order, and this does not second guess it. What it
 * adds is two rounds, because the two cases want opposite things. The normal case wants speed, and
 * gets it from [remembered]: one TCP connect to the address that worked last time, which on a LAN
 * comes back in single digit milliseconds. The failure case wants completeness, and gets it from
 * [reachable], which probes every address at once so that four dead addresses cost one timeout
 * rather than four.
 */
class RouteSelector(
    private val machine: Machine,
    private val settings: Settings,
) {

    data class Probe(val endpoint: Endpoint, val reachable: Boolean)

    /** What the last full probe found, so a screen can say what it looked at rather than just fail. */
    @Volatile
    var lastProbe: List<Probe> = emptyList()
        private set

    /**
     * The route that worked last time, if it still answers.
     *
     * Answering is not a promise that the agent is there or that the key is authorised. It only says
     * there is a point in trying this address before probing the rest.
     */
    suspend fun remembered(probeTimeoutMillis: Int = PROBE_TIMEOUT_MILLIS): Endpoint? {
        val endpoint = machine.endpoint(settings.lastGoodRoute(machine.id).value) ?: return null
        return endpoint.takeIf { TcpProbe.reachable(it.host, it.port, probeTimeoutMillis) }
    }

    /**
     * Every address that answered, in preference order. Empty means nothing is up, which the caller
     * reports as unreachable.
     */
    suspend fun reachable(probeTimeoutMillis: Int = PROBE_TIMEOUT_MILLIS): List<Endpoint> {
        val probes = coroutineScope {
            machine.endpoints
                .map { endpoint ->
                    endpoint to async { TcpProbe.reachable(endpoint.host, endpoint.port, probeTimeoutMillis) }
                }
                .map { (endpoint, running) -> Probe(endpoint, running.await()) }
        }
        lastProbe = probes
        return probes.filter { it.reachable }.map { it.endpoint }
    }

    /** A sentence naming what was tried and found dead, for the message on an unreachable failure. */
    fun probeSummary(): String = lastProbe
        .filterNot { it.reachable }
        .joinToString("; ") { "${it.endpoint.label} (${it.endpoint.address}) did not answer" }
        .ifEmpty { "No address answered." }

    /** Called once a command has actually worked, so the next one starts with a single probe. */
    fun remember(endpoint: Endpoint) = settings.rememberRoute(machine.id, endpoint.id)

    fun forget() = settings.forgetRoute(machine.id)

    private companion object {
        /**
         * Short on purpose. A machine that is up answers a TCP connect on a LAN in single digit
         * milliseconds and over a tunnel in tens; anything slower is a machine that is not there,
         * and waiting longer only delays the fallback.
         */
        const val PROBE_TIMEOUT_MILLIS = 1_500
    }
}
