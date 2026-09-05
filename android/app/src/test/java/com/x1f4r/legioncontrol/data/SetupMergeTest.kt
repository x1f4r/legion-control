package com.x1f4r.legioncontrol.data

import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Putting two versions of a setup back together.
 *
 * The case this is for: two people edit the same setup while one of them is away. A JSON diff would
 * show forty changed lines when what actually happened is "they renamed the tower and I added an
 * address to the Pi", and those two edits do not conflict at all. So the document is taken apart into
 * pieces somebody can decide about, and only the pieces both sides moved need an answer.
 */
class SetupMergeTest {

    private fun document(machines: String, sites: String = "", extra: String = "") =
        ControllerDocument.parse(
            "{\"version\":1$sites,\"machines\":[$machines]$extra}",
        ).getOrThrow().root

    private val base = document(
        """{"id":"pi","name":"Atlas","endpoints":[{"id":"lan","host":"10.0.0.5","user":"me"}],"systems":[]},
           {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
    )

    private val mine = document(
        """{"id":"pi","name":"Atlas","endpoints":[{"id":"lan","host":"10.0.0.5","user":"me"},{"id":"tailnet","host":"100.1.1.1","user":"me"}],"systems":[]},
           {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
    )

    private val theirs = document(
        """{"id":"pi","name":"Atlas","endpoints":[{"id":"lan","host":"10.0.0.5","user":"me"}],"systems":[]},
           {"id":"tower","name":"The big tower","endpoints":[],"systems":[]}""",
    )

    @Test
    fun `two edits to different things merge with nothing to decide`() {
        val differences = SetupMerge.differences(mine, theirs, base)
        assertEquals(2, differences.size)

        // One side added an address, the other renamed a machine. Neither touched what the other did.
        assertTrue(differences.none { it.bothChanged })
        assertTrue(SetupMerge.needsDecision(differences).isEmpty())

        val merged = SetupMerge.merge(mine, theirs, SetupMerge.defaults(differences))
        val machines = merged["machines"]!!.jsonArray.map { it.jsonObject }
        val pi = machines.first { it["id"]!!.jsonPrimitive.content == "pi" }
        val tower = machines.first { it["id"]!!.jsonPrimitive.content == "tower" }

        assertEquals("my new address survives", 2, pi["endpoints"]!!.jsonArray.size)
        assertEquals("their rename survives", "The big tower", tower["name"]!!.jsonPrimitive.content)
    }

    @Test
    fun `an entry both sides changed is listed for a decision and defaults to theirs`() {
        val bothChanged = document(
            """{"id":"pi","name":"Atlas the Pi","endpoints":[{"id":"lan","host":"10.0.0.5","user":"me"}],"systems":[]},
               {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
        )
        val theirRename = document(
            """{"id":"pi","name":"Atlas the Raspberry","endpoints":[{"id":"lan","host":"10.0.0.5","user":"me"}],"systems":[]},
               {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
        )

        val differences = SetupMerge.differences(bothChanged, theirRename, base)
        val decision = SetupMerge.needsDecision(differences).single()
        assertEquals("machines[pi].info", decision.path)
        assertEquals(SetupMerge.Choice.THEIRS, SetupMerge.defaults(differences)[decision.path])

        // And choosing mine actually keeps mine.
        val merged = SetupMerge.merge(
            bothChanged,
            theirRename,
            mapOf(decision.path to SetupMerge.Choice.MINE),
        )
        val pi = merged["machines"]!!.jsonArray.map { it.jsonObject }
            .first { it["id"]!!.jsonPrimitive.content == "pi" }
        assertEquals("Atlas the Pi", pi["name"]!!.jsonPrimitive.content)
    }

    @Test
    fun `without a base every difference needs an answer`() {
        val differences = SetupMerge.differences(mine, theirs, base = null)
        assertEquals(2, differences.size)
        assertEquals(
            "with nothing to compare against, neither side can be called untouched",
            2,
            SetupMerge.needsDecision(differences).size,
        )
    }

    @Test
    fun `a machine only one side has is added or removed by one choice`() {
        val withExtra = document(
            """{"id":"pi","endpoints":[],"systems":[]},{"id":"laptop","name":"Laptop","endpoints":[],"systems":[]}""",
        )
        val without = document("""{"id":"pi","endpoints":[],"systems":[]}""")

        val differences = SetupMerge.differences(withExtra, without, base = without)
        val entry = differences.single { it.path == "machines[laptop].info" }
        assertNotNull(entry.mine)
        assertNull(entry.theirs)

        val kept = SetupMerge.merge(withExtra, without, mapOf(entry.path to SetupMerge.Choice.MINE))
        assertEquals(2, kept["machines"]!!.jsonArray.size)

        val dropped = SetupMerge.merge(withExtra, without, mapOf(entry.path to SetupMerge.Choice.THEIRS))
        assertEquals(1, dropped["machines"]!!.jsonArray.size)
    }

    @Test
    fun `a merge keeps keys neither side's model understands`() {
        val a = ControllerDocument.parse(
            """{"version":1,"futureTopLevel":{"a":1},"machines":[{"id":"pi","futureMachineKey":"x","name":"Atlas","endpoints":[],"systems":[]}]}""",
        ).getOrThrow().root
        val b = ControllerDocument.parse(
            """{"version":1,"futureTopLevel":{"a":1},"machines":[{"id":"pi","futureMachineKey":"x","name":"Pi","endpoints":[],"systems":[]}]}""",
        ).getOrThrow().root

        val differences = SetupMerge.differences(a, b, base = null)
        val merged = SetupMerge.merge(a, b, SetupMerge.defaults(differences))

        assertNotNull("a top-level key nobody knows survives", merged["futureTopLevel"])
        val pi = merged["machines"]!!.jsonArray[0].jsonObject
        assertEquals("x", pi["futureMachineKey"]!!.jsonPrimitive.content)
        assertEquals("the chosen side is theirs by default", "Pi", pi["name"]!!.jsonPrimitive.content)
    }

    @Test
    fun `the common base is the newest hash both sides know`() {
        assertEquals(
            "h5",
            SetupMerge.commonBase(
                myLineage = listOf("h6a", "h5", "h4"),
                theirLineage = listOf("h6b", "h5", "h4"),
            ),
        )
        assertNull(
            SetupMerge.commonBase(myLineage = listOf("a"), theirLineage = listOf("b")),
        )
    }

    @Test
    fun `a change preview counts what a person would notice`() {
        assertEquals(listOf("pi", "tower"), base.machineIds())
        assertEquals("Atlas", base.machineLabel("pi"))
        assertEquals(1, base.endpointCount())
        assertEquals(2, mine.endpointCount())
        assertTrue(sameMachine(base, mine, "tower"))
        assertTrue(!sameMachine(base, mine, "pi"))
    }
}
