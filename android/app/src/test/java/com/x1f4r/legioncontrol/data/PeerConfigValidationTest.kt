package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.contract.VectorJson
import com.x1f4r.legioncontrol.contract.VectorJson.boolean
import com.x1f4r.legioncontrol.contract.VectorJson.string
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The checks that decide whether a setup can actually drive the app, for everything the peer setup
 * added (contract section 9.7).
 *
 * The point of refusing at Apply rather than at the moment a button is pressed: a wake helper that
 * does not exist, or a machine at a site that was deleted, is a document somebody can still fix. The
 * same fault found while a machine is rebooting is not. What this app merely cannot use is a warning
 * instead, because a peer whose setup this app refused would stop replicating without anybody
 * noticing.
 */
class PeerConfigValidationTest {

    private fun read(text: String) = readControllerConfig(text)

    private fun errors(text: String): List<String> = DocumentAcceptance.check(text, Intake.PEER).errors.map { it.code }

    private fun warnings(text: String): List<String> = acceptControllerConfig(text).getOrThrow().verdict.warnings.map { it.code }

    private val base = """
        "machines": [
          {
            "id": "pi", "name": "Beacon", "site": "home",
            "endpoints": [{ "id": "lan", "kind": "lan", "host": "192.0.2.5", "port": 22, "user": "me" }],
            "systems": [{ "id": "linux", "platform": "linux", "agent": ["node", "a.mjs"] }]
          }
        ]
    """.trimIndent()

    @Test
    fun `a setup id has to be a setup id and a revision has to count up`() {
        assertTrue(
            read("""{"version":1,"controller":{"id":"has spaces","revision":1},$base,"sites":[{"id":"home"}]}""")
                .exceptionOrNull()!!.message!!.contains("not a setup id"),
        )
        assertTrue(
            read("""{"version":1,"controller":{"id":"setup-a","revision":-1},$base,"sites":[{"id":"home"}]}""")
                .exceptionOrNull()!!.message!!.contains("counts up from zero"),
        )
        // A block with no id is one a peer must not send: only a paste here gives it one.
        assertEquals(
            listOf("schema"),
            errors("""{"version":1,"controller":{"revision":3},$base,"sites":[{"id":"home"}]}"""),
        )
    }

    @Test
    fun `an ancestry has to be sha256 hashes, unique, and within the cap`() {
        val good = "a".repeat(64)
        fun with(lineage: String) = """{"version":1,"controller":{"id":"setup-a","revision":1,"lineage":[$lineage]},$base,"sites":[{"id":"home"}]}"""
        assertTrue(read(with("\"$good\"")).isSuccess)
        assertTrue(read(with("\"nope\"")).exceptionOrNull()!!.message!!.contains("not a sha256"))
        assertTrue(read(with("\"$good\",\"$good\"")).exceptionOrNull()!!.message!!.contains("twice"))
        val tooMany = (0 until 33).joinToString(",") { "\"${it.toString().padStart(64, '0')}\"" }
        assertTrue(read(with(tooMany)).exceptionOrNull()!!.message!!.contains("at most"))
    }

    @Test
    fun `an ancestry entry is a hash exactly when the contract's sha256 vectors say so`() {
        // The ancestry is checked with Contract.isSha256Hex, so the identifier vectors pin the code
        // that runs here rather than a copy of the pattern kept beside it.
        val cases = VectorJson.cases("identifiers", "sha256")
        assertTrue(cases.isNotEmpty())
        cases.forEach { case ->
            val entry = JsonPrimitive(case.input.string("value")!!)
            val text = """{"version":1,"controller":{"id":"setup-a","revision":1,"lineage":[$entry]},$base,"sites":[{"id":"home"}]}"""
            assertEquals(case.name, case.expect.boolean("valid"), read(text).isSuccess)
        }
    }

    @Test
    fun `a machine cannot sit at a site that is not there, unless there are no sites`() {
        assertEquals(listOf("unknown-site"), errors("""{"version":1,"sites":[{"id":"away"}],$base}"""))
        assertTrue(read("""{"version":1,"sites":[{"id":"home"}],$base}""").isSuccess)
        // A setup without sites places nobody, so a site name has nothing to be checked against.
        assertTrue(read("""{"version":1,$base}""").isSuccess)
    }

    @Test
    fun `a machine cannot be its own wake helper`() {
        val text = """
            {"version":1,"sites":[{"id":"home"}],"machines":[
              {"id":"pi","site":"home",
               "endpoints":[{"id":"lan","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
               "systems":[{"id":"linux","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:01","broadcast":["192.0.2.255"],
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
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:01","broadcast":["192.0.2.255"],
                       "helpers":[{"machine":"b","action":"wake-a"}]}},
              {"id":"b","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.6","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:02","broadcast":["192.0.2.255"],
                       "helpers":[{"machine":"a","action":"wake-b"}]}}
            ]}
        """.trimIndent()
        assertTrue(read(text).exceptionOrNull()!!.message!!.contains("circle"))
    }

