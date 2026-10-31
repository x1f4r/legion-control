package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.agent.LaunchState
import com.x1f4r.legioncontrol.agent.toMachines
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.LaunchDocuments.LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.PI
import com.x1f4r.legioncontrol.data.LaunchDocuments.PI_LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.TOWER_LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.TOWER_WINDOWS
import com.x1f4r.legioncontrol.data.LaunchDocuments.WINDOWS
import com.x1f4r.legioncontrol.data.LaunchDocuments.descendant
import com.x1f4r.legioncontrol.data.LaunchDocuments.parse
import com.x1f4r.legioncontrol.data.LaunchDocuments.recorded
import com.x1f4r.legioncontrol.data.LaunchDocuments.store
import com.x1f4r.legioncontrol.data.LaunchDocuments.tuple
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * This device's record of the commands it approved, and every path that does or does not add to it.
 *
 * Contract section 9.9 lists exactly four sources of approval: a decision on the review, an edit made
 * on this device, enrolment, and the seed on the first start. Everything here is one of those four
 * doing what it should, or one of the other paths, automatic fetch, merge, keep mine, take theirs and
 * following another setup, approving nothing at all.
 */
class LaunchApprovalsTest {

    private val writer = SetupMerge.Writer("phone", "Test phone", "2026-09-05T14:00:00.000Z")

    private val tower = "tower"

    private fun record(prefs: MemoryPreferences): JsonObject = parse(prefs.values[LaunchApprovals.KEY] as String)

    // MARK: - The seed

    @Test
    fun `the first start of this build approves every launch value of the document it already applies`() {
        val upgraded = descendant(HELD) { root ->
            JsonObject(
                root.withSystemValue(tower, "windows", "agent", listOf("C:\\Program Files\\nodejs\\node.exe", "D:\\lc\\bin\\launcher.mjs"))
                    .editedMachine(tower) { JsonObject(it + ("ssh" to buildJsonObject { put("identityFile", "~/.ssh/id_tower") })) } +
                    ("appUpdates" to buildJsonObject { put("githubRepo", "owner/fork") }) +
                    ("local" to buildJsonObject { put("agent", "/Users/me/.legion-control/agent/src/index.mjs") }),
            )
        }
        val prefs = MemoryPreferences().apply { values["controller-config"] = upgraded }

        val store = store(prefs)

        val seeded = record(prefs)
        assertEquals(JsonPrimitive(CanonicalSetup.hashOf(upgraded)), seeded["seededFrom"])
        assertEquals(LaunchValues.approvalsFrom(parse(upgraded), CanonicalSetup.hashOf(upgraded)), seeded)
        assertEquals(emptyList<LaunchReviewEntry>(), store.launch.value.review)
        assertTrue(store.launch.value.toMachines().flatMap { it.systems }.all { it.launchState == LaunchState.APPROVED })
        assertEquals("owner/fork", store.launch.value.config?.appUpdates?.githubRepo)

        // A second start finds the record and leaves it exactly as it is.
        val writes = prefs.writes.size
        store(prefs)
        assertEquals(writes, prefs.writes.size)
        assertEquals(seeded, record(prefs))
    }

    @Test
    fun `a device with nothing applied records that it was seeded from nothing`() {
        val prefs = MemoryPreferences()
        store(prefs)
        assertEquals(LaunchValues.EMPTY_RECORD, record(prefs))
    }

    @Test
    fun `an unreadable record approves nothing, is never seeded again, and is kept aside by the next approval`() {
        val prefs = MemoryPreferences().apply {
            values["controller-config"] = ControllerDocument.parse(HELD).getOrThrow().canonicalText
            values[LaunchApprovals.KEY] = "{ not a record"
        }
        val store = store(prefs)

        assertNotNull(store.launchProblem.value)
        assertEquals("{ not a record", prefs.values[LaunchApprovals.KEY])
        assertEquals(listOf(TOWER_LINUX, TOWER_WINDOWS, PI_LINUX), store.launch.value.review.map { it.path })
        assertTrue(store.launch.value.toMachines().flatMap { it.systems }.all { it.launchState == LaunchState.NOT_DRIVEN })

        // Anything that approves nothing leaves it alone, adoption included.
        store.adoptAsFetched(descendant(store.text.value) { it.editedMachine(tower) { m -> JsonObject(m + ("name" to JsonPrimitive("Desk"))) } })
        assertEquals("{ not a record", prefs.values[LaunchApprovals.KEY])

        assertNull(store.approveLaunch(store.launch.value.review.take(1)))
        val aside = prefs.values.filterKeys { it.startsWith("${LaunchApprovals.KEY}.corrupt-") }
        assertEquals(listOf("{ not a record"), aside.values.toList())
        assertNull(store.launchProblem.value)
        assertEquals(tuple(LINUX, "posix"), recorded(prefs, tower, "linux"))
        assertEquals(listOf(TOWER_WINDOWS, PI_LINUX), store.launch.value.review.map { it.path })
    }

