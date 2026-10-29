package com.x1f4r.legioncontrol.net

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Which addresses this phone could send a wake packet from, as networks come, change and go
 * (contract section 10.1).
 *
 * The callbacks arrive per network, in any interleaving, and the book is what turns them into the
 * list every placement reads. It used to follow the default network alone, which under a VPN is the
 * tunnel, and to keep an address after the network that held it had gone.
 */
class NetworkAddressBookTest {

    private val wifi = 101L
    private val vpn = 102L
    private val ethernet = 103L

    private fun addresses(book: NetworkAddressBook) = book.snapshot().map { it.address }

    @Test
    fun `wifi lost while a vpn stays up takes the wifi address with it`() {
        val book = NetworkAddressBook()
        book.capabilities(wifi, carriesBroadcast = true)
        book.put(wifi, null, listOf("192.168.1.23" to 24))
        book.capabilities(vpn, carriesBroadcast = false)
        book.put(vpn, null, listOf("100.64.0.9" to 32))
        assertEquals("a tunnel carries no broadcast, whatever its address looks like", listOf("192.168.1.23"), addresses(book))

        book.remove(wifi)
        assertEquals(emptyList<String>(), addresses(book))
    }

    @Test
    fun `a network the system blocks for this app contributes nothing until it is unblocked`() {
        val book = NetworkAddressBook()
        book.capabilities(wifi, carriesBroadcast = true)
        book.blocked(wifi, true)
        book.put(wifi, null, listOf("192.168.1.23" to 24))
        assertEquals(emptyList<String>(), addresses(book))

        book.blocked(wifi, false)
        assertEquals(listOf("192.168.1.23"), addresses(book))
    }

    @Test
    fun `a network that stops carrying a broadcast stops counting, and its link changes are kept`() {
        val book = NetworkAddressBook()
        book.put(wifi, null, listOf("192.168.1.23" to 24))
        assertEquals("nothing said yet about what it carries", listOf("192.168.1.23"), addresses(book))

        book.capabilities(wifi, carriesBroadcast = false)
        assertEquals(emptyList<String>(), addresses(book))
        book.put(wifi, null, listOf("192.168.1.24" to 24))
        assertEquals("a new link on it changes nothing while it carries no broadcast", emptyList<String>(), addresses(book))

        book.capabilities(wifi, carriesBroadcast = true)
        assertEquals(listOf("192.168.1.24"), addresses(book))
    }

    @Test
    fun `addresses come out in address order, each once, with their prefix lengths`() {
        val book = NetworkAddressBook()
        book.put(wifi, null, listOf("192.168.10.7" to 24, "192.168.1.23" to 24))
        book.put(ethernet, null, listOf("10.4.0.2" to 30, "192.168.1.23" to 24))
        val snapshot = book.snapshot()
        assertEquals(listOf("10.4.0.2", "192.168.1.23", "192.168.10.7"), snapshot.map { it.address })
        assertEquals(listOf(30, 24, 24), snapshot.map { it.prefixLength })
    }
}
