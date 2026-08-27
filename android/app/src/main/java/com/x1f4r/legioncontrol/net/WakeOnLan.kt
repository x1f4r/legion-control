package com.x1f4r.legioncontrol.net

import android.net.Network
import com.x1f4r.legioncontrol.agent.WakeTarget
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds
import kotlin.time.TimeSource

/**
 * Wake-on-LAN for one machine's network card.
 *
 * This is LAN only and there is no way around that: a magic packet is a link local broadcast, and a
 * tunnel is a point to point link with no broadcast domain to put one on. The app is expected to
 * check [HomeNetwork] first and disable the action with a reason when the phone is elsewhere, rather
 * than offering a button that sends packets into nothing.
 */
class WakeOnLan(private val target: WakeTarget) {

    /**
     * Sends the magic packets. Returns null when at least one went out, or a sentence if none did.
     *
     * [via] is the network the packet has to leave by, from [HomeNetwork.homeNetwork]. Binding to it
     * matters whenever the default route is not the home network, which is any time a tunnel is up:
     * an unbound broadcast would go into the tunnel, be dropped there, and the app would sit waiting
     * for a machine that was never asked to wake.
     *
     * Every configured broadcast address is used. The all ones address is deliberately not assumed
     * on the machine's behalf: Android drops it on some interfaces, and the directed form is what
     * actually reaches a switch port, so which addresses to send to is the configuration's business.
     */
    suspend fun send(via: Network? = null): String? = withContext(Dispatchers.IO) {
        val payload = magicPacket(target.mac)
            ?: return@withContext "${target.mac} is not a valid hardware address."
        if (target.broadcasts.isEmpty()) {
            return@withContext "No broadcast address is configured for this machine."
        }

        try {
            DatagramSocket().use { socket ->
                // Best effort: an always-on VPN set to block other traffic can refuse this, and then
                // the default route is the only thing left to try.
                via?.let { runCatching { it.bindSocket(socket) } }
                socket.broadcast = true
                var delivered = 0
                var lastFailure: String? = null
                for (address in target.broadcasts) {
                    val destination = try {
                        InetAddress.getByName(address)
                    } catch (failure: Exception) {
                        lastFailure = "$address: ${failure.message ?: failure::class.java.simpleName}"
                        continue
                    }
                    for (port in target.ports) {
                        // Wi-Fi drops UDP without telling anyone, and the packet is 102 bytes, so
                        // repeat it.
                        repeat(REPEATS) {
                            try {
                                socket.send(DatagramPacket(payload, payload.size, destination, port))
                                delivered++
                            } catch (failure: Exception) {
                                lastFailure = "$address port $port: " +
                                    (failure.message ?: failure::class.java.simpleName)
                            }
                        }
                    }
                }
                if (delivered > 0) null else lastFailure ?: "No magic packet could be sent."
            }
        } catch (failure: Exception) {
            "Could not open a UDP socket: ${failure.message ?: failure::class.java.simpleName}."
        }
    }

    /**
     * Polls the configured probe address until the machine answers or [timeout] runs out.
     *
     * The LAN address rather than a tunnel address, and not by accident: a machine that has just come
     * out of sleep answers sshd well before a tunnel daemon has re-registered, so watching a remote
     * address would report a failure for a machine that is already up.
     */
    suspend fun waitForSsh(timeout: Duration = 45.seconds): Boolean {
        if (target.probeHost.isBlank()) return true
        val started = TimeSource.Monotonic.markNow()
        while (started.elapsedNow() < timeout && currentCoroutineContext().isActive) {
            if (TcpProbe.reachable(target.probeHost, target.probePort, POLL_TIMEOUT_MILLIS)) return true
            delay(1.seconds)
        }
        return false
    }

    /** Where the wait is watching, so a failure can say which address never answered. */
    val probeAddress: String get() = "${target.probeHost}:${target.probePort}"

    private companion object {
        const val REPEATS = 3
        const val POLL_TIMEOUT_MILLIS = 2_000
    }
}

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
