package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.data.Settings
import java.util.concurrent.ConcurrentHashMap

/**
 * Decides which of a machine's addresses to talk to, without knocking on any of them first.
 *
 * This used to open a bare TCP connection to every address before the one that mattered, on the
 * grounds that a connect is cheaper than a handshake. It is, and it is also exactly the traffic
 * OpenSSH 9.8 and later count against a source: connections that arrive and never complete
 * authentication. A phone polling four addresses every fifteen seconds is a machine teaching its own
 * sshd to refuse it. The probe has been taken out of the normal path entirely.
 *
 * What replaces it is memory and patience. The route that worked last time is tried first, as a real
 * authenticated request rather than a knock. An address that fails before the command is dispatched
 * is held back for a while, doubling each time it fails again, so a machine that is switched off
 * costs one connection per poll instead of one per address. Nothing here ever holds back the only
 * candidate there is: when everything is in backoff, the one closest to being due is still tried, so
 * a machine that comes back is found on the next poll rather than at the end of some cooldown.
 */
class RouteSelector(
    private val machine: Machine,
    private val settings: Settings,
    private val now: () -> Long = System::currentTimeMillis,
) {

    @Volatile
    private var expectedSystem: String? = null

    @Volatile
    private var preferLan: Boolean = false

    /** What one address did the last time it was dialled, as a sentence for a failure message. */
    data class Attempt(val endpoint: Endpoint, val note: String, val at: Long)

    private val attempts = ConcurrentHashMap<String, Attempt>()
    private val backoff = ConcurrentHashMap<String, Backoff>()

    private data class Backoff(val until: Long, val step: Long)

    /**
     * The route that worked last time, unprobed.
     *
     * It is returned without checking that anything is listening, which is the entire change: the
     * check was one unauthenticated connection per call, and the authenticated request that follows
     * answers the same question a moment later anyway.
     */
    fun remembered(): Endpoint? = machine.endpoint(settings.lastGoodRoute(machine.id).value)

    /**
     * The addresses worth dialling on this call, in the order to dial them.
     *
     * The remembered route leads, the configuration's own order follows, and addresses in backoff
     * are left out. If that empties the list, the address whose backoff expires soonest is returned
     * on its own: a machine that is off should cost one connection per poll, not none and not four.
     */
    fun candidates(): List<Endpoint> {
        val ordered = orderEndpoints(machine.endpoints, remembered()?.id, expectedSystem, preferLan)
        val moment = now()
        val ready = ordered.filter { (backoff[it.id]?.until ?: 0L) <= moment }
        if (ready.isNotEmpty()) return ready
        return listOfNotNull(ordered.minByOrNull { backoff[it.id]?.until ?: 0L })
    }

    /** Every address the configuration names, whatever their backoff. For diagnostics only. */
    val allEndpoints: List<Endpoint> get() = machine.endpoints

    /** How long [endpoint] is being held back for, or null when it is not. */
    fun heldBackFor(endpoint: Endpoint): Long? =
        backoff[endpoint.id]?.until?.minus(now())?.takeIf { it > 0L }

    /**
     * Called when an address failed before the command reached the far side.
     *
     * Doubling, so a machine that is switched off stops being dialled four times a minute, and
     * capped, so one that comes back is found again within a poll or two of coming back.
     */
    fun penalise(endpoint: Endpoint, note: String) {
        val previous = backoff[endpoint.id]?.step ?: 0L
        val step = if (previous <= 0L) FIRST_BACKOFF_MS else (previous * 2).coerceAtMost(MAX_BACKOFF_MS)
        backoff[endpoint.id] = Backoff(until = now() + step, step = step)
        attempts[endpoint.id] = Attempt(endpoint, note, now())
    }

    /** Called once a command has actually worked, so the next one starts here and unpenalised. */
    fun remember(endpoint: Endpoint) {
        expectedSystem = null
        preferLan = false
        backoff.remove(endpoint.id)
        attempts[endpoint.id] = Attempt(endpoint, "answered", now())
        settings.rememberRoute(machine.id, endpoint.id)
    }

    /** Records what an address did without holding it back: it answered, it just did not help. */
    fun note(endpoint: Endpoint, note: String) {
        attempts[endpoint.id] = Attempt(endpoint, note, now())
    }

    fun forget() {
        backoff.clear()
        settings.forgetRoute(machine.id)
    }

    /**
     * Drops an outgoing system's route and arranges the next authenticated call for the requested
     * system. The endpoint hint only affects order; whatever system answers is still accepted.
     */
    fun prepareForSystem(systemId: String, onSite: Boolean) {
        expectedSystem = systemId
        preferLan = onSite
        forget()
        settings.rememberSystem(machine.id, systemId)
    }

    /** A sentence naming what was tried and what each one did, for an unreachable failure. */
    fun probeSummary(): String {
        val lines = machine.endpoints.mapNotNull { endpoint ->
            attempts[endpoint.id]?.let { "${endpoint.label} (${endpoint.address}) ${it.note}" }
        }
        if (lines.isEmpty()) return "No address has been tried yet."
        return lines.joinToString("; ")
    }

    /** What each address last did, for the diagnostics page. */
    fun lastAttempts(): List<Attempt> = machine.endpoints.mapNotNull { attempts[it.id] }

    private companion object {
        /**
         * Never more than once per sixty seconds per address while an address keeps failing. A poll runs every fifteen seconds, so a machine that is switched
         * off costs one connection in four rather than four in four.
         */
        const val FIRST_BACKOFF_MS = 60_000L

        /** Four polls' worth at the outside, so a machine that comes back is found within a minute. */
        const val MAX_BACKOFF_MS = 240_000L
    }
}

/** The W4 endpoint order as a pure function, so boot transitions do not depend on UI timing. */
internal fun orderEndpoints(
    endpoints: List<Endpoint>,
    rememberedId: String?,
    expectedSystem: String?,
    preferLan: Boolean,
): List<Endpoint> = buildList {
    fun addUnique(endpoint: Endpoint) {
        if (none { it.id == endpoint.id }) add(endpoint)
    }
    endpoints.firstOrNull { it.id == rememberedId }?.let(::addUnique)
    expectedSystem?.let { wanted ->
        endpoints.filter { it.systemHint == wanted }.forEach(::addUnique)
    }
    endpoints.filter { it.kind == if (preferLan) RouteKind.LAN else RouteKind.REMOTE }
        .forEach(::addUnique)
    endpoints.forEach(::addUnique)
}
