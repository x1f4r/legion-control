package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.contract.ContractCorpus
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Which setup documents this app accepts (contract section 9.7), beyond the shared vectors.
 *
 * The vectors pin the verdict on one document at a time. These hold the rest of what has to agree
 * with the other clients: the shared document fixtures, each of which is valid or invalid exactly as
 * `contract/fixtures/index.json` says, the schema grammars, which have to be the schema's own
 * patterns character for character, and the reading a typed model is made from, which must never
 * turn away a document acceptance let through (C3).
 */
class DocumentAcceptanceTest {

    @Test
    fun `every document fixture is accepted or refused as the fixture index says`() {
        val fixtures = (ContractCorpus.json("fixtures/index.json") as JsonObject)["fixtures"] as JsonArray
        val documents = fixtures.map { it as JsonObject }.filter { (it["kind"] as JsonPrimitive).content == "document" }
            .filter { (it["file"] as JsonPrimitive).content.startsWith("controller-document.") }
        assertTrue("the index lists no setup documents", documents.size >= 10)
        for (entry in documents) {
            val file = (entry["file"] as JsonPrimitive).content
            val expected = (entry["expect"] as JsonPrimitive).content
            val verdict = DocumentAcceptance.check(ContractCorpus.file("fixtures/$file").readBytes(), Intake.PEER)
            assertEquals("$file is $expected: ${verdict.errors}", expected == "valid", verdict.valid)
            if (expected == "valid") {
                // And a document acceptance let through is one the typed model reads.
                assertTrue(file, readControllerConfig(ContractCorpus.file("fixtures/$file").readText()).isSuccess)
            }
        }
    }

    /** C-2: the guard grammars are the schema's, so the agent, the schema and this app cannot drift apart. */
    @Test
    fun `the string grammars are the schemas' own patterns`() {
        val common = ((ContractCorpus.json("schemas/common.schema.json") as JsonObject)["\$defs"] as JsonObject)
        fun pattern(definition: String) = ((common[definition] as JsonObject)["pattern"] as JsonPrimitive).content
        assertEquals(pattern("sshHostGuard"), DocumentAcceptance.HOST_GUARD_PATTERN)
        assertEquals(pattern("sshUser"), DocumentAcceptance.USER_GUARD_PATTERN)
        assertEquals(pattern("agentArgItem"), DocumentAcceptance.AGENT_ARG_PATTERN)
        assertEquals(pattern("githubRepo"), DocumentAcceptance.GITHUB_REPO_PATTERN)

        val document = ContractCorpus.json("schemas/controller-document.schema.json") as JsonObject
        val wake = (((document["\$defs"] as JsonObject)["wake"] as JsonObject)["properties"] as JsonObject)
        assertEquals(((wake["mac"] as JsonObject)["pattern"] as JsonPrimitive).content, DocumentAcceptance.MAC_PATTERN)
    }

    /**
     * The document C3 was found with: the minimal fixture, a machine with nothing but an id, which
     * this app used to refuse for having no systems and no endpoints. It is valid, with warnings that
     * say why nothing on it can be driven yet.
     */
    @Test
    fun `a machine that is not finished yet is a warning, not a refused setup`() {
        val text = """{"version":1,"machines":[{"id":"tower"}]}"""
        val accepted = acceptControllerConfig(text).getOrThrow()
        assertEquals(listOf("tower"), accepted.config.machines.map { it.id })
        assertEquals(listOf("machine-no-systems", "machine-no-endpoints"), accepted.verdict.warnings.map { it.code })
        assertTrue(accepted.advisories.isNotEmpty())
    }

    @Test
    fun `a schema-valid number is read whatever its spelling`() {
        // 22.0 and 1e1 are integers to the schema, so they are a port and a revision here too.
        val text = """
            {"version":1,"controller":{"id":"setup-a","revision":1e1},
             "machines":[{"id":"tower","endpoints":[{"id":"lan","host":"desk-tower.local","user":"me","port":22.0}],
               "systems":[{"id":"linux","agent":["node","a.mjs"]}]}]}
        """.trimIndent()
        val config = readControllerConfig(text).getOrThrow()
        assertEquals(22, config.machines.single().endpoints.single().port)
        assertEquals(10L, config.controller?.revision)

        // The identity a push and an edit are made from reads it the same way. A push of this
        // document is revision 10, the one the agent finds in it, and an edit of it is revision 11.
        val document = ControllerDocument.parse(text).getOrThrow()
        assertEquals(10L, document.identity!!.revision)
        assertEquals(11L, document.asEditOf(document, "Test phone", "2026-09-05T14:00:00.000Z").identity!!.revision)
    }

