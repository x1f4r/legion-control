package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.ControllerMark
import com.x1f4r.legioncontrol.agent.SetupDecision
import com.x1f4r.legioncontrol.agent.SetupHead
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.agent.decide
import com.x1f4r.legioncontrol.agent.readControllerReply
import com.x1f4r.legioncontrol.agent.toMachines
import com.x1f4r.legioncontrol.data.CanonicalSetup
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.LaunchDocuments.LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.TOWER_LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.descendant
import com.x1f4r.legioncontrol.data.LaunchDocuments.recorded
import com.x1f4r.legioncontrol.data.LaunchDocuments.tuple
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.enrolAsFetched
import com.x1f4r.legioncontrol.data.withSystemValue
import com.x1f4r.legioncontrol.data.withText
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
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

    private fun hashOf(text: String) = CanonicalSetup.hashOf(text)

    /** [text] as an agent that sends `raw` serves it, with the hash of exactly those bytes. */
    private fun served(text: String) = ControllerFetch.Document(
        text = text,
        provenance = SetupProvenance(authority = null, revision = null, hash = hashOf(text)),
        replyHash = hashOf(text),
    )

    private fun pi(text: String, name: String) =
        descendant(text) { root -> ControllerDocument.of(root).editMachine("pi") { it.withText("name", name) }.root }

    private val unverifiable =
        "$host's control agent has to be updated before its setup can be used: what it sent does not match the hash it reported."

    @Test
    fun `a fetched setup is only shown, and Cancel leaves the device as it was`() {
        val prefs = MemoryPreferences()
        val store = LaunchDocuments.store(prefs)
        val untouched = prefs.values.toMap()
        val writes = prefs.writes.size
        val enrolment = SetupEnrolment(servicesOver(store))

        assertEquals("Check what the setup from $host runs before using it.", enrolment.offer(served(HELD), host))
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

        enrolment.offer(served(HELD), host)
        assertEquals("The setup came from $host: 2 machines.", enrolment.confirm())
        assertNull(enrolment.preview)
        assertEquals(ControllerDocument.parse(HELD).getOrThrow().hash, store.document.value?.hash)
        assertEquals(emptyList<String>(), store.launch.value.review.map { it.path })
        assertEquals(tuple(LINUX, "posix"), recorded(prefs, "tower", "linux"))
        assertTrue(store.launch.value.toMachines().flatMap { it.systems }.all { it.isDriven })
    }

    @Test
    fun `Confirm on a device that has a setup approves nothing that was shown`() {
        val store = LaunchDocuments.store().also { it.enrolAsFetched(HELD) }
        val enrolment = SetupEnrolment(servicesOver(store))
        val changed = descendant(HELD) { it.withSystemValue("tower", "linux", "agent", listOf("sh", "-c", "id")) }
        val held = store.text.value

        enrolment.offer(served(changed), host)
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

        assertTrue(enrolment.offer(served("{ \"version\": 1, \"machines\": [] "), host).startsWith("The setup from $host was refused. "))
        assertNull(enrolment.preview)
        assertNull(enrolment.confirm())
        assertNull(store.document.value)
    }

    // MARK: - Only the bytes the machine holds (contract section 9.5)

    /**
     * A revision the desktop app wrote before 1.4.0, through .NET's default encoder, which spells `+`
     * as `\u002B` and is otherwise laid out as `JSON.stringify` lays it out. An agent before 3.1.0
     * sends the document parsed, with no `raw`, and printing it again gives `+`: a document whose
     * hash is in no machine's lineage. Enrolled, every machine diverged from it.
     */
    @Test
    fun `a copy rebuilt from an agent before 3_1_0 that is not the stored bytes is refused`() {
        val written = HELD.replace("\"revision\": 7,", "\"revision\": 7,\n    \"updatedAt\": \"2026-10-04T06:36:35.3956760+00:00\",")
        val stringified = ControllerDocument.of(LaunchDocuments.parse(written)).canonicalText
        val stored = stringified.replace("+00:00", "\\u002B00:00")
        val reply = buildJsonObject {
            put("ok", true)
            put("contract", 3)
            put("agentVersion", "3.0.1")
            put("controller", Json.parseToJsonElement(stored))
            put("hash", hashOf(stored))
        }
        val rebuilt = readControllerReply(reply) as ControllerFetch.Document
        assertEquals("the escape is the only difference", stringified, rebuilt.text)
        assertNotEquals(hashOf(stored), hashOf(rebuilt.text))

        val store = LaunchDocuments.store()
        val enrolment = SetupEnrolment(servicesOver(store))
        // A copy that was fine a moment ago is not left waiting behind the refusal.
        enrolment.offer(served(HELD), host)

        assertEquals(unverifiable, enrolment.offer(rebuilt, host))
        assertNull(enrolment.preview)
        assertNull(enrolment.confirm())
        assertNull(store.document.value)

        // An agent from 3.1.0 on sends the stored bytes, which are taken exactly as they are.
        val raw = readControllerReply(JsonObject(reply + ("raw" to JsonPrimitive(stored)))) as ControllerFetch.Document
        assertEquals("Check what the setup from $host runs before using it.", enrolment.offer(raw, host))
        enrolment.confirm()
        assertEquals(hashOf(stored), store.appliedHash())
        assertEquals(stored, store.text.value)
    }

    @Test
    fun `a reply that names no hash is never offered`() {
        val store = LaunchDocuments.store()
        val enrolment = SetupEnrolment(servicesOver(store))

        assertEquals(unverifiable, enrolment.offer(served(HELD).copy(replyHash = null), host))
        assertNull(enrolment.preview)
        assertNull(store.document.value)
    }

    // MARK: - Decided by ancestry, never re-stamped

    /**
     * The phone holds revision 7 and the machine revision 9, made from it. Fetching to catch up used
     * to stamp the fetched copy as revision 10, an edit of 7: a revision descending from neither,
     * which every machine then diverged from instead of being in step.
     */
    @Test
    fun `a newer revision of this setup is taken exactly as served, and every machine holding it is in step`() {
        val store = LaunchDocuments.store().also { it.enrolAsFetched(HELD) }
        val enrolment = SetupEnrolment(servicesOver(store))
        val newer = pi(pi(HELD, "Beacon two"), "Beacon three")

        assertEquals("Check what the setup from $host runs before using it.", enrolment.offer(served(newer), host))
        assertFalse(enrolment.preview!!.approves)
        assertEquals("The setup came from $host: 2 machines.", enrolment.confirm())

        assertEquals(newer, store.text.value)
        val document = store.document.value!!
        assertEquals(9L, document.revision)
        assertEquals(ControllerDocument.parse(newer).getOrThrow().lineage, document.lineage)
        val head = SetupHead(document.hash, document.setupId, document.lineage)
        val mark = ControllerMark(hash = hashOf(newer), id = document.setupId)
        assertEquals(SetupDecision.IN_SYNC, decide(head, 3, mark, emptyMap()).decision)
    }

    @Test
    fun `the copy this device has, or one it was made from, changes nothing and waits for nothing`() {
        val prefs = MemoryPreferences()
        val store = LaunchDocuments.store(prefs).also { it.enrolAsFetched(HELD) }
        assertTrue(store.enrolAsFetched(pi(HELD, "Beacon two")) is SetupOutcome.Applied)
        val text = store.text.value
        val writes = prefs.writes.size
        val enrolment = SetupEnrolment(servicesOver(store))

        assertEquals("This device already has the setup $host holds.", enrolment.offer(served(text), host))
        assertNull(enrolment.preview)
        assertEquals(
            "This device's setup was made from the one $host holds, so there is nothing newer to take.",
            enrolment.offer(served(HELD), host),
        )
        assertNull(enrolment.preview)
        assertEquals(text, store.text.value)
        assertEquals(writes, prefs.writes.size)
    }

    @Test
    fun `a branch of this device's copy goes to be merged, and is not put over it`() {
        val store = LaunchDocuments.store().also { it.enrolAsFetched(HELD) }
        assertNull(store.applyEdit(store.document.value!!.editMachine("tower") { it.withText("name", "Edited here") }, "Test phone"))
        val mine = store.text.value
        val theirs = pi(HELD, "Edited there")
        val enrolment = SetupEnrolment(servicesOver(store))

        assertEquals(
            "$host has a copy of the setup that was edited separately. It is waiting to be merged.",
            enrolment.offer(served(theirs), host),
        )
        assertNull(enrolment.preview)
        assertEquals(mine, store.text.value)
        val divergence = store.divergence.value!!
        assertEquals(host, divergence.machineName)
        assertEquals(theirs, divergence.theirText)
        assertEquals(hashOf(mine), divergence.mineProvenance?.hash)
    }

    @Test
    fun `Confirm after the setup here changed applies nothing`() {
        val store = LaunchDocuments.store().also { it.enrolAsFetched(HELD) }
        val enrolment = SetupEnrolment(servicesOver(store))
        enrolment.offer(served(pi(HELD, "Beacon two")), host)
        assertNull(store.applyEdit(store.document.value!!.editMachine("tower") { it.withText("name", "Edited here") }, "Test phone"))
        val edited = store.text.value

        assertEquals(
            "The setup here changed while this was open, so nothing from $host was applied. Fetch it again to compare.",
            enrolment.confirm(),
        )
        assertEquals(edited, store.text.value)
    }
}
