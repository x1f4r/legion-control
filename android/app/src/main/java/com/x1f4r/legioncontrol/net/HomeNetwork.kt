package com.x1f4r.legioncontrol.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import androidx.core.content.getSystemService
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.flow.distinctUntilChanged
import java.net.Inet4Address

/** Whether the phone is on the network a wake-on-LAN broadcast can cross, and why not when it is not. */
data class HomeNetworkState(
    val onHomeNetwork: Boolean,
    /** The phone's own address on that network, when it has one. Useful when this goes wrong. */
    val localAddress: String?,
    /** One sentence for the UI. Empty when the answer is yes and there is nothing to explain. */
    val explanation: String,
)

/**
 * Answers the one network question the UI cannot fake: is wake-on-LAN possible right now.
 *
 * It is answered by looking at the addresses of the attached networks and checking whether any of
 * them starts with the prefix the machine's configuration gave for its own network. That is
 * deliberately not a Wi-Fi name check: reading the SSID needs the location permission, would be
 * wrong on ethernet, and would still say yes on a home network the phone is attached to through a
 * VPN that swallows broadcasts. Sharing a subnet with the target is the actual precondition for a
 * broadcast reaching it.
 *
 * The prefix is a parameter rather than a constant because it belongs to a machine: two machines in
 * the configuration can sit on two different networks, and each of them is at home somewhere else.
 */
class HomeNetwork(context: Context) {
    private val appContext = context.applicationContext
    private val connectivity = appContext.getSystemService<ConnectivityManager>()

    fun current(lanPrefix: String?): HomeNetworkState {
        if (lanPrefix.isNullOrBlank()) return noPrefix()
        val manager = connectivity ?: return HomeNetworkState(
            onHomeNetwork = false,
            localAddress = null,
            explanation = "Android would not say what network this phone is on.",
        )
        val network = manager.activeNetwork ?: return offNetwork()
        return evaluate(manager, network, lanPrefix)
    }

    /**
     * Every IPv4 address this device holds, on every attached network.
     *
     * Every attached network and not only the default one: with a tunnel up the default network's
     * only addresses are tunnel addresses, so asking it alone would say "not at home" while the
     * phone is sitting on the home Wi-Fi.
     */
    @Suppress("DEPRECATION")
    fun addresses(): List<String> = runCatching {
        val manager = connectivity ?: return emptyList()
        manager.allNetworks.flatMap { network ->
            manager.getLinkProperties(network)?.linkAddresses.orEmpty()
                .mapNotNull { it.address as? Inet4Address }
                .mapNotNull { it.hostAddress }
        }.distinct()
    }.getOrDefault(emptyList())