    @Test
    fun `a record in a format this build does not know is unreadable too`() {
        val prefs = MemoryPreferences().apply { values[LaunchApprovals.KEY] = """{"schema":2,"machines":{}}""" }
        val store = store(prefs)
        assertNotNull(store.launchProblem.value)
        assertEquals("""{"schema":2,"machines":{}}""", prefs.values[LaunchApprovals.KEY])
    }

    // MARK: - Edits made here

    @Test
    fun `an edit approves only the tuple it changed, in the same write as the document`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        // A peer changes the tower's Linux command, which waits.
        store.adoptAsFetched(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("sh", "-c", "id")) })
        assertEquals(listOf(TOWER_LINUX), store.launch.value.review.map { it.path })

        val newWindows = listOf("C:\\Program Files\\nodejs\\node.exe", "D:\\lc\\bin\\launcher.mjs")
        val edited = ControllerDocument.of(store.document.value!!.root.withSystemValue(tower, "windows", "agent", newWindows))
        val writes = prefs.writes.size
        assertNull(store.applyEdit(edited, "Test phone"))

        assertEquals(tuple(newWindows, "cmd"), recorded(prefs, tower, "windows"))
        assertEquals(tuple(LINUX, "posix"), recorded(prefs, tower, "linux"))
        assertEquals(listOf(TOWER_LINUX), store.launch.value.review.map { it.path })
        // One write carried both, so no moment exists in which the edit is stored and not approved.
        val write = prefs.writes.drop(writes).single()
        assertTrue("controller-config" in write && LaunchApprovals.KEY in write)
    }

    @Test
    fun `a paste approves what it changed`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        val pasted = descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("/usr/local/bin/node", "/srv/lc/bin/launcher.mjs")) }

        assertNull(store.applyPasted(pasted, "Test phone"))

        assertEquals(tuple(listOf("/usr/local/bin/node", "/srv/lc/bin/launcher.mjs"), "posix"), recorded(prefs, tower, "linux"))
        assertEquals(emptyList<LaunchReviewEntry>(), store.launch.value.review)
    }

    // MARK: - Paths that approve nothing

    @Test
    fun `fetching, merging, keeping mine, taking theirs and following another setup approve nothing`() {
        val changed = listOf("sh", "-c", "touch /tmp/x")
        val paths = mapOf<String, (ConfigStore, String) -> Unit>(
            "automatic fetch" to { store, text -> store.adoptAsFetched(text) },
            "take theirs" to { store, text ->
                store.recordDivergence("tower", "Tower", "edited twice", text)
                val divergence = store.divergence.value!!
                store.adoptFromMachine(divergence.theirText, divergence.theirProvenance.hash, divergence.mineProvenance?.hash, "Tower")
            },
            "merge" to { store, text ->
                val mine = store.text.value
                val merged = SetupMerge.merge(null, mine, text, CanonicalSetup.hashOf(mine), CanonicalSetup.hashOf(text), writer)
                store.applyMerge(merged.resultText!!, store.appliedHash())
            },
            "keep mine" to { store, text ->
                // Mine is the document a peer changed here, kept as a revision descending from both.
                val theirs = store.text.value
                store.adoptAsFetched(text)
                val choices = SetupMerge.differences(text, theirs, null).associate { it.entry to SetupMerge.Choice.MINE }
                val kept = SetupMerge.merge(null, text, theirs, CanonicalSetup.hashOf(text), CanonicalSetup.hashOf(theirs), writer, choices)
                store.applyMerge(kept.resultText!!, store.appliedHash())
            },
            "another setup" to { store, text ->
                val other = ControllerDocument.of(
                    ControllerDocument.parse(text).getOrThrow().withIdentity(
                        DocumentIdentity(id = "setup-0b4f6a12-3c5d-4e7f-8a9b-0c1d2e3f4a5b", revision = 1),
                    ).root,
                )
                store.adoptAsFetched(other.canonicalText)
            },
        )
        for ((name, path) in paths) {
            val prefs = MemoryPreferences()
            val store = store(prefs)
            store.enrolAsFetched(HELD)
            val before = record(prefs)
            val offered = descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", changed) }

            path(store, offered)

            assertTrue("$name stored nothing", store.text.value.contains("touch /tmp/x"))
            assertEquals("$name approved something", before, record(prefs))
            assertEquals(name, listOf(TOWER_LINUX), store.launch.value.review.map { it.path })
        }
    }

    @Test
    fun `fetching on a device that already applies a setup approves nothing either`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        val before = record(prefs)

        store.enrolAsFetched(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("sh", "-c", "id")) })

        assertEquals(before, record(prefs))
        assertEquals(listOf(TOWER_LINUX), store.launch.value.review.map { it.path })
    }

    // MARK: - Approve

    @Test
    fun `approving records exactly the value shown, and a newer one waits as a new entry`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        val first = listOf("/usr/bin/node", "/opt/lc/bin/launcher.mjs")
        store.adoptAsFetched(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", first) })
        val shown = store.launch.value.review.single()

        val second = listOf("/usr/bin/node", "/opt/other/bin/launcher.mjs")
        store.adoptAsFetched(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", second) })
        assertNull(store.approveLaunch(listOf(shown)))

        assertEquals(tuple(first, "posix"), recorded(prefs, tower, "linux"))
        val waiting = store.launch.value.review.single()
        assertEquals(TOWER_LINUX, waiting.path)
        assertEquals(tuple(second, "posix"), waiting.offeredTuple)
        assertEquals(tuple(first, "posix"), waiting.approvedTuple)
        val linux = store.launch.value.toMachines().single { it.id == tower }.system("linux")!!
        assertEquals(first, linux.agent)
        assertEquals(LaunchState.APPROVED_WITH_CHANGE_WAITING, linux.launchState)
    }

    @Test
    fun `an approval the disk refused approves nothing, and is said so`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        store.adoptAsFetched(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("/usr/bin/node", "/opt/lc/bin/launcher.mjs")) })
        val before = record(prefs)
        val launch = store.launch.value
        val shown = launch.review.single()

        prefs.diskRefuses = true
        assertNotNull(store.approveLaunch(listOf(shown)))
        assertEquals(before, record(prefs))
        assertEquals(launch, store.launch.value)

        prefs.diskRefuses = false
        assertNull(store.approveLaunch(listOf(shown)))
        assertTrue(store.launch.value.review.isEmpty())
    }

    @Test
    fun `an approval the disk refused puts back a record that was not text, as it was`() {
        val prefs = MemoryPreferences().apply {
            values["controller-config"] = ControllerDocument.parse(HELD).getOrThrow().canonicalText
            values[LaunchApprovals.KEY] = 7
        }
        val store = store(prefs)
        assertNotNull(store.launchProblem.value)
        val saved = prefs.values.toMap()

        prefs.diskRefuses = true
        assertNotNull(store.approveLaunch(store.launch.value.review.take(1)))
        assertEquals(saved, prefs.values)

        // Nothing was approved, so the next start still finds a record it cannot read, and
        // everything waits.
        prefs.diskRefuses = false
        val next = store(prefs)
        assertNotNull(next.launchProblem.value)
        assertEquals(listOf(TOWER_LINUX, TOWER_WINDOWS, PI_LINUX), next.launch.value.review.map { it.path })
    }

    @Test
    fun `a value that cannot be shown faithfully is never approved`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        store.adoptAsFetched(
            descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("/usr/bin/node", "/home/me/\u202Esjm.rehcnual/nib/")) },
        )
        val before = record(prefs)
        val entry = store.launch.value.review.single()
        assertFalse(entry.approvable)

        assertNotNull(store.approveLaunch(listOf(entry)))
        assertEquals(before, record(prefs))
    }

    @Test
    fun `keys the record does not know about are kept through every write`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        val extended = JsonObject(record(prefs) + ("note" to JsonPrimitive("kept")))
        prefs.edit().putString(LaunchApprovals.KEY, extended.toString()).apply()
        store.adoptAsFetched(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("/usr/bin/node", "/opt/lc/bin/launcher.mjs")) })

        store.approveLaunch(store.launch.value.review)

        assertEquals(JsonPrimitive("kept"), record(prefs)["note"])
    }

    // MARK: - Recording on accept

    @Test
    fun `a value accepted by reuse is recorded at its new place`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        // A new machine that runs exactly what the pi runs: accepted by value, then recorded there.
        val withNas = descendant(store.text.value) { root ->
            val pi = (root["machines"] as JsonArray)[1] as JsonObject
            JsonObject(
                root + (
                    "machines" to JsonArray(
                        (root["machines"] as JsonArray) +
                            JsonObject(pi + ("id" to JsonPrimitive("nas")) + ("name" to JsonPrimitive("Nas"))),
                    )
                    ),
            )
        }
        store.adoptAsFetched(withNas)

        assertEquals(emptyList<LaunchReviewEntry>(), store.launch.value.review)
        assertEquals(tuple(PI, "dispatch"), recorded(prefs, "nas", "linux"))
    }

    @Test
    fun `revert puts the approved tuple back as an edit, and the grammar with it`() {
        val prefs = MemoryPreferences()
        val store = store(prefs)
        store.enrolAsFetched(HELD)
        store.adoptAsFetched(
            descendant(store.text.value) { it.withSystemValue("pi", "linux", "restricted", false).withSystemValue("pi", "linux", "shell", "powershell") },
        )
        val entry = store.launch.value.review.single()
        assertEquals(PI_LINUX, entry.path)

        val reverted = LaunchValues.reverted(store.document.value!!.root, entry)!!
        assertNull(store.applyEdit(ControllerDocument.of(reverted), "Test phone"))

        assertEquals(emptyList<LaunchReviewEntry>(), store.launch.value.review)
        assertEquals(tuple(PI, "dispatch"), store.offered("pi", "linux"))
        assertEquals(tuple(WINDOWS, "cmd"), recorded(prefs, tower, "windows"))
    }
}
