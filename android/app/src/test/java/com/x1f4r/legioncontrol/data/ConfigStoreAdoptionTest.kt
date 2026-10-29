package com.x1f4r.legioncontrol.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Writing a document read elsewhere over the one in force (contract section 9.5).
 *
 * Every such write is a compare-and-swap against the head the decision was made from, re-read from
 * storage at the moment of writing (A15). The bytes are stored exactly as read and only if they are
 * the document that was promised (C1). And the document in force is never refused on load, so a
 * setup an earlier build stored under other rules is still there after an update (section 9.7).
 */
class ConfigStoreAdoptionTest {

    private fun enrolled(prefs: MemoryPreferences = MemoryPreferences()): ConfigStore =
        LaunchDocuments.store(prefs).also { assertNull(it.applyEnrolment(LaunchDocuments.HELD, "Test phone")) }

    private fun newer(of: String, name: String) =
        LaunchDocuments.descendant(of) { root -> ControllerDocument.of(root).editMachine("pi") { it.withText("name", name) }.root }

    private fun hashOf(text: String) = ControllerDocument.parse(text).getOrThrow().hash

    @Test
    fun `a copy is adopted only over the head it was chosen against`() {
        val prefs = MemoryPreferences()
        val store = enrolled(prefs)
        val head = store.appliedHash()!!
        val copy = newer(store.text.value, "Beacon two")

        // An edit lands first. The copy was chosen against the document the edit replaced.
        assertNull(store.applyEdit(store.document.value!!.editMachine("tower") { it.withText("name", "Edited") }, "Test phone"))
        val edited = store.appliedHash()
        val writes = prefs.writes.size
        assertEquals(SetupOutcome.HeadMoved, store.adoptFromMachine(copy, hashOf(copy), head, "Pi"))
        assertEquals("nothing was written", writes, prefs.writes.size)
        assertEquals(edited, store.appliedHash())

        // Against the head that is in force it goes through, byte for byte.
        val again = newer(store.text.value, "Beacon three")
        assertTrue(store.adoptFromMachine(again, hashOf(again), edited, "Pi") is SetupOutcome.Applied)
        assertEquals(again, store.text.value)
    }

    @Test
    fun `bytes that are not the promised document, or a promise of none, are refused`() {
        val store = enrolled()
        val head = store.appliedHash()
        val copy = newer(store.text.value, "Beacon two")
        assertTrue(store.adoptFromMachine(copy, "0".repeat(64), head, "Pi") is SetupOutcome.Invalid)
        assertTrue(store.adoptFromMachine(copy, null, head, "Pi") is SetupOutcome.Invalid)
        assertTrue(store.adoptFromMachine(copy, "", head, "Pi") is SetupOutcome.Invalid)
        assertEquals(head, store.appliedHash())
    }

    /**
     * A6: the head is read from storage under one lock for the whole process, so a second model of
     * the same preferences, such as one a rebuilt activity made, cannot write over a head the first
     * one moved while its own view of the document was stale.
     */
    @Test
    fun `two stores over one preferences file take turns at the head`() {
        val prefs = MemoryPreferences()
        val first = enrolled(prefs)
        val second = LaunchDocuments.store(prefs)
        val head = second.appliedHash()!!
        val one = newer(first.text.value, "From the first")
        val other = newer(first.text.value, "From the second")

        assertTrue(first.adoptFromMachine(one, hashOf(one), head, "Pi") is SetupOutcome.Applied)
        assertEquals(SetupOutcome.HeadMoved, second.adoptFromMachine(other, hashOf(other), head, "Tower"))
        assertEquals(hashOf(one), second.appliedHash())
    }

    @Test
    fun `a merge is written only over the document the divergence screen was built from`() {
        val store = enrolled()
        val shown = store.appliedHash()
        val merged = newer(store.text.value, "Merged")
        assertNull(store.applyEdit(store.document.value!!.editMachine("tower") { it.withText("name", "Edited") }, "Test phone"))
        assertEquals(SetupOutcome.HeadMoved, store.applyMerge(merged, shown))
        assertTrue(store.applyMerge(newer(store.text.value, "Merged"), store.appliedHash()) is SetupOutcome.Applied)
    }

