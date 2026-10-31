package com.x1f4r.legioncontrol.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.RouteInfo
import android.os.Build
import androidx.core.content.getSystemService
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.flow.distinctUntilChanged
import java.net.Inet4Address

/**
 * The addresses this device could send a wake packet from, as the networks come and go (contract
 * section 10.1).
 *
 * Answered from every attached network and not from the default one: with a tunnel up the default
 * network is the tunnel, its only addresses are tunnel addresses, and asking it alone said "not at
 * home" while the phone sat on the home Wi-Fi. It used to follow only the default network, and to
 * miss a network that changed what it could carry, so a Wi-Fi that dropped while a VPN stayed up
 * left its address behind as if the phone were still there.
 *
 * Deliberately not a Wi-Fi name check: reading the SSID needs the location permission, would be
 * wrong on ethernet, and would still say yes on a home network the phone reaches through a VPN that
 * swallows broadcasts. Sharing a subnet with the target is the actual precondition for a broadcast
 * reaching it, and each address keeps the network it came from so the packet leaves by that one.
 *
 * Beside that it follows the way out of the phone, tunnels included ([observePaths]), which is what
 * says that a tailnet address is silent because no VPN is up rather than because its machine is off.
 */
class HomeNetwork(context: Context) {
    private val appContext = context.applicationContext
    private val connectivity = appContext.getSystemService<ConnectivityManager>()

    /**
     * Every candidate address, as a stream that follows the phone out of the house.
     *
     * Every non-VPN network is followed with its own entry in a [NetworkAddressBook]: its addresses
     * when its link changes, whether it can carry a broadcast when its capabilities change, whether
     * the system blocks it for this app, and its going away. The list is what the book holds after
     * each of those.
     */
    fun observeAddresses(): Flow<List<LocalAddress>> = callbackFlow {
        val manager = connectivity ?: run {
            trySend(emptyList())
            awaitClose { }
            return@callbackFlow
        }
        val book = NetworkAddressBook()
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
                book.put(network.networkHandle, network, linkProperties.ipv4WithLength())
                trySend(book.snapshot())
            }

