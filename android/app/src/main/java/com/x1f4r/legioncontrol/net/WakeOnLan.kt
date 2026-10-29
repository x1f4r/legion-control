package com.x1f4r.legioncontrol.net

import android.net.Network
import com.x1f4r.legioncontrol.agent.WAKE_REPEATS
import com.x1f4r.legioncontrol.agent.WakeTarget
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/** What one send did: how many magic packets left this device, and why none did when none did. */
data class WakeSend(val sent: Int, val problem: String?)

/**
 * Wake-on-LAN for one machine's network card.
 *
 * This is LAN only and there is no way around that: a magic packet is a link local broadcast, and a
 * tunnel is a point to point link with no broadcast domain to put one on. Whether this device is on
 * the machine's network, and which addresses and ports to send to, is the wake plan's to decide
 * (contract section 10.2); this only sends what it is given.
 */
class WakeOnLan(private val target: WakeTarget) {

    /**
     * Sends [WAKE_REPEATS] magic packets to every one of [broadcasts] on every one of [ports].
     *
     * [via] is the network that holds the address that placed the machine on this device's network.
     * Binding to it matters whenever the default route is not that network, which is any time a
     * tunnel is up: an unbound broadcast would go into the tunnel, be dropped there, and the app
     * would sit waiting for a machine that was never asked to wake.
     *
     * Every entry is a dotted IPv4 address by the time it gets here, and it is turned into one
     * without a lookup: a broadcast entry is never resolved as a name.
     */
    suspend fun send(via: Network?, broadcasts: List<String>, ports: List<Int>): WakeSend = withContext(Dispatchers.IO) {
        val payload = magicPacket(target.mac)
            ?: return@withContext WakeSend(0, "${target.mac} is not a valid hardware address.")
        sendPackets(payload, broadcasts, ports) { socket -> via?.bindSocket(socket) }
    }
}

/**
 * Sends [payload] [WAKE_REPEATS] times to every address and port, from one socket with
 * `SO_BROADCAST` that [bind] may first tie to a network.
 */
internal fun sendPackets(
    payload: ByteArray,
    broadcasts: List<String>,
    ports: List<Int>,
    bind: (DatagramSocket) -> Unit = {},
): WakeSend {
    val destinations = broadcasts.mapNotNull { address -> LanPrefix.parseIPv4(address)?.let { address to it } }
    if (destinations.isEmpty() || ports.isEmpty()) return WakeSend(0, "There is no address to send a wake packet to.")
    return try {
        DatagramSocket().use { socket ->
            // Best effort: an always-on VPN set to block other traffic can refuse this, and then
            // the default route is the only thing left to try.
            runCatching { bind(socket) }
            socket.broadcast = true
            var sent = 0
            var lastFailure: String? = null
            for ((text, address) in destinations) {
                val destination = InetAddress.getByAddress(addressBytes(address))
                for (port in ports) {
                    // Wi-Fi drops UDP without telling anyone, and the packet is 102 bytes, so
                    // repeat it.
                    repeat(WAKE_REPEATS) {
                        try {
                            socket.send(DatagramPacket(payload, payload.size, destination, port))
                            sent++
                        } catch (failure: Exception) {
                            lastFailure = "$text port $port: " + (failure.message ?: failure::class.java.simpleName)
                        }
                    }
                }
            }
            WakeSend(sent, if (sent > 0) null else lastFailure ?: "No magic packet could be sent.")
        }
    } catch (failure: Exception) {
        WakeSend(0, "Could not open a UDP socket: ${failure.message ?: failure::class.java.simpleName}.")
    }
}

/** An address as [LanPrefix.parseIPv4] gives it, as the four bytes a socket takes. */
private fun addressBytes(address: Long): ByteArray =
    byteArrayOf((address shr 24).toByte(), (address shr 16).toByte(), (address shr 8).toByte(), address.toByte())

/**
 * Six 0xFF bytes then the hardware address sixteen times over, or null when that is not a hardware
 * address at all.
 */
internal fun magicPacket(mac: String): ByteArray? {
    val bytes = parseMacAddress(mac) ?: return null
    val packet = ByteArray(6 + 16 * 6)
    for (index in 0 until 6) packet[index] = 0xFF.toByte()
    for (repeat in 0 until 16) {
        for (index in 0 until 6) packet[6 + repeat * 6 + index] = bytes[index]
    }
    return packet
}

/**
 * Six hex pairs, separated by colons or dashes, or null.
 *
 * Shared with the configuration check, so that a hardware address the app would refuse to build a
 * packet from is refused at the moment the configuration is applied rather than at the moment the
 * user presses Wake on a machine that will not come back.
 */
internal fun parseMacAddress(mac: String): ByteArray? {
    val parts = mac.trim().split(':', '-')
    if (parts.size != 6) return null
    val bytes = ByteArray(6)
    for ((index, part) in parts.withIndex()) {
        val value = part.trim().takeIf { it.length in 1..2 }?.toIntOrNull(16) ?: return null
        if (value !in 0..255) return null
        bytes[index] = value.toByte()
    }
    return bytes
}
