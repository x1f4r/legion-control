package com.x1f4r.legioncontrol.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Putting two versions of a setup back together (contract section 9.8).
 *
 * The case this is for: two people edit the same setup while one of them is away. A JSON diff would
 * show forty changed lines when what actually happened is "they renamed the tower and I added an
 * address to the Pi", and those two edits do not conflict at all. So the document is taken apart into
 * pieces somebody can decide about, and only the pieces both sides moved need an answer.
 *
 * The shared vectors pin the merge case by case. These name what this app used to lose (C6): an edit
 * beaten by a deletion without a question, endpoints without an id, another peer's unknown keys, the
 * order of a list and the spelling of a number.
 */
class SetupMergeTest {

    private val writer = SetupMerge.Writer("phone", "Test phone", "2026-09-05T14:00:00.000Z")

    /** A whole document with an identity, so it can be merged; [machines] is the body of the list. */
    private fun document(machines: String, extra: String = "", lineage: List<String> = emptyList()) =
        "{\"version\":1,\"controller\":{\"id\":\"setup-a\",\"revision\":4,\"lineage\":" +
            lineage.joinToString(",", "[", "]") { "\"$it\"" } + "}$extra,\"machines\":[$machines]}"

    private val baseText = document(
        """{"id":"pi","name":"Beacon","endpoints":[{"id":"lan","host":"192.0.2.5","user":"me"}],"systems":[]},
           {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
    )
    private val base = CanonicalSetup.hashOf(baseText)

    private val mineText = document(
        """{"id":"pi","name":"Beacon","endpoints":[{"id":"lan","host":"192.0.2.5","user":"me"},{"id":"tailnet","host":"100.64.1.1","user":"me"}],"systems":[]},
           {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
        lineage = listOf(base),
    )

    private val theirsText = document(
        """{"id":"pi","name":"Beacon","endpoints":[{"id":"lan","host":"192.0.2.5","user":"me"}],"systems":[]},
           {"id":"tower","name":"The big tower","endpoints":[],"systems":[]}""",
        lineage = listOf(base),
    )

    private fun merge(mine: String, theirs: String, baseText: String? = this.baseText, choices: Map<String, SetupMerge.Choice> = emptyMap()) =
        SetupMerge.merge(baseText, mine, theirs, CanonicalSetup.hashOf(mine), CanonicalSetup.hashOf(theirs), writer, choices)

    private fun parsed(text: String) = SetupJsonParser.parse(text) as SetupJson.Object

    private fun machine(text: String, id: String): SetupJson.Object? =
        (parsed(text)["machines"] as SetupJson.Array).items.map { it as SetupJson.Object }
            .firstOrNull { (it["id"] as SetupJson.Text).value == id }

    private fun SetupJson.Object.text(key: String) = (this[key] as? SetupJson.Text)?.value

    @Test
    fun `two edits to different things merge with nothing to decide`() {
        val outcome = merge(mineText, theirsText)
        assertEquals(
            listOf("machines[pi].endpoints[tailnet]" to SetupMerge.Kind.MINE, "machines[tower].fields" to SetupMerge.Kind.THEIRS),
            outcome.differences.map { it.entry to it.kind },
        )
        assertTrue(outcome.differences.all { it.settles })

        val result = outcome.resultText!!
        assertEquals("my new address survives", 2, (machine(result, "pi")!!["endpoints"] as SetupJson.Array).items.size)
        assertEquals("their rename survives", "The big tower", machine(result, "tower")!!.text("name"))

        // The result descends from both, so either branch takes it as a fast-forward.
        val identity = ControllerDocument.parse(result).getOrThrow().identity!!
        assertEquals("setup-a", identity.id)
        assertEquals(5L, identity.revision)
        assertEquals(listOf(CanonicalSetup.hashOf(mineText), CanonicalSetup.hashOf(theirsText), base), identity.lineage)
        assertEquals("phone", identity.source)
        assertEquals("Test phone", identity.device)
    }

    @Test
    fun `a deletion against an edit is a question, not a silent loss`() {
        val mine = document(
            """{"id":"pi","name":"Beacon","endpoints":[],"systems":[]},{"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
            lineage = listOf(base),
        )
        val theirs = document(
            """{"id":"pi","name":"Beacon","endpoints":[{"id":"lan","host":"192.0.2.6","user":"me"}],"systems":[]},
               {"id":"tower","name":"Tower","endpoints":[],"systems":[]}""",
            lineage = listOf(base),
        )
        val outcome = merge(mine, theirs)
        val difference = outcome.differences.single()
        assertEquals("machines[pi].endpoints[lan]", difference.entry)
        assertEquals(SetupMerge.Kind.BOTH, difference.kind)
        assertEquals("removed here and changed on the other side", difference.summary)

        // Preselected theirs, so the edit comes back unless somebody chooses the deletion.
        val endpoints = machine(outcome.resultText!!, "pi")!!["endpoints"] as SetupJson.Array
        assertEquals("192.0.2.6", (endpoints.items.single() as SetupJson.Object).text("host"))
        val deleted = merge(mine, theirs, choices = mapOf(difference.entry to SetupMerge.Choice.MINE))
        assertTrue((machine(deleted.resultText!!, "pi")!!["endpoints"] as SetupJson.Array).items.isEmpty())
    }

    @Test
    fun `endpoints without an id are one entry, and are never dropped`() {
        val base = document("""{"id":"pi","endpoints":[{"host":"192.0.2.5","user":"me"}],"systems":[]}""")
        val mine = document("""{"id":"pi","name":"Beacon","endpoints":[{"host":"192.0.2.5","user":"me"}],"systems":[]}""")
        val theirs = document("""{"id":"pi","endpoints":[{"host":"192.0.2.5","user":"me"},{"host":"192.0.2.9","user":"me"}],"systems":[]}""")
        val outcome = merge(mine, theirs, baseText = base)
        assertEquals(
            listOf("machines[pi].endpoints[]" to SetupMerge.Kind.THEIRS, "machines[pi].fields" to SetupMerge.Kind.MINE),
            outcome.differences.map { it.entry to it.kind },
        )
        val pi = machine(outcome.resultText!!, "pi")!!
        assertEquals("Beacon", pi.text("name"))
        assertEquals(listOf("192.0.2.5", "192.0.2.9"), (pi["endpoints"] as SetupJson.Array).items.map { (it as SetupJson.Object).text("host") })
    }

    @Test
    fun `an unknown top-level key is taken from the side that changed it`() {
        val base = document("""{"id":"pi","endpoints":[],"systems":[]}""", extra = ",\"notes\":\"old\"")
        val mine = document("""{"id":"pi","name":"Beacon","endpoints":[],"systems":[]}""", extra = ",\"notes\":\"old\"")
        val theirs = document("""{"id":"pi","endpoints":[],"systems":[]}""", extra = ",\"notes\":\"new\",\"future\":{\"a\":1}")
        val outcome = merge(mine, theirs, baseText = base)
        val text = outcome.resultText!!
        val result = parsed(text)
        assertEquals("new", (result["notes"] as SetupJson.Text).value)
        assertTrue("a key only theirs has arrives", result.has("future"))
        assertEquals("Beacon", machine(text, "pi")!!.text("name"))
    }

    @Test
    fun `what is not taken from theirs keeps its place and its spelling`() {
        val mine = document(
            """{"id":"a","weight":22.0,"endpoints":[],"systems":[]},{"id":"b","endpoints":[],"systems":[]},{"id":"c","endpoints":[],"systems":[]}""",
        )
        // Theirs lists the machines in another order, spells the number differently and adds one.
        val theirs = document(
            """{"id":"c","endpoints":[],"systems":[]},{"id":"a","weight":22,"endpoints":[],"systems":[]},{"id":"x","endpoints":[],"systems":[]},{"id":"b","endpoints":[],"systems":[]}""",
        )
        val outcome = merge(mine, theirs, baseText = null)
        assertEquals(listOf("machines[x]" to SetupMerge.Kind.NO_BASE), outcome.differences.map { it.entry to it.kind })
        val result = outcome.resultText!!
        assertEquals(
            "mine's order, with theirs' new machine after its predecessor in theirs",
            listOf("a", "x", "b", "c"),
            (parsed(result)["machines"] as SetupJson.Array).items.map { (it as SetupJson.Object).text("id") },
        )
        assertTrue("22.0 is not rewritten as 22", result.contains("\"weight\": 22.0"))
    }

    @Test
    fun `a merge whose choices break a rule is not written, and says which`() {
        val mine = document("""{"id":"tower","endpoints":[],"systems":[]}""")
        val theirs = document(
            """{"id":"pi","endpoints":[],"systems":[]},
               {"id":"tower","endpoints":[],"systems":[],"wake":{"mac":"02:00:00:00:00:01","helpers":[{"machine":"pi","action":"wol"}]}}""",
        )
        val outcome = merge(mine, theirs, baseText = null, choices = mapOf("machines[pi]" to SetupMerge.Choice.MINE))
        assertNull(outcome.resultText)
        assertEquals("helper-unknown", outcome.error)
    }

    @Test
    fun `a side that breaks a rule its entries depend on cannot be merged`() {
        val broken = document("""{"id":"pi","endpoints":[],"systems":[]},{"id":"pi","endpoints":[],"systems":[]}""")
        assertTrue(SetupMerge.mineBlocks(broken) != null)
        assertNull(SetupMerge.mineBlocks(mineText))
    }

    @Test
    fun `the base is the newest shared ancestor whose kept copy still hashes to it`() {
        val kept = mapOf(base to baseText, "f".repeat(64) to "not the document it claims to be")
        assertEquals(
            base,
            SetupMerge.commonBase("m", "t", listOf("f".repeat(64), base), listOf("f".repeat(64), base), kept::get),
        )
        assertNull(SetupMerge.commonBase("m", "t", listOf(base), listOf("e".repeat(64)), kept::get))
    }

    @Test
    fun `a change preview counts what a person would notice`() {
        val baseRoot = ControllerDocument.parse(baseText).getOrThrow().root
        val mineRoot = ControllerDocument.parse(mineText).getOrThrow().root
        assertEquals(listOf("pi", "tower"), baseRoot.machineIds())
        assertEquals("Beacon", baseRoot.machineLabel("pi"))
        assertEquals(1, baseRoot.endpointCount())
        assertEquals(2, mineRoot.endpointCount())
        assertTrue(sameMachine(baseRoot, mineRoot, "tower"))
        assertTrue(!sameMachine(baseRoot, mineRoot, "pi"))
    }
}
