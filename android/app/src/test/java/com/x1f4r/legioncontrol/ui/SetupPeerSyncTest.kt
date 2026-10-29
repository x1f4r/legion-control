package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ConfigMetaReply
import com.x1f4r.legioncontrol.agent.ConfigProblem
import com.x1f4r.legioncontrol.agent.ConfigSetReply
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.ControllerMark
import com.x1f4r.legioncontrol.agent.ControllerMeta
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.OpSummary
import com.x1f4r.legioncontrol.agent.OperationsBlock
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.withList
import com.x1f4r.legioncontrol.data.withText
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Setup sync with one machine at a time, against a real store and a machine that answers whatever
 * the test says (contract section 9.5).
 *
 * The decision table, the admission rows, the guardrails and the push replies are pinned one case
 * at a time by the shared vectors. These are the sequences the vectors cannot hold: a read that is
 * suspended while somebody saves an edit (A15), two machines read at once, a machine that changes
 * between two requests, and the guardrails over several polls, which is where this app used to
 * retry a refused push every fifteen seconds and push into a machine in the middle of an update
 * (C7).
 */
class SetupPeerSyncTest {

    private var now = 1_000_000L
    private val store: ConfigStore = LaunchDocuments.store(MemoryPreferences()).also {
        assertNull(it.applyEnrolment(LaunchDocuments.HELD, "Test phone"))
    }
    private val services = servicesOver(store)
    private val sync = SetupPeerSync(services) { now }

    private val setupId = store.document.value!!.setupId!!
    private val original = store.document.value!!

    /** A newer copy of [of] as another peer would have published it to a machine. */
    private fun descendant(of: String = original.canonicalText, name: String) =
        LaunchDocuments.descendant(of) { root -> ControllerDocument.of(root).editMachine("pi") { it.withText("name", name) }.root }

    private fun hashOf(text: String) = ControllerDocument.parse(text).getOrThrow().hash

    private fun status(hash: String?, running: Int = 0, version: String = "3.1.0") = AgentStatus(
        ok = true,
        contract = 3,
        agentVersion = version,
        controller = ControllerMark(hash = hash, id = hash?.let { setupId }),
        operations = OperationsBlock(running = List(running) { OpSummary(id = "op-$it") }),
    )

    private fun metaOf(text: String): ConfigMetaReply {
        val document = ControllerDocument.parse(text).getOrThrow()
        return ConfigMetaReply(ok = true, meta = ControllerMeta(id = document.setupId, hash = document.hash, lineage = document.lineage), hash = document.hash)
    }

    private fun served(text: String) = ControllerFetch.Document(
        text = text,
        provenance = SetupProvenance(authority = setupId, revision = null, hash = hashOf(text)),
        replyHash = hashOf(text),
    )

    /** A machine that answers `config meta`, `config` and `config set` as the test says, and counts each. */
    private inner class FakeMachine(id: String) : MachineClient by unusable<MachineClient>("setup sync") {
        override val machine = Machine(id = id, name = id.replaceFirstChar { it.uppercase() }, endpoints = emptyList(), systems = emptyList(), wake = null)
        val calls = mutableListOf<String>()
        var meta: suspend () -> Result<ConfigMetaReply> = { error("no config meta was expected") }
        var fetch: suspend () -> Result<ControllerFetch> = { error("no config read was expected") }
        var set: suspend (String) -> Result<ConfigSetReply> = { error("no config set was expected") }

        override suspend fun configMeta(): Result<ConfigMetaReply> = meta().also { calls += "meta" }
        override suspend fun fetchSetup(): Result<ControllerFetch> = fetch().also { calls += "config" }
        override suspend fun configSet(canonicalDocument: String, setupId: String, revision: Long, replace: Boolean): Result<ConfigSetReply> {
            assertTrue("an automatic push never replaces", !replace)
            return set(canonicalDocument).also { calls += "set" }
        }

        /** A machine that holds [text] and answers every read about it. */
        fun holding(text: String) {
            meta = { Result.success(metaOf(text)) }
            fetch = { Result.success(served(text)) }
        }
    }

