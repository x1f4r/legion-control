package com.x1f4r.legioncontrol.data

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Editing the shared document without losing anything.
 *
 * The rule that makes a peer arrangement survivable: an edit must preserve every key it does not
 * understand. A newer client, or somebody's hand, can put a field in this document that this build
 * has never heard of, and if a phone drops it on the first edit the document silently diverges for
 * everybody. That is why editing works on the parsed tree and not on the typed model.
 */
class ControllerDocumentTest {

    private val withUnknownKeys = """
        {
          "version": 1,
          "somethingNewerClientsUnderstand": { "keep": ["me", "please"] },
          "controller": { "id": "setup-abc", "revision": 4, "lineage": ["aaa"], "futureField": 7 },
          "machines": [
            {
              "id": "pi",
              "name": "Atlas",
              "aFieldFromTheFuture": true,
              "endpoints": [
                { "id": "lan", "kind": "lan", "host": "10.0.0.5", "port": 22, "user": "me", "extra": 1 }
              ],
              "systems": [
                { "id": "linux", "platform": "linux", "agent": ["node", "a.mjs"], "unknown": "x" }
              ]
            }
          ]
        }
    """.trimIndent()

    @Test
    fun `an edit keeps every key it does not understand`() {
        val document = ControllerDocument.parse(withUnknownKeys).getOrThrow()
        val edited = document.editMachine("pi") { it.withText("name", "Atlas the Pi") }

        val root = edited.root
        assertNotNull("a top-level key nobody knows has to survive", root["somethingNewerClientsUnderstand"])
        assertEquals(
            "me",
            root["somethingNewerClientsUnderstand"]!!.jsonObject["keep"]!!.jsonArray[0].jsonPrimitive.content,
        )

        val machine = root["machines"]!!.jsonArray[0].jsonObject
        assertEquals("Atlas the Pi", machine["name"]!!.jsonPrimitive.content)
        assertEquals(true, machine["aFieldFromTheFuture"]!!.jsonPrimitive.content.toBoolean())
        assertEquals(
            1,
            machine["endpoints"]!!.jsonArray[0].jsonObject["extra"]!!.jsonPrimitive.content.toInt(),
        )
        assertEquals(
            "x",
            machine["systems"]!!.jsonArray[0].jsonObject["unknown"]!!.jsonPrimitive.content,
        )
    }

    @Test
    fun `an edit descends from what it was made from`() {
        val base = ControllerDocument.parse(withUnknownKeys).getOrThrow()
        val edited = base
            .editMachine("pi") { it.withText("name", "Atlas the Pi") }
            .asEditOf(base, deviceName = "Pixel", now = "2026-09-05T10:00:00Z")

        val identity = edited.identity!!
        assertEquals("setup-abc", identity.id)
        assertEquals(5L, identity.revision)
        assertEquals("the parent's hash goes to the front", base.hash, identity.lineage.first())
        assertTrue("the parent's own ancestry is kept", identity.lineage.contains("aaa"))
        assertEquals("phone", identity.source)
        assertEquals("Pixel", identity.device)

        // And the setup block's own unknown key survives being rewritten.
        assertEquals(
            7,
            edited.root["controller"]!!.jsonObject["futureField"]!!.jsonPrimitive.content.toInt(),
        )
    }

    @Test
    fun `the ancestry never grows past its cap`() {
        var document = ControllerDocument.parse(withUnknownKeys).getOrThrow()
        repeat(40) { index ->
            val next = document.editMachine("pi") { it.withText("name", "Atlas $index") }
            document = next.asEditOf(document, "Pixel", "2026-09-05T10:00:00Z")
        }
        assertEquals(MAX_LINEAGE, document.identity!!.lineage.size)
        assertEquals(44L, document.identity!!.revision)
    }

    @Test
    fun `a document with no setup block gets a fresh identity rather than a device one`() {
        // There is no per-device identity any more: a document created on a phone
        // belongs to the fleet, not to the phone.
        val plain = ControllerDocument.parse("""{"version":1,"machines":[]}""").getOrThrow()
        assertNull(plain.identity)

        val fresh = plain.withFreshIdentity("Pixel", "2026-09-05T10:00:00Z")
        val identity = fresh.identity!!
        assertTrue(identity.id.startsWith("setup-"))
        assertEquals(1L, identity.revision)
        assertTrue(identity.lineage.isEmpty())
    }

    @Test
    fun `writing helpers keeps the singular alias in step for older clients`() {
        val document = ControllerDocument.parse(
            """
            {"version":1,"machines":[{"id":"pi","wake":{"mac":"AA:BB:CC:DD:EE:FF"}}]}
            """.trimIndent(),
        ).getOrThrow()

        val withHelpers = document.setHelpers(
            "pi",
            listOf(WakeHelperConfig("tower", "wake-pi"), WakeHelperConfig("router", "wake-pi")),
        )
        val wake = withHelpers.root["machines"]!!.jsonArray[0].jsonObject["wake"]!!.jsonObject
        assertEquals(2, wake["helpers"]!!.jsonArray.size)
        assertEquals(
            "a 1.2 client only reads the singular one, so it has to be the first",
            "tower",
            wake["helper"]!!.jsonObject["machine"]!!.jsonPrimitive.content,
        )

        val cleared = withHelpers.setHelpers("pi", emptyList())
        val clearedWake = cleared.root["machines"]!!.jsonArray[0].jsonObject["wake"]!!.jsonObject
        assertNull(clearedWake["helpers"])
        assertNull(clearedWake["helper"])
    }

    @Test
    fun `removing a site takes the pointers to it with it`() {
        val document = ControllerDocument.parse(
            """
            {"version":1,"sites":[{"id":"home","lanPrefixes":["10.0.0."]}],
             "machines":[{"id":"pi","site":"home"}]}
            """.trimIndent(),
        ).getOrThrow()

        val without = document.removeSite("home")
        assertEquals(0, without.root["sites"]!!.jsonArray.size)
        assertNull(
            "a machine pointing at a site that has gone would not validate",
            without.root["machines"]!!.jsonArray[0].jsonObject["site"],
        )
    }

    @Test
    fun `the canonical text is what gets hashed and what a machine would store`() {
        val document = ControllerDocument.parse(withUnknownKeys).getOrThrow()
        assertTrue(document.canonicalText.endsWith("\n"))
        assertEquals(CanonicalSetup.hashOf(document.canonicalText), document.hash)

        // Re-parsing the canonical form gives the same hash, so a round trip is stable.
        val again = ControllerDocument.parse(document.canonicalText).getOrThrow()
        assertEquals(document.hash, again.hash)
    }

    @Test
    fun `a document that is not an object is refused with a sentence`() {
        val outcome = ControllerDocument.parse("[1,2,3]")
        assertTrue(outcome.isFailure)
        assertTrue(outcome.exceptionOrNull()!!.message!!.contains("JSON object"))
    }
}
