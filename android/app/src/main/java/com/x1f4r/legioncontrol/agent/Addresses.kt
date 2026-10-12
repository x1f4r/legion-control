package com.x1f4r.legioncontrol.agent

/** Which of the three things a dialable host is. */
enum class HostForm(val wire: String) {
    DNS("dns"),
    IPV4("ipv4"),
    IPV6("ipv6"),
}

/**
 * The dial grammar of contract section 15.6: which hosts, users and ports this app will connect to,
 * scan or pin a key for.
 *
 * Stricter than what a document may hold. The document's own guard only refuses what ssh or a
 * known_hosts file would read as syntax; a value that passes it and fails here is still a valid
 * document, and the entry is simply never dialled. The reason it matters on a phone that writes no
 * known_hosts file is the host key store: a pin is keyed by the address it was made for, and a host
 * string a person never typed (a resolver's reading of `1234`, a name with a trailing dot that
 * matches a different search domain) is how one machine's key ends up answering for another.
 *
 * A port of `hostForm`, `isUser` and `isPort` in `contract/tools/transport.mjs`, pinned by the
 * `hosts`, `users` and `ports` groups of `contract/vectors/transport.json`. ASCII classes only, and
 * the whole value has to match.
 */
object Addresses {
    private val LABEL = "[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?"
    private val NAME = Regex("^$LABEL(?:\\.$LABEL)*$")
    private val DIGITS = Regex("^[0-9]+$")
    private val OCTET = Regex("^(?:0|[1-9][0-9]{0,2})$")
    private val IPV6_CHARACTERS = Regex("^[0-9A-Fa-f:.]+$")
    private val HEX_GROUP = Regex("^[0-9A-Fa-f]{1,4}$")
    private val USER = Regex("^[A-Za-z0-9_][A-Za-z0-9._@\\\\ -]*$")
    private val USER_END = Regex("[A-Za-z0-9._-]$")

    /**
     * `dns`, `ipv4` or `ipv6` for a host this app may dial, or null.
     *
     * A name whose last label is all digits has to be a dotted quad: `1234` and `1.2.3` are what a
     * resolver turns into addresses nobody wrote. No brackets, no zone, no trailing dot, no port.
     */
    fun hostForm(value: String): HostForm? {
        if (value.isEmpty() || value.length > 253) return null
        if (value.contains('\n') || value.contains('\r')) return null
        if (value.contains(':')) return if (isIPv6(value)) HostForm.IPV6 else null
        if (!NAME.matches(value)) return null
        val last = value.substringAfterLast('.')
        return if (DIGITS.matches(last)) {
            if (isIPv4(value)) HostForm.IPV4 else null
        } else {
            HostForm.DNS
        }
    }

    /** A login name: local or domain account, 1 to 64 characters, nothing a shell or ssh reads as syntax. */
    fun isUser(value: String): Boolean =
        value.length in 1..64 &&
            !value.contains('\n') && !value.contains('\r') &&
            USER.matches(value) &&
            USER_END.containsMatchIn(value) &&
            !value.contains("  ") && !value.contains(" -")

    /** A TCP port: an integer from 1 to 65535. */
    fun isPort(value: Int): Boolean = value in 1..65535

    /**
     * Whether an endpoint is one this app dials at all. Every part of it is checked, because a user
     * name is as much a part of the ssh command as the host is.
     */
    fun isDialable(host: String, user: String, port: Int): Boolean =
        hostForm(host) != null && isUser(user) && isPort(port)

    /**
     * Whether a host key store address is one built from checked values.
     *
     * The store keys a pin by `host:port`, exactly as [com.x1f4r.legioncontrol.net.Endpoint.address]
     * writes it. The port is whatever follows the last colon, so an IPv6 host cannot be confused with
     * another host and a different port; what this refuses is a key made from a host that never
     * passed [hostForm], which is the only way a pin could end up written for something that was not
     * dialled.
     */
    fun isPinnable(address: String): Boolean {
        val colon = address.lastIndexOf(':')
        if (colon <= 0) return false
        val port = address.substring(colon + 1).toIntOrNull() ?: return false
        if (port.toString() != address.substring(colon + 1)) return false
        return isPort(port) && hostForm(address.substring(0, colon)) != null
    }

    private fun isIPv4(value: String): Boolean {
        val parts = value.split('.')
        return parts.size == 4 && parts.all { OCTET.matches(it) && it.toInt() <= 255 }
    }

    private fun isIPv6(value: String): Boolean {
        if (!IPV6_CHARACTERS.matches(value)) return false
        val halves = value.split("::")
        if (halves.size > 2) return false
        fun groups(text: String): List<String> = if (text.isEmpty()) emptyList() else text.split(':')
        val head = groups(halves[0])
        val tail = if (halves.size == 2) groups(halves[1]) else emptyList()
        val all = head + tail
        var count = 0
        for ((index, group) in all.withIndex()) {
            if (group.contains('.')) {
                // An embedded IPv4 address (::ffff:192.0.2.1) is the last group or nothing.
                val last = index == all.size - 1 && (halves.size == 1 || tail.isNotEmpty())
                if (!last || !isIPv4(group)) return false
                count += 2
            } else if (HEX_GROUP.matches(group)) {
                count += 1
            } else {
                return false
            }
        }
        return if (halves.size == 2) count <= 7 else count == 8
    }
}