    private suspend fun FakeMachine.poll(status: AgentStatus, working: Boolean = false): String? =
        sync.reconcile(this, status, working, sync.state(machine.id))

    private fun stored(hash: String) = ConfigSetReply(ok = true, action = "stored", hash = hash)

    private fun refused(code: String, current: ControllerMeta? = null, problems: List<ConfigProblem>? = null) =
        ConfigSetReply(ok = false, reasonCode = code, message = "refused: $code", current = current, problems = problems)

    private fun edit(name: String) {
        assertNull(store.applyEdit(store.document.value!!.editMachine("tower") { it.withText("name", name) }, "Test phone"))
    }

    // MARK: - Adoption is a compare-and-swap (A15)

    @Test
    fun `a newer copy is read and adopted exactly as the machine stores it`() = runBlocking {
        val newer = descendant(name = "Beacon two")
        val machine = FakeMachine("pi").apply { holding(newer) }
        assertEquals("Setup updated from Pi.", machine.poll(status(hashOf(newer))))
        assertEquals(listOf("meta", "config"), machine.calls)
        assertEquals(newer, store.text.value)
        assertEquals(hashOf(newer), store.appliedHash())
    }

    @Test
    fun `an edit saved while a read was suspended stays, and the next decision sees the divergence`() = runBlocking {
        val newer = descendant(name = "Beacon two")
        val machine = FakeMachine("pi").apply { holding(newer) }
        val asked = CompletableDeferred<Unit>()
        val answer = CompletableDeferred<Unit>()
        machine.fetch = {
            asked.complete(Unit)
            answer.await()
            Result.success(served(newer))
        }

        val poll = async { machine.poll(status(hashOf(newer))) }
        asked.await()
        edit("Edited while the read was out")
        val edited = store.appliedHash()
        machine.fetch = { Result.success(served(newer)) }
        answer.complete(Unit)

        assertEquals("Pi has a copy of the setup that was edited separately. It is waiting to be merged.", poll.await())
        assertEquals("the edit is still in force", edited, store.appliedHash())
        val divergence = store.divergence.value!!
        assertEquals(hashOf(newer), divergence.theirProvenance.hash)
        assertEquals(edited, divergence.mineProvenance?.hash)
    }

    @Test
    fun `of two reads made against one head, only the first one is adopted`() = runBlocking {
        val first = descendant(name = "From the tower")
        val second = descendant(name = "From the laptop")
        val tower = FakeMachine("tower").apply { holding(first) }
        val laptop = FakeMachine("laptop").apply { holding(second) }
        val bothAsked = CompletableDeferred<Unit>()
        val release = CompletableDeferred<Unit>()
        var asking = 0
        val hold: suspend (String) -> Result<ControllerFetch> = { text ->
            if (++asking == 2) bothAsked.complete(Unit)
            release.await()
            Result.success(served(text))
        }
        tower.fetch = { hold(first) }
        laptop.fetch = { hold(second) }

        val fromTower = async { tower.poll(status(hashOf(first))) }
        val fromLaptop = async { laptop.poll(status(hashOf(second))) }
        bothAsked.await()
        laptop.fetch = { Result.success(served(second)) }
        release.complete(Unit)

        assertEquals("Setup updated from Tower.", fromTower.await())
        // The laptop's bytes were chosen against the head the tower's replaced: they never land, and
        // deciding again from the new head finds two branches.
        assertTrue(fromLaptop.await()!!.contains("edited separately"))
        assertEquals(hashOf(first), store.appliedHash())

        // The store refuses the same write on its own, whoever asks.
        assertEquals(SetupOutcome.HeadMoved, store.adoptFromMachine(second, hashOf(second), original.hash, "Laptop"))
    }

    @Test
    fun `a machine that changed between the meta read and the fetch is decided on again`() = runBlocking {
        val announced = descendant(name = "Announced")
        val later = descendant(of = announced, name = "Changed in between")
        val machine = FakeMachine("pi")
        machine.meta = { Result.success(metaOf(announced)) }
        machine.fetch = { Result.success(served(later)) }

        assertNull(machine.poll(status(hashOf(announced))))
        assertEquals("nothing is adopted from bytes the decision was not about", original.hash, store.appliedHash())

        machine.holding(later)
        assertEquals("Setup updated from Pi.", machine.poll(status(hashOf(later))))
        assertEquals(hashOf(later), store.appliedHash())
    }