    /**
     * Section 9.7 sets no upper bound on a revision, and one past the range of a Long is still a
     * document the contract accepts. It is read as the largest Long, and an edit of it stays there
     * rather than wrapping round to a revision no machine takes.
     */
    @Test
    fun `a revision past the range of a Long is accepted, and never wraps round`() {
        val text = """{"version":1,"controller":{"id":"setup-a","revision":1e20},"machines":[]}"""
        assertTrue(DocumentAcceptance.check(text, Intake.PEER).valid)
        assertEquals(Long.MAX_VALUE, readControllerConfig(text).getOrThrow().controller?.revision)

        val document = ControllerDocument.parse(text).getOrThrow()
        assertEquals(Long.MAX_VALUE, document.identity!!.revision)
        val edited = document.asEditOf(document, "Test phone", "2026-09-05T14:00:00.000Z")
        assertEquals(Long.MAX_VALUE, edited.identity!!.revision)
        assertTrue(DocumentAcceptance.check(edited.canonicalText, Intake.PEER).valid)

        val store = LaunchDocuments.store()
        assertTrue(store.adoptFromMachine(text, CanonicalSetup.hashOf(text), null, "Pi") is SetupOutcome.Applied)
        assertEquals(CanonicalSetup.canonicalText(text), store.text.value)
    }

    /**
     * Applied intake never refuses: a document an earlier build stored under other rules stays in
     * force with its error shown, and the machine the error is inside is the only one disabled.
     */
    @Test
    fun `the document in force is kept with its errors, and only the machine at fault is disabled`() {
        val text = """
            {"version":1,"controller":{"id":"setup-a","revision":3},"machines":[
              {"id":"tower","endpoints":[{"id":"lan","host":"desk-tower.local,evil","user":"me"}],
               "systems":[{"id":"linux","agent":["node","a.mjs"]}]},
              {"id":"pi","endpoints":[{"id":"lan","host":"192.0.2.40","user":"me"}],
               "systems":[{"id":"linux","agent":["node","a.mjs"]}]}
            ]}
        """.trimIndent()
        assertTrue(readControllerConfig(text).isFailure)

        val applied = acceptControllerConfig(text, Intake.APPLIED).getOrThrow()
        assertEquals(listOf("schema"), applied.verdict.errors.map { it.code })
        val (tower, pi) = applied.config.machines
        assertNotNull("the machine with the bad host is disabled", tower.problem)
        assertNull("the other machine is not", pi.problem)
    }

    @Test
    fun `a text section 12 refuses is not kept, not even as the document in force`() {
        val verdict = DocumentAcceptance.check("{\"machines\":[]}", Intake.APPLIED)
        assertFalse(verdict.keep)
        assertEquals(listOf("schema"), verdict.errors.map { it.code })
        assertTrue(acceptControllerConfig("   ", Intake.APPLIED).isFailure)
    }

    @Test
    fun `a repeated member is the only thing said about a document`() {
        val text = """{"version":1,"machines":[{"id":"a","systems":[{"id":"s","agent":["x"],"agent":["y"]}]}]}"""
        val verdict = DocumentAcceptance.check(text, Intake.PEER)
        assertEquals(listOf("duplicate-member"), verdict.errors.map { it.code })
        assertEquals("machines[0].systems[0].agent", verdict.errors.single().path)
        assertTrue("the document in force keeps it", DocumentAcceptance.check(text, Intake.APPLIED).keep)
    }

    @Test
    fun `local intake gives a document without an id one, and peer intake does not`() {
        val text = """{"version":1,"controller":{"name":"Home"},"machines":[]}"""
        val local = DocumentAcceptance.check(text, Intake.LOCAL)
        assertTrue(local.valid)
        assertTrue(local.assignsIdentity)
        val peer = DocumentAcceptance.check(text, Intake.PEER)
        assertFalse(peer.valid)
        assertEquals(listOf("controller.id", "controller.revision"), peer.errors.map { it.path })
    }

    /**
     * C10: an id the generic token grammar allows and the setup-id grammar does not is refused here,
     * as the agent refuses it, rather than sent and turned away with `bad-argument`.
     */
    @Test
    fun `a setup id and a helper action are held to their own grammars`() {
        fun setup(id: String) = """{"version":1,"controller":{"id":"$id","revision":1},"machines":[]}"""
        assertTrue(readControllerConfig(setup("setup:desktop-1")).isSuccess)
        assertTrue(readControllerConfig(setup("me@tower")).isFailure)
        assertTrue(readControllerConfig(setup("a/b")).isFailure)
        assertTrue(readControllerConfig(setup("a".repeat(65))).isFailure)

        fun helper(action: String) = """
            {"version":1,"machines":[{"id":"pi","alwaysOn":true},
              {"id":"tower","wake":{"mac":"02:00:00:00:00:01","helpers":[{"machine":"pi","action":"$action"}]}}]}
        """.trimIndent()
        assertTrue(readControllerConfig(helper("wol")).isSuccess)
        assertTrue(readControllerConfig(helper("wol:tower")).isFailure)
    }
}
