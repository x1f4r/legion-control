package com.x1f4r.legioncontrol.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

/** A magic packet is six 0xFF bytes and then the hardware address sixteen times over, or nothing. */
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
    fun `anything that is not six hex pairs is refused rather than padded`() {
        assertNull(magicPacket(""))
        assertNull(magicPacket("aa:bb:cc:dd:ee"))
        assertNull(magicPacket("aa:bb:cc:dd:ee:ff:00"))
        assertNull(magicPacket("XX:XX:XX:XX:XX:XX"))
        assertNull(magicPacket("aa:bb:cc:dd:ee:fff"))
        assertNull(magicPacket("192.0.2.1"))
    }
}
