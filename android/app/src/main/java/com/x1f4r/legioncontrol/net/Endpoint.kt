package com.x1f4r.legioncontrol.net

/** How an address reaches a machine, which decides what it can and cannot do. */
enum class RouteKind {
    /** A broadcast domain shared with the machine. Reaches a sleeping one, and only from there. */
    LAN,

    /** A tailnet or VPN address. Works from anywhere, carries no broadcast, so no wake-on-LAN. */
    REMOTE,
}

/**
 * One address a machine might answer on, as the configuration described it.
 *
 * [systemHint] is a routing hint and nothing more. An address that belongs to one system, the way a
 * tailnet address does when the two systems are separate nodes, saves a wasted round trip guessing
 * at the interpreter path. It never decides what the UI reports: that comes out of the agent's own
 * reply, which is also the only thing that can answer it for a LAN address, where every system on
 * the machine answers on the same IP.
 */
data class Endpoint(
    val id: String,
    val kind: RouteKind,
    val host: String,
    val port: Int,
    val user: String,
    val systemHint: String?,
    val label: String,

) {
    val address: String get() = "$host:$port"
}
