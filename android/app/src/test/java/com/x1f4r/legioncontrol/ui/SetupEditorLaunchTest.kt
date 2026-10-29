package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.LaunchGate
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.LaunchDocuments.LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.descendant
import com.x1f4r.legioncontrol.data.LaunchDocuments.tuple
import com.x1f4r.legioncontrol.data.offered
import com.x1f4r.legioncontrol.data.withSystemValue
import kotlinx.coroutines.flow.StateFlow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.lang.reflect.Proxy

/**
 * The editor never writes a command that could not run, and never lets an edit approve one that
 * nobody approved (contract section 9.9).
 */
class SetupEditorLaunchTest {

    /** The services the editor reads and writes, backed by a real store; nothing else is reachable. */
    private fun servicesOver(store: ConfigStore): ControlServices {
        val unused = Proxy.newProxyInstance(ControlServices::class.java.classLoader, arrayOf(ControlServices::class.java)) { _, method, _ ->
            throw UnsupportedOperationException("${method.name} is not part of editing")
        } as ControlServices
        return object : ControlServices by unused {
            override val document: StateFlow<ControllerDocument?> get() = store.document
            override val launch: StateFlow<LaunchGate.Effective> get() = store.launch
            override fun deviceName(): String = "Test phone"
            override fun applyEdit(document: ControllerDocument, deviceName: String?): String? = store.applyEdit(document, deviceName)
        }
    }

    private fun editor(store: ConfigStore) = SetupEditorModel(servicesOver(store)).apply { startFresh() }

    private fun enrolled() = LaunchDocuments.store().also { it.applyEnrolment(HELD, "Test phone") }

    @Test
    fun `a system is added with the command that runs its agent, and never without one`() {
        val editor = editor(enrolled())

        assertNotNull(editor.addSystem("tower", "mac", "macOS", "mac", emptyList()))
        assertNotNull(editor.addSystem("tower", "mac", "macOS", "mac", listOf("/usr/bin/node", "/Users/me/​launcher.mjs")))
        assertFalse(editor.hasChanges)

        assertNull(editor.addSystem("tower", "mac", "macOS", "mac", argvOf(" /usr/local/bin/node   /Users/me/.legion-control/bin/launcher.mjs ")))
        assertTrue(editor.isValid)
        assertTrue(editor.apply())
    }

    @Test
    fun `a command emptied in the editor cannot be applied`() {
        val editor = editor(enrolled())
        editor.setSystemAgent("tower", "linux", "   ")

        assertFalse(editor.isValid)
        assertNotNull(editor.problem)
        assertTrue(editor.commandProblem!!.startsWith("linux on tower: "))
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
}
