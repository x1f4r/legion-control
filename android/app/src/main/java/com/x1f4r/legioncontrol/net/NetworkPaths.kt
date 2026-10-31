package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind

/**
 * The way out of this phone: the network its traffic takes by default, by handle, and the IPv4
 * prefixes every tunnel this app's traffic can use carries.
 *
 * Kept apart from the addresses a wake is placed from, which leave every VPN out: a tunnel has no
 * broadcast domain. What a tunnel routes is what says whether a tailnet address can be reached at
 * all, and a change in either half is a reason to dial an address again that was silent a moment
 * ago.
 */
data class NetworkPaths(
    val defaultNetwork: Long?,
    val tunnels: List<LanPrefix>,
)

/**
 * What the default network and each tunnel contribute, kept per network so that one going away
 * takes exactly its own routes with it.
 *
 * Pure Kotlin, as [NetworkAddressBook] is, so the bookkeeping can be tested without a device:
 * networks are keyed by their handle, and the routes arrive already read off their link.
 */
class NetworkPathBook {
    private data class Tunnel(val prefixes: List<LanPrefix> = emptyList(), val blocked: Boolean = false)

    private var defaultNetwork: Long? = null
    private val tunnels = LinkedHashMap<Long, Tunnel>()

    /** The network the system now sends this app's traffic over by default. */
    @Synchronized
    fun defaultNetwork(handle: Long) {
        defaultNetwork = handle
    }

    /** The default network went away; one that takes its place says so on its own. */
    @Synchronized
    fun defaultLost(handle: Long) {
        if (defaultNetwork == handle) defaultNetwork = null
    }

    /** The IPv4 prefixes the tunnel [handle] routes now. */
    @Synchronized
    fun tunnel(handle: Long, prefixes: List<LanPrefix>) {
        tunnels[handle] = (tunnels[handle] ?: Tunnel()).copy(prefixes = prefixes)
    }

    /** Whether the system blocks the tunnel [handle] for this app. A blocked one carries nothing. */
    @Synchronized
    fun blocked(handle: Long, blocked: Boolean) {
        tunnels[handle] = (tunnels[handle] ?: Tunnel()).copy(blocked = blocked)
    }

    @Synchronized
    fun remove(handle: Long) {
        tunnels.remove(handle)
    }

    @Synchronized
    fun snapshot(): NetworkPaths =
        NetworkPaths(defaultNetwork, tunnels.values.filter { !it.blocked }.flatMap { it.prefixes }.distinct())
}

/**
 * The way out of this phone as last seen, and what a change in it does: every address held back for
 * going unanswered is dialled again on its machine's next call. One that was silent a moment ago may
 * be on the far side of a tunnel that has just come up, or of a network the phone has just joined.
 *
 * Pure Kotlin, so what lifts a backoff is tested without a device. [selectors] is read at each change
 * rather than kept, because the machines are rebuilt as the setup changes.
 */
class WayOut(private val selectors: () -> Collection<RouteSelector>) {
    /** As last seen, or null before anything has been said about it. Until then no silence is put down to a missing tunnel. */
    @Volatile
    var paths: NetworkPaths? = null
        private set

    /** Takes the way out as it is now. A change, the first word about it included, dials every address again. */
    fun follow(now: NetworkPaths) {
        if (now == paths) return
        paths = now
        retry()
    }

    /** Lets every address of every machine be dialled again on its next call, whatever it did before. */
    fun retry() {
        selectors().forEach { it.liftBackoff() }
    }
}

/** 100.64.0.0/10, the range every tailnet address is in. */
val TAILNET: LanPrefix = LanPrefix(LanPrefix.parseIPv4("100.64.0.0")!!, 10, LanPrefix.Form.CIDR)

/**
 * Whether [failure] on [endpoint] is this phone having no tunnel there, rather than a machine that
 * did not answer.
 *
 * Only for a remote address on the tailnet, an address in [TAILNET] or a `*.ts.net` name, where
 * nothing answered and nothing was sent, and only while [tunnels], the prefixes this phone's tunnels
 * carry, are known and cover none of it: an address no tunnel routes leaves the phone by Wi-Fi or
 * mobile data, where nothing that has it can be. A name is not resolved here; a tunnel that routes
 * any of the tailnet's range might carry it. Null [tunnels] means nothing has been said about them
 * yet, and nothing is blamed on them.
 */
fun lacksTunnel(endpoint: Endpoint, failure: AgentFailure, tunnels: List<LanPrefix>?): Boolean {
    if (tunnels == null || endpoint.kind != RouteKind.REMOTE) return false
    if (failure.dispatch != DispatchStage.NOT_STARTED || failure.kind !in UNANSWERED) return false
    val address = LanPrefix.parseIPv4(endpoint.host)
    return when {
        address != null -> TAILNET.contains(address) && tunnels.none { it.contains(address) }
        endpoint.host.endsWith(".ts.net", ignoreCase = true) -> tunnels.none { it.overlaps(TAILNET) }
        else -> false
    }
}

/** [failure], or a [AgentFailure.NoTunnel] in its place when [lacksTunnel] says that is what it was. */
fun untunnelled(endpoint: Endpoint, failure: AgentFailure, tunnels: List<LanPrefix>?): AgentFailure {
    if (!lacksTunnel(endpoint, failure, tunnels)) return failure
    val said = failure.detail?.trim()?.let { if (it.endsWith(".")) "$it " else "$it. " }.orEmpty()
    return AgentFailure.NoTunnel(
        "${said}No tunnel on this phone carries ${endpoint.host}, which is a tailnet address.",
        failure,
        failure.kind ?: FailureKind.HOST_UNREACHABLE,
    )
}

/**
 * How an address that never got as far as ssh is read. With no tunnel it can be any of them: no
 * answer in time, no route, or a network that turns the address away at once.
 */
private val UNANSWERED = setOf(FailureKind.HOST_UNREACHABLE, FailureKind.CONNECTION_REFUSED, FailureKind.NO_ROUTE)
