package com.x1f4r.legioncontrol.net

import android.net.Network

/**
 * What a LAN prefix is, what it contains, and what a broadcast address is (contract section 10.1).
 *
 * Every client used to match a prefix with "starts with" on the text, so `192.168.1` also matched an
 * address on 192.168.10.x and a phone two subnets away believed it was at home. A prefix is parsed
 * into a network and a length here and matched on octet or bit boundaries, and anything that does
 * not parse never matches anything.
 *
 * A port of `parseIPv4`, `parseLanPrefix`, `prefixContains` and `prefixesOverlap` in
 * `contract/tools/wake-plan.mjs`. Addresses are unsigned 32-bit numbers, held in a Long so that
 * 255.255.255.255 is not negative. Nothing here resolves a name.
 */
data class LanPrefix(
    /** The network address, with no host bits set. */
    val network: Long,
    /** The length in bits: 8, 16 or 24 for a dotted prefix, 8 to 32 for CIDR. */
    val length: Int,
    val form: Form,
) {
    enum class Form { DOTTED, CIDR }

    /** Whether [address], as [parseIPv4] gives it, is inside this prefix. */
    fun contains(address: Long): Boolean {
        val mask = maskOf(length)
        return address and mask == network and mask
    }

    /** Whether two prefixes share any address: the shorter one contains the other's network. */
    fun overlaps(other: LanPrefix): Boolean {
        val mask = maskOf(minOf(length, other.length))
        return network and mask == other.network and mask
    }

    /**
     * How an editor writes this prefix (contract section 10.1): an octet-aligned one as dotted
     * octets with the trailing dot, because a client that predates the section compares text with
     * "starts with" and is right only when the prefix ends in a dot; any other length as CIDR.
     */
    val normalised: String
        get() = if (length % 8 == 0 && length <= 24) {
            formatIPv4(network).split('.').take(length / 8).joinToString(".", postfix = ".")
        } else {
            "${formatIPv4(network)}/$length"
        }

    companion object {
        private val OCTET = Regex("^(?:0|[1-9][0-9]{0,2})$")

        /** A CIDR length from 8 to 32, written without leading zeros. */
        private val LENGTH = Regex("^(?:[89]|[12][0-9]|3[0-2])$")

        /**
         * A dotted-quad IPv4 address, or null. Exactly four decimal octets from 0 to 255 without
         * leading zeros: `192.168.01.1` is octal to some parsers and decimal to others, so it is
         * neither.
         */
        fun parseIPv4(text: String?): Long? {
            val parts = text?.split('.') ?: return null
            if (parts.size != 4) return null
            var value = 0L
            for (part in parts) value = value * 256 + (octet(part) ?: return null)
            return value
        }

        /**
         * A `sites[].lanPrefixes` entry or a `wake.lanPrefix`, or null.
         *
         * Dotted: one to three octets, optionally followed by a single dot, eight bits each, so
         * `192.168.1` and `192.168.1.` are the same /24 and neither contains 192.168.10.7. CIDR: a
         * dotted-quad network, `/`, and a length from 8 to 32 with no host bits set, so
         * `192.168.1.1/24` names an address rather than a network and is refused instead of being
         * widened. Anything else, a full address without a length, IPv6, a wildcard, surrounding
         * whitespace, is not a prefix.
         */
        fun parse(text: String?): LanPrefix? {
            if (text.isNullOrEmpty()) return null
            val slash = text.indexOf('/')
            if (slash >= 0) {
                val network = parseIPv4(text.substring(0, slash)) ?: return null
                val lengthText = text.substring(slash + 1)
                if (!LENGTH.matches(lengthText)) return null
                val length = lengthText.toInt()
                if (network and maskOf(length).inv() and ALL_BITS != 0L) return null
                return LanPrefix(network, length, Form.CIDR)
            }
            val body = if (text.endsWith('.')) text.dropLast(1) else text
            val parts = body.split('.')
            if (parts.size !in 1..3) return null
            var network = 0L
            for (part in parts) network = network * 256 + (octet(part) ?: return null)
            return LanPrefix(network shl (8 * (4 - parts.size)), 8 * parts.size, Form.DOTTED)
        }

        /** An address as [parseIPv4] gives it, as dotted-quad text. */
        fun formatIPv4(address: Long): String = listOf(24, 16, 8, 0).joinToString(".") { shift ->
            ((address shr shift) and 0xFF).toString()
        }

        /**
         * The directed broadcast of [address] on a network of [length] bits: every host bit set.
         * Null for a /31 or /32, which has no broadcast address of its own.
         */
        fun directedBroadcast(address: Long, length: Int): Long? =
            if (length in 0..30) (address or maskOf(length).inv()) and ALL_BITS else null

        private fun octet(text: String): Long? {
            if (!OCTET.matches(text)) return null
            return text.toLong().takeIf { it <= 255 }
        }

        private const val ALL_BITS = 0xFFFF_FFFFL

        /** The mask of the first [length] bits. */
        private fun maskOf(length: Int): Long = if (length == 0) 0L else (ALL_BITS shl (32 - length)) and ALL_BITS
    }
}

/**
 * One of this device's own IPv4 addresses, with the length of the network it is on when the
 * interface said, and the network that holds it (contract section 10.1).
 *
 * [network] is what a magic packet is bound to when this address places the machine being woken:
 * with a tunnel up the default route is the tunnel, and a broadcast put on that reaches nothing. It
 * is null where there is no such thing to bind to, which is every test.
 */
data class LocalAddress(
    val address: String,
    val prefixLength: Int?,
    val network: Network? = null,
) {
    /** The address as a number, or null when it is not a dotted-quad IPv4 address. */
    val value: Long? get() = LanPrefix.parseIPv4(address)

    companion object {
        private val LOOPBACK = LanPrefix.parse("127.0.0.0/8")!!

        /** A prefix length of one of this device's addresses, 0 to 32. */
        private val LENGTH = Regex("^(?:[0-9]|[12][0-9]|3[0-2])$")

        /**
         * `a.b.c.d` or `a.b.c.d/length`, as the vectors and an interface write one; null for
         * anything else and for loopback, which is on no site's network however its prefixes read.
         * A port of `parseLocalAddress` in `contract/tools/wake-plan.mjs`.
         */
        fun parse(text: String?): LocalAddress? {
            val raw = text ?: return null
            val slash = raw.indexOf('/')
            val address = if (slash < 0) raw else raw.substring(0, slash)
            val value = LanPrefix.parseIPv4(address) ?: return null
            if (LOOPBACK.contains(value)) return null
            if (slash < 0) return LocalAddress(address, null)
            val length = raw.substring(slash + 1).takeIf { LENGTH.matches(it) } ?: return null
            return LocalAddress(address, length.toInt())
        }
    }
}