    /** The same, as a stream, so the site line follows the phone out of the house. */
    fun observeAddresses(): Flow<List<String>> = callbackFlow {
        val manager = connectivity
        if (manager == null) {
            trySend(emptyList())
            awaitClose { }
            return@callbackFlow
        }
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                trySend(addresses())
            }

            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
                trySend(addresses())
            }

            override fun onLost(network: Network) {
                trySend(addresses())
            }
        }
        trySend(addresses())
        manager.registerDefaultNetworkCallback(callback)
        awaitClose { runCatching { manager.unregisterNetworkCallback(callback) } }
    }.distinctUntilChanged().conflate()

    /**
     * The network a magic packet has to leave by for one of a set of prefixes.
     *
     * The site version of [homeNetwork]: a site can name several prefixes and the packet has to go
     * out of whichever interface actually holds one of them.
     */
    fun networkForPrefixes(prefixes: List<String>): Network? {
        val manager = connectivity ?: return null
        return prefixes.firstNotNullOfOrNull { prefix ->
            prefix.takeIf { it.isNotBlank() }?.let { homeAddress(manager, it)?.second }
        }
    }

    /**
     * The interface a magic packet has to leave by, when the phone has one.
     *
     * Handed to [WakeOnLan] so the socket is bound to the home network rather than to whatever is
     * currently the default route. With a tunnel up the default is the tunnel, and a broadcast put
     * on that goes nowhere at all.
     */
    fun homeNetwork(lanPrefix: String?): Network? {
        if (lanPrefix.isNullOrBlank()) return null
        return connectivity?.let { homeAddress(it, lanPrefix)?.second }
    }

    /** The same answer, as a stream, so a screen can enable and disable the wake action live. */
    fun observe(lanPrefix: String?): Flow<HomeNetworkState> = callbackFlow {
        val manager = connectivity
        if (manager == null || lanPrefix.isNullOrBlank()) {
            trySend(current(lanPrefix))
            awaitClose { }
            return@callbackFlow
        }

        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                trySend(evaluate(manager, network, lanPrefix))
            }

            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
                trySend(evaluate(manager, network, lanPrefix))
            }

            override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
                trySend(evaluate(manager, network, lanPrefix))
            }

            override fun onLost(network: Network) {
                trySend(current(lanPrefix))
            }
        }

        trySend(current(lanPrefix))
        manager.registerDefaultNetworkCallback(callback)
        awaitClose { runCatching { manager.unregisterNetworkCallback(callback) } }
    }.distinctUntilChanged().conflate()

    private fun evaluate(
        manager: ConnectivityManager,
        network: Network,
        lanPrefix: String,
    ): HomeNetworkState {
        // Every attached network, not only the default one. With a tunnel up the default network is
        // the tunnel and its only addresses are tunnel addresses, so asking it alone would say "not
        // at home" while the phone is sitting on the home Wi-Fi, and the wake action would be greyed
        // out with a reason that is not true.
        homeAddress(manager, lanPrefix)?.let { (address, _) ->
            return HomeNetworkState(onHomeNetwork = true, localAddress = address, explanation = "")
        }

        val capabilities = manager.getNetworkCapabilities(network) ?: return offNetwork()

        val addresses = manager.getLinkProperties(network)?.linkAddresses.orEmpty()
            .mapNotNull { it.address as? Inet4Address }
            .map { it.hostAddress.orEmpty() }
            .filter { it.isNotEmpty() }

        val kind = when {
            capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "another Wi-Fi network"
            capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "mobile data"
            capabilities.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "a wired network"
            capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN) -> "a VPN"
            else -> "a network that is not the machine's own"
        }
        return HomeNetworkState(
            onHomeNetwork = false,
            localAddress = addresses.firstOrNull(),
            explanation = "Waking needs the machine's own network. This phone is on $kind, and the " +
                "magic packet is a LAN broadcast, which a tunnel cannot carry.",
        )
    }

    /** The first attached network holding an address in the machine's subnet, and that address. */
    @Suppress("DEPRECATION")
    private fun homeAddress(manager: ConnectivityManager, lanPrefix: String): Pair<String, Network>? =
        runCatching {
            manager.allNetworks.firstNotNullOfOrNull { candidate ->
                manager.getLinkProperties(candidate)?.linkAddresses.orEmpty()
                    .mapNotNull { it.address as? Inet4Address }
                    .mapNotNull { it.hostAddress }
                    .firstOrNull { it.startsWith(lanPrefix) }
                    ?.let { it to candidate }
            }
        }.getOrNull()

    private fun offNetwork() = HomeNetworkState(
        onHomeNetwork = false,
        localAddress = null,
        explanation = "This phone is not on a network.",
    )

    /**
     * A machine whose configuration does not say which network it is on. Waking is still offered
     * only where it can work, so this reads as "not there" rather than as an error: the answer is
     * the same, and there is nothing the user can do about it from this screen.
     */
    private fun noPrefix() = HomeNetworkState(
        onHomeNetwork = false,
        localAddress = null,
        explanation = "This machine's configuration does not say which network it is on, so the " +
            "app cannot tell whether a wake packet would reach it.",
    )
}