    /**
     * A commit the disk refused still changed the preferences in memory, which is where the head is
     * read from. Left there, the store would be ahead of what it announced, and every adoption after
     * it would be refused as a moved head.
     */
    @Test
    fun `a write the disk refused leaves everything as it was, and is said so`() {
        val prefs = MemoryPreferences()
        val store = enrolled(prefs)
        val head = store.appliedHash()!!
        val text = store.text.value
        val launch = store.launch.value
        val saved = prefs.values.toMap()
        val copy = newer(text, "Beacon two")

        prefs.diskRefuses = true
        assertNotNull(store.applyEdit(store.document.value!!.editMachine("tower") { it.withText("name", "Edited") }, "Test phone"))
        val outcome = store.adoptFromMachine(copy, hashOf(copy), head, "Pi")
        assertTrue("$outcome", outcome is SetupOutcome.NotSaved)

        assertEquals(saved, prefs.values)
        assertEquals(head, store.appliedHash())
        assertEquals(text, store.text.value)
        assertEquals(head, store.document.value!!.hash)
        assertEquals(launch, store.launch.value)

        // Nothing is stuck once the disk takes writes again: the same adoption goes through over the same head.
        prefs.diskRefuses = false
        assertTrue(store.adoptFromMachine(copy, hashOf(copy), head, "Pi") is SetupOutcome.Applied)
        assertEquals(copy, store.text.value)
    }

    /**
     * Only what the refused write wrote is put back. Where the setup was fetched from is remembered
     * by an apply outside the head lock, and one that lands while that write is still on its way to
     * a disk that refuses it stays remembered.
     */
    @Test
    fun `a write the disk refused puts back only the keys it wrote`() {
        val prefs = MemoryPreferences()
        val store = enrolled(prefs)
        val head = store.appliedHash()!!
        val text = store.text.value
        val copy = newer(text, "Beacon two")
        val source = SetupSource("tower.example", 22, "me")

        prefs.diskRefuses = true
        prefs.duringNextCommit = { store.rememberSource(source) }
        val outcome = store.adoptFromMachine(copy, hashOf(copy), head, "Pi")
        assertTrue("$outcome", outcome is SetupOutcome.NotSaved)

        assertEquals(head, store.appliedHash())
        assertEquals(text, store.text.value)
        assertEquals("tower.example", prefs.values["setup-source-host"])
        assertEquals(source, LaunchDocuments.store(prefs).source.value)
    }

    @Test
    fun `the document in force is kept on load even when this build would refuse it`() {
        // A setup id the old token grammar allowed and the setup-id grammar does not (C10), stored
        // by an earlier build.
        val prefs = MemoryPreferences()
        val text = LaunchDocuments.HELD.replace("setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846", "me@tower")
        prefs.values["controller-config"] = text
        prefs.values["controller-hash"] = CanonicalSetup.hashOf(text)
        val store = LaunchDocuments.store(prefs)

        assertNotNull("still in force", store.config.value)
        assertEquals(listOf("tower", "pi"), store.config.value!!.machines.map { it.id })
        assertEquals(listOf("schema"), store.acceptance.value!!.errors.map { it.code })
        assertEquals("controller.id", store.acceptance.value!!.errors.single().path)
        assertTrue("an error outside every machine disables none", store.config.value!!.machines.all { it.problem == null })
    }

    @Test
    fun `a machine an error is inside is not driven`() {
        val prefs = MemoryPreferences()
        val text = LaunchDocuments.HELD.replace("\"host\": \"192.0.2.40\"", "\"host\": \"192.0.2.40,nas\"")
        prefs.values["controller-config"] = text
        prefs.values["controller-hash"] = CanonicalSetup.hashOf(text)
        val store = LaunchDocuments.store(prefs)

        val machines = store.launch.value.config!!.machines.associateBy { it.id }
        assertNotNull(machines.getValue("pi").problem)
        assertTrue(machines.getValue("pi").systems.all { it.agent.isEmpty() })
        assertNull(machines.getValue("tower").problem)
        assertTrue(machines.getValue("tower").systems.any { it.agent.isNotEmpty() })
    }

    @Test
    fun `a pasted document without an id gets one, and one with a wrong id is refused`() {
        val store = LaunchDocuments.store()
        val plain = """{"version":1,"controller":{"name":"Home"},"machines":[]}"""
        assertNull(store.applyPasted(plain, "Test phone"))
        val identity = store.document.value!!.identity!!
        assertTrue(identity.id.startsWith("setup-"))
        assertEquals("Home", identity.name)

        val wrong = """{"version":1,"controller":{"id":"me@tower","revision":1},"machines":[]}"""
        assertNotNull(store.applyPasted(wrong, "Test phone"))
        assertEquals(identity.id, store.document.value!!.setupId)
    }
}
