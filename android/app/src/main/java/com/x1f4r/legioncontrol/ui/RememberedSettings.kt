package com.x1f4r.legioncontrol.ui

import android.content.Context

/**
 * The last value each system reported for automatic updates, and when it was read.
 *
 * Only one system on a machine is ever awake, so the others' switches can never be read live. The
 * choice is between showing nothing and showing what was true the last time that system was up,
 * clearly dated. The second is more useful, as long as the age is never hidden.
 *
 * Kept beside the screen rather than in the transport's own settings, because none of it is a
 * routing hint and none of it is ever sent anywhere: it exists only so that a row can say something
 * honest about a machine that is switched off.
 */
data class RememberedSystem(
    val autoUpdate: Boolean? = null,
    val checkedAt: Long? = null,
)

/** A service this machine had the last time it answered. Only ever an id and what to call it. */
data class RememberedService(
    val id: String,
    val name: String,
)

class RememberedSettings(context: Context) {
    private val prefs = context.applicationContext
        .getSharedPreferences("legion-remembered", Context.MODE_PRIVATE)

    fun load(machineId: String, systemIds: List<String>): Map<String, RememberedSystem> =
        systemIds.associateWith { systemId ->
            val key = valueKey(machineId, systemId)
            RememberedSystem(
                autoUpdate = if (prefs.contains(key)) prefs.getBoolean(key, false) else null,
                checkedAt = prefs.getLong(stampKey(machineId, systemId), 0L).takeIf { it > 0L },
            )
        }

    fun save(machineId: String, systemId: String, autoUpdate: Boolean?, checkedAt: Long) {
        prefs.edit().apply {
            val key = valueKey(machineId, systemId)
            if (autoUpdate == null) remove(key) else putBoolean(key, autoUpdate)
            putLong(stampKey(machineId, systemId), checkedAt)
        }.apply()
    }

    /**
     * The services this machine reported last time.
     *
     * A page per service, and pages cannot appear and disappear as a machine goes to sleep: the
     * pager would renumber itself under the user's thumb and the swipe would land somewhere else.
     * So the list survives the machine being unreachable, and those pages say they are waiting
     * rather than not existing.
     */
    fun services(machineId: String): List<RememberedService> =
        prefs.getString(servicesKey(machineId), null)
            .orEmpty()
            .lineSequence()
            .mapNotNull { line ->
                val parts = line.split('\t')
                val id = parts.getOrNull(0)?.takeIf { it.isNotBlank() } ?: return@mapNotNull null
                RememberedService(id, parts.getOrNull(1)?.takeIf { it.isNotBlank() } ?: id)
            }
            .toList()

    fun saveServices(machineId: String, services: List<RememberedService>) {
        prefs.edit()
            .putString(servicesKey(machineId), services.joinToString("\n") { "${it.id}\t${it.name}" })
            .apply()
    }

    private fun valueKey(machineId: String, systemId: String) = "autoUpdate.$machineId.$systemId"

    private fun stampKey(machineId: String, systemId: String) = "checkedAt.$machineId.$systemId"

    private fun servicesKey(machineId: String) = "services.$machineId"
}
