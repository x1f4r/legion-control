package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.withText
import com.x1f4r.legioncontrol.data.withWake
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * How the editor writes where machines are and where their packets go (contract section 10.1).
 *
 * An octet-aligned prefix is saved with its trailing dot, because a client that predates the section
 * matches prefixes as text and is right only then; one that is not a prefix at all is refused rather
 * than saved to match nothing; and an empty list is never written.
 */
class SetupEditorWakeTest {

    private fun enrolled() = LaunchDocuments.store().also { assertNull(it.applyEnrolment(HELD, "Test phone")) }

    private fun editor(store: ConfigStore) = SetupEditorModel(servicesOver(store)).apply { startFresh() }

    private fun site(store: ConfigStore, id: String): JsonObject =
        (store.document.value!!.root["sites"] as JsonArray).map { it as JsonObject }.single { (it["id"] as JsonPrimitive).content == id }

    private fun strings(value: Any?): List<String>? = (value as? JsonArray)?.map { (it as JsonPrimitive).content }

    @Test
    fun `a new site carries no empty lists`() {
        val store = enrolled()
        val editor = editor(store)
        assertNull(editor.addSite("home", "Home"))
        assertTrue(editor.apply())

        val home = site(store, "home")
        assertFalse("lanPrefixes" in home)
        assertFalse("broadcast" in home)
    }

    @Test
    fun `prefixes are saved the way every client reads them alike`() {
        val store = enrolled()
        val editor = editor(store)
        editor.addSite("home", "Home")
        editor.setSitePrefixes("home", listOf("192.168.1", "10.0.0.0/8", "192.168.16.0/20", "192.0.2.7/32"))
        editor.setSiteBroadcasts("home", listOf("192.168.1.255"))
        assertTrue(editor.error ?: "", editor.apply())

        val home = site(store, "home")
        assertEquals(listOf("192.168.1.", "10.", "192.168.16.0/20", "192.0.2.7/32"), strings(home["lanPrefixes"]))
        assertEquals(listOf("192.168.1.255"), strings(home["broadcast"]))
    }

    @Test
    fun `a prefix that is not one is refused, and nothing is saved`() {
        val store = enrolled()
        val before = store.text.value
        for (prefix in listOf("192.168.1.1", "192.168.1.1/24", "10.*", "2001:db8::/32")) {
            val editor = editor(store)
            editor.addSite("home", "Home")
            editor.setSitePrefixes("home", listOf(prefix))
            assertFalse(prefix, editor.apply())
            assertTrue(prefix, editor.error!!.contains("is not a network prefix"))
            assertEquals(prefix, before, store.text.value)
        }

        val editor = editor(store)
        editor.setWakeMac("tower", "02:00:00:00:00:01")
        editor.setWakeLanPrefix("tower", "10.0.0.5")
        assertFalse(editor.apply())
        editor.setWakeLanPrefix("tower", "10.0.0")
        assertTrue(editor.apply())
        val wake = ((store.document.value!!.root["machines"] as JsonArray).first() as JsonObject)["wake"] as JsonObject
        assertEquals(JsonPrimitive("10.0.0."), wake["lanPrefix"])
    }

    @Test
    fun `clearing a list takes the key out rather than leaving it empty`() {
        val store = enrolled()
        val editor = editor(store)
        editor.setWakeMac("tower", "02:00:00:00:00:01")
        editor.setWakeBroadcasts("tower", listOf("192.168.1.255"))
        editor.setWakeBroadcasts("tower", listOf(""))
        assertTrue(editor.apply())
        val wake = ((store.document.value!!.root["machines"] as JsonArray).first() as JsonObject)["wake"] as JsonObject
        assertFalse("broadcast" in wake)
    }

    @Test
    fun `a prefix the edit did not touch is left as the peer wrote it`() {
        val peer = ControllerDocument.parse(HELD).getOrThrow().editMachine("tower") { machine ->
            machine.withWake { it.withText("mac", "02:00:00:00:00:01").withText("lanPrefix", "10.0.0") }
        }
        val renamed = peer.editMachine("pi") { it.withText("name", "Beacon two") }
        val written = normalisePrefixes(renamed, peer).getOrThrow()
        val wake = ((written.root["machines"] as JsonArray).first() as JsonObject)["wake"] as JsonObject
        assertEquals(JsonPrimitive("10.0.0"), wake["lanPrefix"])
    }
}
