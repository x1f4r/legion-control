package com.x1f4r.legioncontrol.data

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import java.nio.file.Files

/**
 * Setup documents and stores for the launch tests: a tower with Linux and Windows, and a pi whose key
 * is restricted, written the way the contract's own launch vectors write them.
 */
internal object LaunchDocuments {

    val LINUX = listOf("/usr/bin/node", "/home/me/.legion-control/bin/launcher.mjs")
    val WINDOWS = listOf("C:\\Program Files\\nodejs\\node.exe", "C:\\Users\\me\\.legion-control\\bin\\launcher.mjs")
    val PI = listOf("node", "/home/me/.legion-control/bin/launcher.mjs")

    const val TOWER_LINUX = "machines[tower].systems[linux]"
    const val TOWER_WINDOWS = "machines[tower].systems[windows]"
    const val PI_LINUX = "machines[pi].systems[linux]"

    val HELD: String = """
        {
          "version": 1,
          "controller": {
            "id": "setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846",
            "name": "Home setup",
            "revision": 7,
            "source": "desktop",
            "device": "Desk tower",
            "lineage": []
          },
          "machines": [
            {
              "id": "tower",
              "name": "Tower",
              "endpoints": [{ "id": "lan", "kind": "lan", "host": "desk-tower.local", "user": "me", "port": 22 }],
              "systems": [
                { "id": "linux", "name": "Linux", "platform": "linux", "shell": "posix", "agent": ${argv(LINUX)} },
                { "id": "windows", "name": "Windows 11", "platform": "windows", "shell": "cmd", "agent": ${argv(WINDOWS)} }
              ]
            },
            {
              "id": "pi",
              "name": "Beacon",
              "alwaysOn": true,
              "endpoints": [{ "id": "lan", "kind": "lan", "host": "192.0.2.40", "user": "me", "port": 22 }],
              "systems": [
                { "id": "linux", "name": "Raspberry Pi OS", "platform": "linux", "restricted": true, "agent": ${argv(PI)} }
              ]
            }
          ]
        }
    """.trimIndent()

    fun parse(text: String): JsonObject = Json.parseToJsonElement(text).jsonObject

    fun argv(items: List<String>): JsonArray = JsonArray(items.map(::JsonPrimitive))

    /**
     * The next revision of [of] as a peer publishes it: one more, descending from it, with [change]
     * made to the tree. Exactly what a machine would serve after a `config set` from that peer.
     */
    fun descendant(of: String, device: String = "Old phone", change: (JsonObject) -> JsonObject): String {
        val parent = ControllerDocument.parse(of).getOrThrow()
        val identity = parent.identity!!
        return ControllerDocument.of(change(parent.root)).withIdentity(
            identity.copy(
                revision = parent.revision + 1,
                source = "phone",
                device = device,
                lineage = (listOf(parent.hash) + parent.lineage).take(MAX_LINEAGE),
            ),
        ).canonicalText
    }

    /** A store on [prefs], as a device whose preferences hold whatever they hold. */
    fun store(prefs: MemoryPreferences = MemoryPreferences()): ConfigStore =
        ConfigStore(prefs, RevisionCache(Files.createTempDirectory("revisions").toFile()))

    /** What the record in [prefs] holds for one system. */
    fun recorded(prefs: MemoryPreferences, machine: String, system: String): LaunchTuple? {
        val record = parse(prefs.values[LaunchApprovals.KEY] as String)
        val systems = (record["machines"] as? JsonObject)?.get(machine)?.let { it as? JsonObject }?.get("systems") as? JsonObject
        return LaunchTuple.from(systems?.get(system))
    }

    fun tuple(agent: List<String>, grammar: String) = LaunchTuple(agent, grammar)
}

/** [this] with [change] made to one machine. */
internal fun JsonObject.editedMachine(id: String, change: (JsonObject) -> JsonObject): JsonObject =
    ControllerDocument.of(this).editMachine(id, change).root

/** [this] with [change] made to one system of one machine. */
internal fun JsonObject.editedSystem(machine: String, system: String, change: (JsonObject) -> JsonObject): JsonObject =
    editedMachine(machine) { it.withList("systems") { systems -> systems.map { s -> if (s.text("id") == system) change(s) else s } } }

/** [this] with one key of one system set to a string, a boolean or an argv. */
internal fun JsonObject.withSystemValue(machine: String, system: String, key: String, value: Any): JsonObject =
    editedSystem(machine, system) { s ->
        JsonObject(
            s + (
                key to when (value) {
                    is List<*> -> LaunchDocuments.argv(value.map { it as String })
                    is Boolean -> JsonPrimitive(value)
                    else -> JsonPrimitive(value.toString())
                }
                ),
        )
    }

/** The tuple one system has in the document [this] store has in force. */
internal fun ConfigStore.offered(machine: String, system: String): LaunchTuple? =
    LaunchValues.projection(document.value?.root).machine(machine)?.system(system)?.tuple