    @Test
    fun `the singular helper has to repeat the first of the list, so an older client reads the right one`() {
        fun text(alias: String) = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:01","broadcast":["192.0.2.255"],
                       "helper":$alias,
                       "helpers":[{"machine":"b","action":"x"},{"machine":"c","action":"x"}]}},
              {"id":"b","alwaysOn":true,"endpoints":[{"id":"e","kind":"lan","host":"192.0.2.6","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]},
              {"id":"c","alwaysOn":true,"endpoints":[{"id":"e","kind":"lan","host":"192.0.2.7","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        assertTrue(read(text("""{"machine":"c","action":"x"}""")).exceptionOrNull()!!.message!!.contains("has to repeat the first"))
        // The action is part of it too: the same machine with another action is another helper.
        assertEquals(listOf("helper-alias"), errors(text("""{"machine":"b","action":"y"}""")))
        assertTrue(read(text("""{"machine":"b","action":"x"}""")).isSuccess)
    }

    @Test
    fun `the singular helper is read when there is no list, so a 1 2 document still works`() {
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:01","broadcast":["192.0.2.255"],
                       "helper":{"machine":"b","action":"wake-a"}}},
              {"id":"b","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.6","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        val config = read(text).getOrThrow()
        val helpers = config.machines.first().wake!!.effectiveHelpers
        assertEquals(1, helpers.size)
        assertEquals("b", helpers.single().machine)
    }

    @Test
    fun `a command that needs quoting is a valid document, written for the platform's own shell`() {
        // A path with a space used to need a declared shell before the document was accepted at all.
        // Every grammar now has exactly one way to write it, and a system without a shell gets the
        // platform's stock OpenSSH shell, so the document is valid as it stands.
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["/opt/my node/bin/node","a.mjs"]}]}
            ]}
        """.trimIndent()
        assertTrue(read(text).isSuccess)

        // A command its grammar cannot write is still a valid document, and is said at Apply.
        val percent = text.replace("\"/opt/my node/bin/node\"", "\"C:\\\\Users\\\\%USERNAME%\\\\node.exe\"")
            .replace("\"id\":\"s\"", "\"id\":\"s\",\"platform\":\"windows\"")
        val advisories = acceptControllerConfig(percent).getOrThrow().advisories
        assertTrue(advisories.toString(), advisories.any { it.contains("cannot be sent") && it.contains("%") })
    }

    @Test
    fun `a tilde in a command and an address that is never dialled are said, and not refused`() {
        val text = """
            {"version":1,"machines":[
              {"id":"a","endpoints":[
                 {"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"},
                 {"id":"odd","kind":"remote","host":"1234","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","~/.legion-control/agent/src/index.mjs"]}]}
            ]}
        """.trimIndent()
        val accepted = acceptControllerConfig(text).getOrThrow()
        assertTrue(accepted.advisories.toString(), accepted.advisories.any { it.contains("~/.legion-control") && it.contains("not expanded") })
        val undialable = accepted.verdict.warnings.single()
        assertEquals("endpoint-undialable", undialable.code)
        assertEquals("machines[0].endpoints[1].host", undialable.path)
    }

    @Test
    fun `an unknown shell name is refused rather than treated as unset`() {
        listOf("fish", "auto", "POSIX").forEach { shell ->
            val text = """
                {"version":1,"machines":[
                  {"id":"a","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
                   "systems":[{"id":"s","shell":"$shell","agent":["node","a.mjs"]}]}
                ]}
            """.trimIndent()
            assertTrue(shell, read(text).exceptionOrNull()!!.message!!.contains("posix"))
        }
    }

    @Test
    fun `warnings are shown and never block a document`() {
        val text = """
            {"version":1,"sites":[{"id":"home","lanPrefixes":["192.168.1."]},{"id":"away","lanPrefixes":["192.168.10."]}],
             "machines":[
              {"id":"pi","site":"home",
               "endpoints":[{"id":"e","kind":"lan","host":"192.168.1.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:01","broadcast":["192.168.1.255"],
                       "helpers":[{"machine":"far","action":"wake-pi"}]}},
              {"id":"far","site":"away",
               "endpoints":[{"id":"e","kind":"lan","host":"192.168.10.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        // A helper at another site is legitimate: a router action can cross networks. It is a
        // warning with the explanation attached, not a refusal. And nothing there is marked always
        // on, which is the energy warning the user asked for.
        assertEquals(listOf("helper-other-site", "helper-not-always-on"), warnings(text))
    }

    @Test
    fun `a machine that can be woken by nothing at all is warned about`() {
        val text = """
            {"version":1,"machines":[
              {"id":"pi","endpoints":[{"id":"e","kind":"lan","host":"192.0.2.5","port":22,"user":"me"}],
               "systems":[{"id":"s","agent":["node","a.mjs"]}],
               "wake":{"mac":"02:00:00:00:00:01","broadcast":["192.0.2.255"]}}
            ]}
        """.trimIndent()
        assertEquals(listOf("wake-unplaced"), warnings(text))
    }

    @Test
    fun `the example document this app offers actually validates`() {
        // Except for the hardware address, which is deliberately the one field it refuses on sight
        // so that inserting the example and pressing Apply says what has to be filled in.
        assertNotNull(read(EXAMPLE_CONTROLLER_CONFIG).exceptionOrNull())
        val filled = EXAMPLE_CONTROLLER_CONFIG.replace("XX:XX:XX:XX:XX:XX", "02:00:00:00:00:01")
        val config = read(filled).getOrThrow()
        assertEquals(1, config.machines.size)
        assertNotNull(config.controller)
    }
}
