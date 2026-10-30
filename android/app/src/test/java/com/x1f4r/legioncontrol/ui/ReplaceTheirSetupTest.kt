package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentJson
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ConfigProblem
import com.x1f4r.legioncontrol.agent.ConfigSetReply
import com.x1f4r.legioncontrol.agent.ControllerMark
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.SetupPeerState
import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.OperationStore
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.withText
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

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
    fun `a refusal no retry can cure holds this hash for the machine, said with Try again as a push's is`() {
        // The hold was recorded and its sentence was not, so the push of this hash that the hold
        // stops afterwards was stopped without a word, and nothing on the page offered Try again.
        assertNull(sync.hold("tower"))
        replace(Tower(fixture("config.set.restricted-launch.json")))
        assertEquals(
            "Tower takes this revision only from a device whose key is not restricted. A device with full access sends it there on its next check.",
            sync.hold("tower"),
        )
        sync.retry("tower")
        assertNull(sync.hold("tower"))
        assertTrue("Try again lets the push go once more", sync.state("tower").held.isEmpty())

        val invalid = ConfigSetReply(
            ok = false,
            reasonCode = "config-invalid",
            problems = listOf(ConfigProblem(path = "machines[0].wake.helpers[0]", message = "the helper names no machine")),
        )
        replace(Tower(Result.success(invalid)))
        assertEquals("Tower refused this revision of the setup: machines[0].wake.helpers[0]: the helper names no machine.", sync.hold("tower"))

        // A refusal a retry can cure holds nothing.
        sync.retry("tower")
        replace(Tower(fixture("config.set.bad-argument.json")))
        assertNull(sync.hold("tower"))
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
    fun `a second Replace while the first is out sends nothing, and the first one's reply is the one shown`() = runBlocking {
        // Nothing stopped the second press: it sent a second `--replace`, and whichever reply came
        // back last said what had happened, over the other.
        val answer = CompletableDeferred<Result<ConfigSetReply>>()
        val tower = object : MachineClient by Tower() {
            val sent = mutableListOf<String>()
            override suspend fun configSet(canonicalDocument: String, setupId: String, revision: Long, replace: Boolean): Result<ConfigSetReply> {
                sent += canonicalDocument
                return answer.await()
            }
        }
        val clash = question()
        val first = async { sync.replace(tower, clash, otherHash) }
        yield()
        assertEquals(1, tower.sent.size)

        val second = sync.replace(tower, clash, otherHash)
        assertTrue(second.toString(), second is SetupPeerSync.Replaced.NotDone)
        assertFalse("waiting for an answer is not an error", second.isError)
        assertTrue(second.line, second.line.contains("still out to Tower"))
        assertEquals("nothing more was sent", 1, tower.sent.size)
        assertEquals("the question is still the one being answered", clash, store.identityClash.value)

        answer.complete(Result.success(ConfigSetReply(ok = true, action = "replaced", hash = mine.hash)))
        assertEquals(SetupPeerSync.Replaced.Done("Tower now carries this setup."), first.await())
        assertNull(store.identityClash.value)
    }

    @Test
    fun `a machine that moved on since the question was asked is not overwritten`() = runBlocking {
        val tower = Tower()
        val newer = "0".repeat(64)
        val clash = question()
        val moved = sync.replace(tower, clash, newer)
        assertEquals(
            SetupPeerSync.Replaced.Stale("Tower holds another document now, not the one shown. Nothing was sent; it is compared again at its next check."),
            moved,
        )
        assertTrue("nothing was sent", tower.sent.isEmpty())
        // Closing it at once took the page Replace was pressed on away with that sentence.
        assertEquals("the question stays, saying so", clash, store.identityClash.value)

        val already = question()
        assertEquals(SetupPeerSync.Replaced.Stale("Tower carries this setup already."), sync.replace(tower, already, mine.hash))
        assertTrue(tower.sent.isEmpty())
        assertEquals(already, store.identityClash.value)

        // Another machine's status is not the one it waits for.
        val pi = object : MachineClient by unusable<MachineClient>("Replace") {
            override val machine = Machine(id = "pi", name = "Pi", endpoints = emptyList(), systems = emptyList(), wake = null)
        }
        assertNull(sync.reconcile(pi, holding(mine.hash), localMutation = false, state = sync.state("pi")))
        assertEquals(already, store.identityClash.value)

        // Tower's is: the question closes, and the status decides again from what both hold now.
        assertNull(sync.reconcile(tower, holding(mine.hash), localMutation = false, state = sync.state("tower")))
        assertNull(store.identityClash.value)
    }

    @Test
    fun `a setup edited here since the question was asked is not sent over the other one`() = runBlocking {
        val tower = Tower()
        val clash = question()
        assertNull(store.applyEdit(mine.editMachine("tower") { it.withText("name", "Edited meanwhile") }, "Test phone"))
        assertEquals(SetupPeerSync.Replaced.Stale("The setup here changed while this was open; comparing again."), sync.replace(tower, clash, otherHash))
        assertTrue(tower.sent.isEmpty())
        assertEquals("the question stays, saying so, until Tower's next status", clash, store.identityClash.value)
    }

    // MARK: - What Replace is checked against

    private val directory: File = Files.createTempDirectory("replace").toFile()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
    private val operations = OperationCenter(OperationStore(File(directory, "operations.json")), scope, SilentNotifier, clock = { 0L })

    @After
    fun tearDown() {
        scope.cancel()
        directory.deleteRecursively()
    }

    /** A status of a machine holding [hash], of this setup's id when it is this setup's document. */
    private fun holding(hash: String?) = AgentStatus(
        ok = true,
        contract = 3,
        agentVersion = "3.1.0",
        controller = ControllerMark(hash = hash, id = if (hash == mine.hash) mine.setupId else "setup-0b4f6a12-3c5d-4e7f-8a9b-0c1d2e3f4a5b"),
    )

    /** The page of a machine whose every status says it holds [holds], read as the app reads it. */
    private inner class Shown(id: String) {
        var holds: String? = null
        private val client = object : MachineClient by unusable<MachineClient>("reading a status") {
            override val machine = Machine(id = id, name = id.replaceFirstChar { it.uppercase() }, endpoints = emptyList(), systems = emptyList(), wake = null)
            override val abilities = AgentAbilities(contract = 3)
            override val route: Route? get() = null
            override suspend fun status(): Result<AgentStatus> = Result.success(holding(holds))
        }
        private val setup = object : SetupSync by unusable<SetupSync>("reading a status") {
            override fun state(machineId: String) = SetupPeerState()
            override suspend fun reconcile(client: MachineClient, reply: AgentStatus, localMutation: Boolean, state: SetupPeerState): String? = null
        }
        val model = MachineModel(client, RememberedSettings(MemoryPreferences()), scope, operations, setup, unusable("reading a status"))
    }

    @Test
    fun `Replace is checked against the latest status of the machine the question came from, found by id`() = runBlocking {
        val pi = Shown("pi").apply { holds = mine.hash }
        val tower = Shown("tower").apply { holds = otherHash }
        val machines = listOf(pi.model, tower.model)
        assertNull("no status read yet says nothing", machines.heldBy("tower"))

        machines.forEach { it.refreshNow() }
        assertEquals("Tower's own status, not the first machine's", otherHash, machines.heldBy("tower"))
        val sending = Tower(Result.success(ConfigSetReply(ok = true, action = "replaced", hash = mine.hash)))
        assertEquals(SetupPeerSync.Replaced.Done("Tower now carries this setup."), sync.replace(sending, question(), machines.heldBy("tower")))
        assertEquals("what it named is the copy shown, so --replace went", 1, sending.sent.size)

        // Its next status names a copy nobody here has seen, and that is what Replace goes by.
        val newer = "0".repeat(64)
        tower.holds = newer
        tower.model.refreshNow()
        assertEquals(newer, machines.heldBy("tower"))
        val waiting = Tower()
        val outdated = sync.replace(waiting, question(), machines.heldBy("tower"))
        assertTrue(outdated.toString(), outdated is SetupPeerSync.Replaced.Stale)
        assertTrue(waiting.sent.isEmpty())
    }
}
