package com.x1f4r.legioncontrol.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The checks that decide whether a setup can actually drive the app, for everything the peer
 * peer setup added.
 *
 * The point of refusing at Apply rather than at the moment a button is pressed: a wake helper that
 * does not exist, or a machine at a site that was deleted, is a document somebody can still fix. The
 * same fault found while a machine is rebooting is not.
 */
class PeerConfigValidationTest {

    private fun read(text: String) = readControllerConfig(text)

    private val base = """
        "machines": [
          {
            "id": "pi", "name": "Atlas", "site": "home",
            "endpoints": [{ "id": "lan", "kind": "lan", "host": "10.0.0.5", "port": 22, "user": "me" }],
            "systems": [{ "id": "linux", "platform": "linux", "agent": ["node", "a.mjs"] }]
          }
        ]
    """.trimIndent()

    @Test
    fun `a setup id has to be an identifier and a revision has to count up`() {
        assertTrue(
            read("""{"version":1,"controller":{"id":"has spaces"},$base}""")
                .exceptionOrNull()!!.message!!.contains("not a valid identifier"),
        )
        assertTrue(
            read("""{"version":1,"controller":{"id":"setup-a","revision":-1},$base}""")
                .exceptionOrNull()!!.message!!.contains("count up"),
        )
        assertTrue(
            read("""{"version":1,"controller":{"revision":3},$base}""")
                .exceptionOrNull()!!.message!!.contains("no id"),
        )
    }

    @Test
    fun `an ancestry has to be sha256 hashes, unique, and within the cap`() {
        val good = "a".repeat(64)
        assertTrue(
            read("""{"version":1,"controller":{"id":"setup-a","lineage":["$good"]},$base,"sites":[{"id":"home"}]}""")
                .isSuccess,
        )
        assertTrue(
            read("""{"version":1,"controller":{"id":"setup-a","lineage":["nope"]},$base,"sites":[{"id":"home"}]}""")
                .exceptionOrNull()!!.message!!.contains("not a sha256"),
        )
        assertTrue(
            read("""{"version":1,"controller":{"id":"setup-a","lineage":["$good","$good"]},$base,"sites":[{"id":"home"}]}""")
                .exceptionOrNull()!!.message!!.contains("same hash twice"),
        )
        val tooMany = (0 until 33).joinToString(",") { "\"${it.toString().padStart(64, '0')}\"" }
        assertTrue(
            read("""{"version":1,"controller":{"id":"setup-a","lineage":[$tooMany]},$base,"sites":[{"id":"home"}]}""")
                .exceptionOrNull()!!.message!!.contains("at most"),
        )
    }

    @Test
    fun `a machine cannot sit at a site that is not there`() {
        val outcome = read("""{"version":1,$base}""")
        assertTrue(outcome.exceptionOrNull()!!.message!!.contains("no site with that id"))

        assertTrue(read("""{"version":1,"sites":[{"id":"home"}],$base}""").isSuccess)
    }

    @Test
    fun `a machine cannot be its own wake helper`() {
        val text = """
            {"version":1,"sites":[{"id":"home"}],"machines":[
              {"id":"pi","site":"home",
               "endpoints":[{"id":"lan","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"linux","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:FF","broadcast":["10.0.0.255"],
                       "helpers":[{"machine":"pi","action":"wake-self"}]}}
            ]}
        """.trimIndent()
        assertTrue(read(text).exceptionOrNull()!!.message!!.contains("cannot send its own magic packet"))
    }

    @Test
    fun `helpers that wake each other in a circle are refused`() {
        // Two machines that are each other's only way of being woken would sit there forever.
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:FF","broadcast":["10.0.0.255"],
                       "helpers":[{"machine":"b","action":"wake-a"}]}},
              {"id":"b","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.6","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:00","broadcast":["10.0.0.255"],
                       "helpers":[{"machine":"a","action":"wake-b"}]}}
            ]}
        """.trimIndent()
        assertTrue(read(text).exceptionOrNull()!!.message!!.contains("circle"))
    }

    @Test
    fun `the singular helper has to be the first of the list, so an older client reads the right one`() {
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:FF","broadcast":["10.0.0.255"],
                       "helper":{"machine":"c","action":"x"},
                       "helpers":[{"machine":"b","action":"x"},{"machine":"c","action":"x"}]}},
              {"id":"b","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.6","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]},
              {"id":"c","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.7","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        assertTrue(read(text).exceptionOrNull()!!.message!!.contains("has to be the first"))
    }

    @Test
    fun `the singular helper is read when there is no list, so a 1 2 document still works`() {
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:FF","broadcast":["10.0.0.255"],
                       "helper":{"machine":"b","action":"wake-a"}}},
              {"id":"b","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.6","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        val config = read(text).getOrThrow()
        val helpers = config.machines.first().wake!!.effectiveHelpers
        assertEquals(1, helpers.size)
        assertEquals("b", helpers.single().machine)
    }

    @Test
    fun `a system whose command needs quoting has to say which shell it logs in to`() {
        // Found at Apply rather than during a restart, which is the whole reason it is checked here.
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["/opt/my node/bin/node","a.mjs"]}]}
            ]}
        """.trimIndent()
        assertTrue(read(text).exceptionOrNull()!!.message!!.contains("cannot be run"))

        val fixed = text.replace("\"id\":\"s\"", "\"id\":\"s\",\"shell\":\"posix\"")
        assertTrue(read(fixed).isSuccess)
    }

    @Test
    fun `an unknown shell name is refused rather than treated as unset`() {
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","shell":"fish","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        assertTrue(read(text).exceptionOrNull()!!.message!!.contains("posix"))
    }

    @Test
    fun `warnings are shown and never block a document`() {
        val text = """
            {"version":1,"sites":[{"id":"home","lanPrefixes":["10.0.0."]},{"id":"away","lanPrefixes":["10.1.0."]}],
             "machines":[
              {"id":"pi","site":"home",
               "endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:FF","broadcast":["10.0.0.255"],
                       "helpers":[{"machine":"far","action":"wake-pi"}]}},
              {"id":"far","site":"away",
               "endpoints":[{"id":"e","kind":"lan","host":"10.1.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        val config = read(text).getOrThrow()
        val advisories = config.advisories()

        // A helper at another site is legitimate: a router action can cross networks. It is a
        // warning with the explanation attached, not a refusal.
        assertTrue(advisories.any { it.contains("different site") })
        // And nothing there is marked always on, which is the energy warning the user asked for.
        assertTrue(advisories.any { it.contains("always on") })
    }

    @Test
    fun `a machine that can be woken by nothing at all is warned about`() {
        val text = """
            {"version":1,"machines":[
              {"id":"pi","endpoints":[{"id":"e","kind":"lan","host":"10.0.0.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"AA:BB:CC:DD:EE:FF","broadcast":["10.0.0.255"]}}
            ]}
        """.trimIndent()
        val advisories = read(text).getOrThrow().advisories()
        assertTrue(advisories.any { it.contains("only a device already on its LAN") })
    }

    @Test
    fun `the example document this app offers actually validates`() {
        // Except for the hardware address, which is deliberately the one field it refuses on sight
        // so that inserting the example and pressing Apply says what has to be filled in.
        assertNotNull(read(EXAMPLE_CONTROLLER_CONFIG).exceptionOrNull())
        val filled = EXAMPLE_CONTROLLER_CONFIG.replace("XX:XX:XX:XX:XX:XX", "AA:BB:CC:DD:EE:FF")
        val config = read(filled).getOrThrow()
        assertEquals(1, config.machines.size)
        assertNotNull(config.controller)
    }
}
