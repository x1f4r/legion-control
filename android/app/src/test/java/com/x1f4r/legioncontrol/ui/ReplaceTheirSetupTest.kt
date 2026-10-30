package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.ConfigProblem
import com.x1f4r.legioncontrol.agent.ConfigSetReply
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.withText
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Replace on the different-setup question (contract sections 5.6 and 9.5), against a real store and
 * a machine that answers whatever the test says.
 *
 * `--replace` comes back as a reply whatever became of it: a refusal, a push the machine's lock held
 * back and a write the agent failed part way through are all replies, and this app used to take
 * each of them for "the machine now carries this setup" and close the question, which then came back
 * at the next status. Only a reply that stored this document closes it.
 */
class ReplaceTheirSetupTest {

    private val store: ConfigStore = LaunchDocuments.store(MemoryPreferences()).also {
        assertNull(it.applyEnrolment(LaunchDocuments.HELD, "Test phone"))
    }
    private val services = servicesOver(store)
    private val sync = SetupPeerSync(services) { 1_000_000L }
    private val mine = store.document.value!!

    private val other = LaunchDocuments.HELD.replace(mine.setupId!!, "setup-0b4f6a12-3c5d-4e7f-8a9b-0c1d2e3f4a5b")
    private val otherHash = ControllerDocument.parse(other).getOrThrow().hash

    /** Tower, carrying [other], answering a `config set` with [answer]. */
    private inner class Tower(private val answer: Result<ConfigSetReply>? = null) : MachineClient by unusable<MachineClient>("Replace") {
        override val machine = Machine(id = "tower", name = "Tower", endpoints = emptyList(), systems = emptyList(), wake = null)
        val sent = mutableListOf<String>()

        override suspend fun configSet(canonicalDocument: String, setupId: String, revision: Long, replace: Boolean): Result<ConfigSetReply> {
            assertTrue("Replace is the one place --replace is sent", replace)
            sent += canonicalDocument
            return answer ?: error("nothing was to be sent")
        }
    }

    private fun question(): SetupIdentityClash {
        store.recordIdentityClash("tower", "Tower", "This device and Tower carry two different setups.", other)
        return store.identityClash.value!!
    }

    private fun reply(json: String): Result<ConfigSetReply> = Result.success(AgentJson.decodeFromString(ConfigSetReply.serializer(), json))

    private fun fixture(name: String): Result<ConfigSetReply> = reply(ContractCorpus.file("fixtures/$name").readText())

    private fun replace(tower: Tower, latest: String? = otherHash): SetupPeerSync.Replaced = runBlocking {
        val clash = question()
        sync.replace(tower, clash, latest).also { assertEquals(mine.canonicalText, tower.sent.single()) }
    }

    /** What a person sees, and that the question is still the one they were answering. */
    private fun assertStays(replaced: SetupPeerSync.Replaced, isError: Boolean, vararg says: String) {
        assertTrue(replaced.toString(), replaced is SetupPeerSync.Replaced.NotDone)
        assertEquals(isError, replaced.isError)
        assertFalse(replaced.line, replaced.line.contains("now carries"))
        says.forEach { assertTrue("\"${replaced.line}\" says $it", replaced.line.contains(it)) }
        assertEquals("the question stays open", otherHash, store.identityClash.value?.theirProvenance?.hash)
        assertEquals("nothing changed here", mine.hash, store.appliedHash())
    }

    @Test
    fun `a restricted key's refusal is not a replacement, and the question stays`() {
        val replaced = replace(Tower(fixture("config.set.restricted-launch.json")))
        assertStays(replaced, isError = true, "was not replaced", "restricted")
    }

    @Test
    fun `a push the lock held back says it was not replaced yet, whether ok is true or false`() {
        assertStays(
            replace(Tower(reply("""{"ok":true,"contract":3,"action":"conflict","reasonCode":"operation-in-progress","message":"an update holds the lock"}"""))),
            isError = false,
            "was not replaced yet: an update holds the lock.",
        )
        assertStays(replace(Tower(fixture("config.set.locked.json"))), isError = false, "was not replaced yet")
    }

    @Test
    fun `an invalid document and a bad argument are refusals, said in the agent's words`() {
        val invalid = ConfigSetReply(
            ok = false,
            reasonCode = "config-invalid",
            message = "the document breaks a rule",
            problems = listOf(ConfigProblem(path = "machines[0].wake.helpers[0]", message = "the helper names no machine")),
        )
        assertStays(replace(Tower(Result.success(invalid))), isError = true, "machines[0].wake.helpers[0]: the helper names no machine.")
        assertStays(replace(Tower(fixture("config.set.bad-argument.json"))), isError = true, "was not replaced")
    }

    @Test
    fun `a write the agent failed part way through, or a link lost after sending, may or may not have replaced it`() {
        assertStays(
            replace(Tower(reply("""{"ok":false,"contract":3,"reasonCode":"internal","message":"ENOSPC"}"""))),
            isError = true,
            "may or may not have been replaced: ENOSPC.",
        )
        val lost = AgentFailure.Unreachable("Tower", "connection reset", stage = DispatchStage.AMBIGUOUS)
        assertStays(replace(Tower(Result.failure(lost))), isError = true, "may or may not have been replaced")
        val never = AgentFailure.Unreachable("Tower", "no route to host")
        assertStays(replace(Tower(Result.failure(never))), isError = true, "was not replaced: Tower did not answer.")
    }

    @Test
    fun `only a reply that stored this document closes the question`() {
        val replaced = replace(Tower(Result.success(ConfigSetReply(ok = true, action = "replaced", hash = mine.hash))))
        assertEquals(SetupPeerSync.Replaced.Done("Tower now carries this setup."), replaced)
        assertNull(store.identityClash.value)

        // One that names another document did not store this one, whatever it says about itself.
        assertStays(
            replace(Tower(Result.success(ConfigSetReply(ok = true, action = "replaced", hash = otherHash)))),
            isError = true,
            "may or may not have been replaced",
        )
    }

    @Test
    fun `a machine that moved on since the question was asked is not overwritten`() = runBlocking {
        val tower = Tower()
        val newer = "0".repeat(64)
        val moved = sync.replace(tower, question(), newer)
        assertEquals(
            SetupPeerSync.Replaced.Stale("Tower holds another document now, not the one shown. Nothing was sent; it is compared again at its next check."),
            moved,
        )
        assertTrue("nothing was sent", tower.sent.isEmpty())
        assertNull("the next status asks again, about what it holds then", store.identityClash.value)

        assertEquals(SetupPeerSync.Replaced.Stale("Tower carries this setup already."), sync.replace(tower, question(), mine.hash))
        assertTrue(tower.sent.isEmpty())
    }

    @Test
    fun `a setup edited here since the question was asked is not sent over the other one`() = runBlocking {
        val tower = Tower()
        val clash = question()
        assertNull(store.applyEdit(mine.editMachine("tower") { it.withText("name", "Edited meanwhile") }, "Test phone"))
        assertEquals(SetupPeerSync.Replaced.Stale("The setup here changed while this was open; comparing again."), sync.replace(tower, clash, otherHash))
        assertTrue(tower.sent.isEmpty())
    }
}
