package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.toMachines
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.LaunchDocuments.LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.TOWER_LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.descendant
import com.x1f4r.legioncontrol.data.LaunchDocuments.recorded
import com.x1f4r.legioncontrol.data.LaunchDocuments.tuple
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.withSystemValue
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * What "Fetch from a machine" does with the document it gets (contract section 9.9): shows it, and
 * puts it in force only on Confirm. The store underneath is a real one, so "nothing was applied" is
 * read off its preferences rather than taken on trust.
 */
class SetupEnrolmentTest {

    private val host = "desk-tower.local"

    @Test
    fun `a fetched setup is only shown, and Cancel leaves the device as it was`() {
        val prefs = MemoryPreferences()
        val store = LaunchDocuments.store(prefs)
        val untouched = prefs.values.toMap()
        val writes = prefs.writes.size
        val enrolment = SetupEnrolment(servicesOver(store))

        assertEquals("Check what the setup from $host runs before using it.", enrolment.offer(HELD, host))
        val preview = enrolment.preview!!
        assertTrue(preview.approves)
        assertEquals(listOf("Tower", "Beacon"), preview.machines.map { it.name })
        assertNull(store.document.value)
        assertEquals(untouched, prefs.values)

        assertEquals("Nothing from $host was applied.", enrolment.cancel())
        assertNull(enrolment.preview)
        // Nothing to confirm any more, and nothing was written: no document, no text, no record.
        assertNull(enrolment.confirm())
        assertNull(store.document.value)
        assertEquals("", store.text.value)
        assertEquals(untouched, prefs.values)
        assertEquals(writes, prefs.writes.size)
    }

    @Test
    fun `Confirm on a fresh device puts the setup in force and approves everything it showed`() {
        val prefs = MemoryPreferences()
        val store = LaunchDocuments.store(prefs)
        val enrolment = SetupEnrolment(servicesOver(store))

        enrolment.offer(HELD, host)
        assertEquals("The setup came from $host: 2 machines.", enrolment.confirm())
        assertNull(enrolment.preview)
        assertEquals(ControllerDocument.parse(HELD).getOrThrow().hash, store.document.value?.hash)
        assertEquals(emptyList<String>(), store.launch.value.review.map { it.path })
        assertEquals(tuple(LINUX, "posix"), recorded(prefs, "tower", "linux"))
        assertTrue(store.launch.value.toMachines().flatMap { it.systems }.all { it.isDriven })
    }

    @Test
    fun `Confirm on a device that has a setup approves nothing that was shown`() {
        val store = LaunchDocuments.store().also { it.applyEnrolment(HELD, "Test phone") }
        val enrolment = SetupEnrolment(servicesOver(store))
        val changed = descendant(HELD) { it.withSystemValue("tower", "linux", "agent", listOf("sh", "-c", "id")) }
        val held = store.text.value

        enrolment.offer(changed, host)
        assertFalse(enrolment.preview!!.approves)
        assertEquals(held, store.text.value)
        enrolment.confirm()
        assertEquals(listOf(TOWER_LINUX), store.launch.value.review.map { it.path })
        assertEquals(LINUX, store.launch.value.toMachines().single { it.id == "tower" }.system("linux")!!.agent)
    }

    @Test
    fun `a setup that would be refused is said so, and nothing waits for Confirm`() {
        val store = LaunchDocuments.store()
        val enrolment = SetupEnrolment(servicesOver(store))

        assertTrue(enrolment.offer("{ \"version\": 1, \"machines\": [] ", host).startsWith("The setup from $host was refused. "))
        assertNull(enrolment.preview)
        assertNull(enrolment.confirm())
        assertNull(store.document.value)
    }
}