    @Test
    fun `bytes an older agent rebuilt in another spelling are never adopted, until the agent changes`() = runBlocking {
        val newer = descendant(name = "Beacon two")
        val respelled = newer.replace("\n", "\r\n").replace("  ", "    ")
        val machine = FakeMachine("pi")
        machine.meta = { Result.success(metaOf(newer)) }
        machine.fetch = { Result.success(served(respelled).copy(replyHash = hashOf(newer))) }

        assertTrue(machine.poll(status(hashOf(newer), version = "3.0.1"))!!.contains("has to be updated"))
        assertEquals(listOf("meta", "config"), machine.calls)
        // Remembered for this pair of hashes and this agent, so it is not read again on every poll.
        assertNull(machine.poll(status(hashOf(newer), version = "3.0.1")))
        assertEquals(listOf("meta", "config"), machine.calls)
        // An updated agent is asked again.
        machine.holding(newer)
        assertEquals("Setup updated from Pi.", machine.poll(status(hashOf(newer), version = "3.1.0")))
    }

    // MARK: - The guardrails (C7)

    @Test
    fun `a push refused for a restricted key is held until the setup changes or a person asks`() = runBlocking {
        val machine = FakeMachine("pi")
        machine.set = { Result.success(refused("restricted")) }

        val line = machine.poll(status(null))!!
        assertTrue(line, line.contains("not restricted"))
        assertEquals(line, sync.hold("pi"))
        assertEquals(1, machine.calls.count { it == "set" })

        // Every later poll leaves it alone, however much time passes.
        now += 3_600_000L
        assertNull(machine.poll(status(null)))
        assertEquals(1, machine.calls.count { it == "set" })

        // Try again sends it once more.
        sync.retry("pi")
        assertNull(sync.hold("pi"))
        machine.poll(status(null))
        assertEquals(2, machine.calls.count { it == "set" })

        // And a new revision starts afresh.
        edit("A new revision")
        machine.set = { canonical -> Result.success(stored(hashOf(canonical))) }
        assertEquals("The setup was sent to Pi. It was not carrying a setup yet.", machine.poll(status(null)))
        assertEquals(3, machine.calls.count { it == "set" })
    }

    @Test
    fun `a push refused as invalid is held, with the first problem the machine named`() = runBlocking {
        val machine = FakeMachine("pi")
        machine.set = {
            Result.success(refused("config-invalid", problems = listOf(ConfigProblem(path = "machines[0].endpoints[0].host", message = "is not a host"))))
        }
        assertEquals("Pi refused this revision of the setup: machines[0].endpoints[0].host: is not a host.", machine.poll(status(null)))
        now += 120_000L
        machine.poll(status(null))
        assertEquals(1, machine.calls.count { it == "set" })
    }

    @Test
    fun `a failed push backs off for sixty seconds for the same setup`() = runBlocking {
        val machine = FakeMachine("pi")
        machine.set = { Result.failure(AgentFailure.Unreachable("Pi", "did not answer")) }
        assertNull(machine.poll(status(null)))
        now += 59_000L
        machine.poll(status(null))
        assertEquals("nothing within the back-off", 1, machine.calls.count { it == "set" })
        now += 1_000L
        machine.poll(status(null))
        assertEquals(2, machine.calls.count { it == "set" })
    }

    @Test
    fun `an unanswered push may have landed, and is said so`() = runBlocking {
        val machine = FakeMachine("pi")
        machine.set = { Result.failure(AgentFailure.Unreachable("Pi", "dropped", stage = DispatchStage.AMBIGUOUS)) }
        assertTrue(machine.poll(status(null))!!.contains("may or may not have reached Pi"))
    }

