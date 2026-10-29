package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.LaunchDocuments.LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.descendant
import com.x1f4r.legioncontrol.data.LaunchDocuments.tuple
import com.x1f4r.legioncontrol.data.offered
import com.x1f4r.legioncontrol.data.withSystemValue
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The editor never writes a command that could not run, and never lets an edit approve one that
 * nobody approved (contract section 9.9).
 */
class SetupEditorLaunchTest {

    private fun editor(store: ConfigStore) = SetupEditorModel(servicesOver(store)).apply { startFresh() }

    private fun enrolled() = LaunchDocuments.store().also { it.applyEnrolment(HELD, "Test phone") }

    @Test
    fun `a system is added with the command that runs its agent, and never without one`() {
        val editor = editor(enrolled())

        assertNotNull(editor.addSystem("tower", "mac", "macOS", "mac", emptyList()))
        assertNotNull(editor.addSystem("tower", "mac", "macOS", "mac", listOf("/usr/bin/node", "/Users/me/\u200Blauncher.mjs")))
        assertFalse(editor.hasChanges)

        assertNull(editor.addSystem("tower", "mac", "macOS", "mac", argvOf(" /usr/local/bin/node   /Users/me/.legion-control/bin/launcher.mjs ")))
        assertTrue(editor.isValid)
        assertTrue(editor.apply())
    }

    @Test
    fun `a command emptied in the editor cannot be applied`() {
        val store = enrolled()
        val held = store.text.value
        // Empty, and one a person could not check by reading it. The store's own checks let the
        // second through, so the editor is what refuses it, whatever presses Apply.
        for (line in listOf("   ", "/usr/bin/node /home/me/\u200B.legion-control/bin/launcher.mjs")) {
            val editor = editor(store)
            editor.setSystemAgent("tower", "linux", line)

            assertFalse(line, editor.isValid)
            assertNotNull(line, editor.problem)
            assertTrue(line, editor.commandProblem!!.startsWith("linux on tower: "))
            assertFalse(line, editor.apply())
            assertEquals(line, editor.commandProblem, editor.error)
            assertTrue(line, editor.hasChanges)
            assertEquals(line, held, store.text.value)
            assertEquals(line, emptyList<String>(), store.launch.value.review.map { it.path })
        }
    }

    @Test
    fun `the launch inputs of a system whose command waits are read-only`() {
        val store = enrolled()
        store.adoptFromMachine(descendant(store.text.value) { it.withSystemValue("tower", "linux", "agent", listOf("sh", "-c", "id")) }, null, "Tower")
        val editor = editor(store)
        assertNotNull(editor.pendingCommand("tower", "linux"))

        // Changing the shell would otherwise approve the argv beside it, which nobody approved.
        editor.setSystemShell("tower", "linux", "powershell")
        editor.setSystemRestricted("tower", "linux", true)
        editor.setSystemAgent("tower", "linux", LINUX.joinToString(" "))
        assertFalse(editor.hasChanges)

        // Another system of the same machine is edited as usual.
        editor.setSystemShell("tower", "windows", "powershell")
        assertTrue(editor.hasChanges)
        assertTrue(editor.apply())
        assertEquals(listOf("sh", "-c", "id"), store.offered("tower", "linux")?.agent)
        assertEquals(listOf(LaunchDocuments.TOWER_LINUX), store.launch.value.review.map { it.path })
        assertEquals(tuple(LaunchDocuments.WINDOWS, "powershell"), store.offered("tower", "windows"))
    }

    @Test
    fun `Revert waits for an edit in progress, which it would otherwise throw away`() {
        val store = enrolled()
        store.adoptFromMachine(descendant(store.text.value) { it.withSystemValue("tower", "linux", "agent", listOf("sh", "-c", "id")) }, null, "Tower")
        val editor = editor(store)
        val entry = editor.pendingCommand("tower", "linux")!!

        // Revert puts a new document in force at once, and the editor starts again from every one.
        editor.setMachineName("pi", "Beacon 2")
        val adopted = store.text.value
        assertNotNull(editor.revertWaits)
        assertEquals(editor.revertWaits, editor.revertLaunch(entry))
        assertEquals(adopted, store.text.value)
        assertTrue(editor.hasChanges)
        assertEquals(listOf(LaunchDocuments.TOWER_LINUX), store.launch.value.review.map { it.path })

        // With the edit discarded it goes ahead: the approved command is back, and nothing waits.
        editor.discard()
        assertNull(editor.revertWaits)
        editor.revertLaunch(entry)
        assertEquals(LINUX, store.offered("tower", "linux")?.agent)
        assertEquals(emptyList<String>(), store.launch.value.review.map { it.path })
    }
}
