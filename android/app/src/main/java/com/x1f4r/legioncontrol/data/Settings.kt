package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The handful of things worth remembering between launches, per machine.
 *
 * All of it is a hint that makes the next call faster and none of it is trusted: a remembered route
 * is still probed before it is used, and a remembered system is still overwritten by whatever the
 * agent says it is. Losing this file costs one extra round trip, nothing else.
 *
 * Keyed by machine id, because two machines have nothing to say about each other's routes, and a
 * machine that leaves the configuration leaves its hints behind rather than misdirecting the next
 * one to take its place.
 */
class Settings(context: Context) {
    private val prefs: SharedPreferences =
        context.applicationContext.getSharedPreferences("legion-control", Context.MODE_PRIVATE)

    private val routes = mutableMapOf<String, MutableStateFlow<String?>>()
    private val systems = mutableMapOf<String, MutableStateFlow<String?>>()

    /** The route that worked last time, so the next call starts there instead of probing everything. */
    @Synchronized
    fun lastGoodRoute(machineId: String): StateFlow<String?> = route(machineId).asStateFlow()

    @Synchronized
    fun rememberRoute(machineId: String, id: String) {
        val flow = route(machineId)
        if (flow.value == id) return
        prefs.edit().putString(routeKey(machineId), id).apply()
        flow.value = id
    }

    @Synchronized
    fun forgetRoute(machineId: String) {
        prefs.edit().remove(routeKey(machineId)).apply()
        route(machineId).value = null
    }

    /**
     * Which system answered last on that machine. Used to guess the interpreter path first rather
     * than second, and for nothing else: what the UI shows always comes from the current reply.
     */
    @Synchronized
    fun lastKnownSystem(machineId: String): StateFlow<String?> = system(machineId).asStateFlow()

    @Synchronized
    fun rememberSystem(machineId: String, systemId: String) {
        val flow = system(machineId)
        if (flow.value == systemId) return
        prefs.edit().putString(systemKey(machineId), systemId).apply()
        flow.value = systemId
    }

    /** Drops the hints of every machine that is not in [machineIds], after a config change. */
    @Synchronized
    fun retainOnly(machineIds: Set<String>) {
        val keep = machineIds.flatMap { listOf(routeKey(it), systemKey(it)) }.toSet()
        val editor = prefs.edit()
        prefs.all.keys.filterNot { it in keep }.forEach(editor::remove)
        editor.apply()
        routes.keys.retainAll(machineIds)
        systems.keys.retainAll(machineIds)
    }

    private fun route(machineId: String) = routes.getOrPut(machineId) {
        MutableStateFlow(prefs.getString(routeKey(machineId), null))
    }

    private fun system(machineId: String) = systems.getOrPut(machineId) {
        MutableStateFlow(prefs.getString(systemKey(machineId), null))
    }

    private fun routeKey(machineId: String) = "$KEY_ROUTE.$machineId"

    private fun systemKey(machineId: String) = "$KEY_SYSTEM.$machineId"

    private companion object {
        const val KEY_ROUTE = "last-good-route"
        const val KEY_SYSTEM = "last-known-system"
    }
}