            override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
                book.capabilities(network.networkHandle, carriesBroadcast = capabilities.carriesBroadcast())
                trySend(book.snapshot())
            }

            override fun onBlockedStatusChanged(network: Network, blocked: Boolean) {
                book.blocked(network.networkHandle, blocked)
                trySend(book.snapshot())
            }

            override fun onLost(network: Network) {
                book.remove(network.networkHandle)
                trySend(book.snapshot())
            }
        }
        trySend(book.snapshot())
        manager.registerNetworkCallback(
            NetworkRequest.Builder().addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN).build(),
            callback,
        )
        awaitClose { runCatching { manager.unregisterNetworkCallback(callback) } }
    }.distinctUntilChanged().conflate()

    /**
     * The way out of this phone, as it changes: the network its traffic takes by default, and every
     * tunnel this app's traffic can use with the IPv4 prefixes it routes.
     *
     * Followed apart from [observeAddresses], which leaves every VPN out for the wake's sake. A
     * second callback asks for VPNs alone, and keeps each one's routes as its link changes; the
     * request is made as this app, so a tunnel that leaves the app out is never seen. The network in
     * force is read once at the start, so a tunnel that is already up is known before its callbacks
     * arrive. With no connectivity service nothing is said at all, and nothing is then put down to a
     * missing tunnel.
     */
    fun observePaths(): Flow<NetworkPaths> = callbackFlow {
        val manager = connectivity ?: run {
            awaitClose { }
            return@callbackFlow
        }
        val book = NetworkPathBook()
        manager.activeNetwork?.let { network ->
            book.defaultNetwork(network.networkHandle)
            if (manager.getNetworkCapabilities(network)?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true) {
                manager.getLinkProperties(network)?.let { book.tunnel(network.networkHandle, it.carriedPrefixes()) }
            }
        }
        trySend(book.snapshot())
        val defaults = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                book.defaultNetwork(network.networkHandle)
                trySend(book.snapshot())
            }

            override fun onLost(network: Network) {
                book.defaultLost(network.networkHandle)
                trySend(book.snapshot())
            }
        }
        val tunnels = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
                book.tunnel(network.networkHandle, linkProperties.carriedPrefixes())
                trySend(book.snapshot())
            }

            override fun onBlockedStatusChanged(network: Network, blocked: Boolean) {
                book.blocked(network.networkHandle, blocked)
                trySend(book.snapshot())
            }

            override fun onLost(network: Network) {
                book.remove(network.networkHandle)
                trySend(book.snapshot())
            }
        }
        manager.registerDefaultNetworkCallback(defaults)
        manager.registerNetworkCallback(
            NetworkRequest.Builder()
                .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
                .build(),
            tunnels,
        )
        awaitClose {
            runCatching { manager.unregisterNetworkCallback(defaults) }
            runCatching { manager.unregisterNetworkCallback(tunnels) }
        }
    }.distinctUntilChanged().conflate()

    /**
     * The IPv4 prefixes a tunnel routes: its unicast routes, and none it throws back to the
     * network underneath or refuses. A VPN can exclude a route only from Android 13, which is also
     * where a route's type can first be read; every route before that is unicast.
     */
    private fun LinkProperties.carriedPrefixes(): List<LanPrefix> = routes.mapNotNull { route ->
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && route.type != RouteInfo.RTN_UNICAST) {
            return@mapNotNull null
        }
        val destination = route.destination
        val address = (destination.address as? Inet4Address)?.hostAddress ?: return@mapNotNull null
        LanPrefix.parseIPv4(address)?.let { LanPrefix(it, destination.prefixLength, LanPrefix.Form.CIDR) }
    }

    /** The IPv4 addresses of a link, each with the length of its network. */
    private fun LinkProperties.ipv4WithLength(): List<Pair<String, Int?>> =
        linkAddresses.mapNotNull { link ->
            val address = (link.address as? Inet4Address)?.hostAddress ?: return@mapNotNull null
            address to link.prefixLength
        }

    /**
     * Whether a magic packet sent on this network can reach anything: never through a VPN, which
     * is a tunnel with no broadcast domain, and never on mobile data, which is a point-to-point link
     * to the carrier. An address on either could still look like a home prefix.
     */
    private fun NetworkCapabilities.carriesBroadcast(): Boolean =
        !hasTransport(NetworkCapabilities.TRANSPORT_VPN) && !hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR)
}

/**
 * What each followed network contributes, kept per network so that one going away takes exactly its
 * own addresses with it.
 *
 * Pure Kotlin on purpose, so the bookkeeping that decides where this phone is can be tested without
 * a device: networks are keyed by their handle, and [Network] itself is only carried through.
 */
class NetworkAddressBook {
    private data class Entry(
        val network: Network? = null,
        val addresses: List<Pair<String, Int?>> = emptyList(),
        /** Null until the system has said what the network can carry. */
        val carriesBroadcast: Boolean? = null,
        val blocked: Boolean = false,
    )

    private val entries = LinkedHashMap<Long, Entry>()

    /** The IPv4 addresses of [handle] now, each with its prefix length. */
    @Synchronized
    fun put(handle: Long, network: Network?, addresses: List<Pair<String, Int?>>) {
        entries[handle] = (entries[handle] ?: Entry()).copy(network = network, addresses = addresses)
    }

    /** Whether [handle] can carry a broadcast: false for a VPN and for mobile data. */
    @Synchronized
    fun capabilities(handle: Long, carriesBroadcast: Boolean) {
        entries[handle] = (entries[handle] ?: Entry()).copy(carriesBroadcast = carriesBroadcast)
    }

    /** Whether the system blocks [handle] for this app. A blocked network contributes nothing. */
    @Synchronized
    fun blocked(handle: Long, blocked: Boolean) {
        entries[handle] = (entries[handle] ?: Entry()).copy(blocked = blocked)
    }

    @Synchronized
    fun remove(handle: Long) {
        entries.remove(handle)
    }

    /** Every candidate address, by address, each once. */
    @Synchronized
    fun snapshot(): List<LocalAddress> = entries.values
        .filter { !it.blocked && it.carriesBroadcast != false }
        .flatMap { entry -> entry.addresses.map { (address, length) -> LocalAddress(address, length, entry.network) } }
        .distinctBy { it.address }
        .sortedWith(compareBy<LocalAddress> { it.value ?: Long.MAX_VALUE }.thenBy { it.address })
}
