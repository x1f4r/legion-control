package com.x1f4r.legioncontrol.net

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

        private fun octet(text: String): Long? {
            if (!OCTET.matches(text)) return null
            return text.toLong().takeIf { it <= 255 }
        }

        private const val ALL_BITS = 0xFFFF_FFFFL

        /** The mask of the first [length] bits. */
        private fun maskOf(length: Int): Long = if (length == 0) 0L else (ALL_BITS shl (32 - length)) and ALL_BITS
    }
}
