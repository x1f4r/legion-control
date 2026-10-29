package com.x1f4r.legioncontrol.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.SocketTimeoutException

/**
 * A magic packet is six 0xFF bytes and then the hardware address sixteen times over, or nothing, and
 * it goes three times to every address and port the wake plan names (contract section 10.2).
 */
class WakeOnLanTest {

    @Test
    fun `the packet is 102 bytes of header and repetition`() {
        val packet = magicPacket("aa:bb:cc:dd:ee:ff")
        assertNotNull(packet)
        assertEquals(6 + 16 * 6, packet!!.size)
        for (index in 0 until 6) assertEquals(0xFF.toByte(), packet[index])

        val address = byteArrayOf(
            0xAA.toByte(), 0xBB.toByte(), 0xCC.toByte(),
            0xDD.toByte(), 0xEE.toByte(), 0xFF.toByte(),
        )
        for (repeat in 0 until 16) {
            for (index in 0 until 6) {
                assertEquals(address[index], packet[6 + repeat * 6 + index])
            }
        }
    }

    @Test
    fun `dashes and single digit pairs are hardware addresses too`() {
        assertNotNull(magicPacket("AA-BB-CC-DD-EE-FF"))
        assertNotNull(magicPacket("0:1:2:3:4:5"))
        assertEquals(0x01.toByte(), parseMacAddress("0:1:2:3:4:5")!![1])
    }

    @Test
    fun `three packets go to every address and port, and a name is never looked up`() {
        // Two loopback listeners stand in for two broadcast addresses' ports. The name in the list
        // is skipped rather than resolved, because a broadcast entry is an address or nothing.
        DatagramSocket(0, InetAddress.getByName("127.0.0.1")).use { first ->
            DatagramSocket(0, InetAddress.getByName("127.0.0.1")).use { second ->
                first.soTimeout = 2_000
                second.soTimeout = 2_000
                val payload = magicPacket("02:00:00:00:00:01")!!
                val send = sendPackets(payload, listOf("127.0.0.1", "tower.local"), listOf(first.localPort, second.localPort))

                assertEquals(WakeSend(sent = 6, problem = null), send)
                assertEquals(3, received(first))
                assertEquals(3, received(second))
            }
        }
    }

    @Test
    fun `with nothing to send to nothing is sent`() {
        val payload = magicPacket("02:00:00:00:00:01")!!
        assertEquals(0, sendPackets(payload, listOf("tower.local"), listOf(9)).sent)
        assertEquals(0, sendPackets(payload, listOf("127.0.0.1"), emptyList()).sent)
    }

    /** How many magic packets arrived on [socket] before it fell quiet. */
    private fun received(socket: DatagramSocket): Int {
        var count = 0
        val buffer = ByteArray(256)
        while (true) {
            val packet = DatagramPacket(buffer, buffer.size)
            try {
                socket.receive(packet)
            } catch (_: SocketTimeoutException) {
                return count
            }
            assertEquals(102, packet.length)
            count++
            socket.soTimeout = 300
        }
    }

    @Test
    fun `anything that is not six hex pairs is refused rather than padded`() {
        assertNull(magicPacket(""))
        assertNull(magicPacket("aa:bb:cc:dd:ee"))
        assertNull(magicPacket("aa:bb:cc:dd:ee:ff:00"))
        assertNull(magicPacket("XX:XX:XX:XX:XX:XX"))
        assertNull(magicPacket("aa:bb:cc:dd:ee:fff"))
        assertNull(magicPacket("192.0.2.1"))
    }
}
