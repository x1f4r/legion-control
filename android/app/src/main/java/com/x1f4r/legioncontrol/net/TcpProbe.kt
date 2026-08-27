package com.x1f4r.legioncontrol.net

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.InetSocketAddress
import java.net.Socket

/**
 * Opens a TCP connection and drops it again.
 *
 * This exists so that picking a route does not cost an ssh handshake. A full handshake against an
 * address that is not there costs the connect timeout plus key exchange, and we may have three
 * addresses to try; a bare connect answers the only question route selection actually asks, which is
 * whether something is listening.
 */
object TcpProbe {
    suspend fun reachable(host: String, port: Int, timeoutMillis: Int): Boolean =
        withContext(Dispatchers.IO) {
            try {
                Socket().use { socket ->
                    socket.connect(InetSocketAddress(host, port), timeoutMillis)
                    true
                }
            } catch (_: Exception) {
                // Refused, timed out, no route, no DNS: all of them mean the same thing here.
                false
            }
        }
}