    @Test
    fun `nothing is pushed while a change runs on the machine, and reads still go out`() = runBlocking {
        val machine = FakeMachine("pi")
        machine.set = { canonical -> Result.success(stored(hashOf(canonical))) }

        assertNull(machine.poll(status(null, running = 1)))
        assertNull(machine.poll(status(null), working = true))
        assertTrue(machine.calls.isEmpty())

        // A machine with a newer copy is still read while it is busy: reading never changes it.
        val newer = descendant(name = "Beacon two")
        val busy = FakeMachine("tower").apply { holding(newer) }
        assertEquals("Setup updated from Tower.", busy.poll(status(hashOf(newer), running = 2), working = true))

        // Once nothing runs, the push goes.
        assertTrue(machine.poll(status(null))!!.startsWith("The setup was sent to Pi."))
    }

    @Test
    fun `a push refused for a lock waits for a status read after the refusal`() = runBlocking {
        val machine = FakeMachine("pi")
        machine.set = { Result.success(refused("lock-held")) }
        assertTrue(machine.poll(status(null))!!.contains("busy with a change"))

        // A status from the same moment as the refusal says nothing about whether the lock is gone.
        assertNull(machine.poll(status(null)))
        assertEquals(1, machine.calls.count { it == "set" })

        now += 1L
        machine.set = { canonical -> Result.success(stored(hashOf(canonical))) }
        assertTrue(machine.poll(status(null))!!.startsWith("The setup was sent to Pi."))
        assertEquals(2, machine.calls.count { it == "set" })
    }

    @Test
    fun `one setup request is in flight per machine`() = runBlocking {
        val machine = FakeMachine("pi")
        val asked = CompletableDeferred<Unit>()
        val answer = CompletableDeferred<ConfigSetReply>()
        machine.set = {
            asked.complete(Unit)
            Result.success(answer.await())
        }
        val first = async { machine.poll(status(null)) }
        asked.await()
        assertNull("a second poll while the first push is out sends nothing", machine.poll(status(null)))
        answer.complete(stored(store.appliedHash()!!))
        first.await()
        assertEquals(1, machine.calls.count { it == "set" })
    }

    @Test
    fun `a stale-revision refusal is filed and decided on at once`() = runBlocking {
        // The machine looked empty, and by the time the push arrived it held a newer copy of this
        // setup. The refusal says so, and the copy is fetched in the same round.
        val newer = descendant(name = "Beacon two")
        val document = ControllerDocument.parse(newer).getOrThrow()
        val machine = FakeMachine("pi").apply { holding(newer) }
        machine.set = { Result.success(refused("stale-revision", current = ControllerMeta(id = setupId, hash = document.hash, lineage = document.lineage))) }

        assertEquals("Setup updated from Pi.", machine.poll(status(null)))
        assertEquals(listOf("set", "config"), machine.calls)
        assertEquals(document.hash, store.appliedHash())
    }

    @Test
    fun `a document in force with an error is published nowhere, and still takes a newer copy`() = runBlocking {
        val prefs = MemoryPreferences()
        val broken = LaunchDocuments.HELD.replace("\"host\": \"192.0.2.40\"", "\"host\": \"192.0.2.40,nas\"")
        prefs.values["controller-config"] = broken
        prefs.values["controller-hash"] = hashOf(broken)
        val brokenStore = LaunchDocuments.store(prefs)
        assertEquals(listOf("schema"), brokenStore.acceptance.value!!.errors.map { it.code })
        val brokenSync = SetupPeerSync(servicesOver(brokenStore)) { now }

        val machine = FakeMachine("tower")
        assertNull(brokenSync.reconcile(machine, status(null), false, brokenSync.state("tower")))
        assertTrue("nothing is pushed from a document with an error", machine.calls.isEmpty())

        val fixed = LaunchDocuments.descendant(broken) { root ->
            ControllerDocument.of(root).editMachine("pi") { pi ->
                pi.withList("endpoints") { endpoints -> endpoints.map { it.withText("host", "192.0.2.40") } }
            }.root
        }
        machine.holding(fixed)
        assertEquals("Setup updated from Tower.", brokenSync.reconcile(machine, status(hashOf(fixed)), false, brokenSync.state("tower")))
        assertTrue(brokenStore.acceptance.value!!.errors.isEmpty())
    }
}
