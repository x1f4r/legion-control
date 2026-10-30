package com.x1f4r.legioncontrol.ui

import android.net.Network
import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.HelperState
import com.x1f4r.legioncontrol.agent.HoldChoice
import com.x1f4r.legioncontrol.agent.InterruptRequest
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.OpResult
import com.x1f4r.legioncontrol.agent.SetupPeerState
import com.x1f4r.legioncontrol.agent.SignedTarball
import com.x1f4r.legioncontrol.agent.WakeHelper
import com.x1f4r.legioncontrol.agent.WakeTarget
import com.x1f4r.legioncontrol.agent.readinessSchedule
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.OperationStore
import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.LocalAddress
import com.x1f4r.legioncontrol.net.WakeSend
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/**
 * One press of Wake, walked against machines that answer whatever the test says (contract section
 * 10.2), with the operation history on disk as it is on a phone.
 *
 * The plan itself is pinned by the shared vectors. What they cannot hold is time and memory: how
 * long readiness waits, and a wake hold that has to outlive the app being closed.
 */
class WakeWalkTest {

    private val directory: File = Files.createTempDirectory("wake").toFile()
    private val file = File(directory, "operations.json")
    private val scopes = mutableListOf<CoroutineScope>()

    @After
    fun tearDown() {
        scopes.forEach { it.cancel() }
        directory.deleteRecursively()
    }

    /**
     * An operation center over [file], as a fresh process would build one. On a clock that stands
     * still, because the phone's uptime clock is not there off a device.
     */
    private fun center(): OperationCenter {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined).also { scopes += it }
        return OperationCenter(OperationStore(file), scope, SilentNotifier, clock = { 0L })
    }

    private val home = SiteConfig(id = "home", name = "Home", lanPrefixes = listOf("192.168.1."), broadcast = listOf("192.168.1.255"))

    private val tower = Machine(
        id = "tower",
        name = "Tower",
        endpoints = emptyList(),
        systems = emptyList(),
        siteId = "home",
        wake = WakeTarget(
            mac = "02:00:00:00:00:01",
            broadcasts = emptyList(),
            ports = emptyList(),
            lanPrefix = null,
            siteId = "home",
            helpers = listOf(WakeHelper("pi", "wake-tower"), WakeHelper("nas", "wake-tower")),
        ),
    )

    /** The machine being woken: it counts packets, and never answers unless the test says so. */
    private inner class Target : MachineClient by unusable<MachineClient>("waking the target") {
        override val machine = tower
        override val abilities = AgentAbilities(contract = 3)
        val sends = mutableListOf<Pair<List<String>, List<Int>>>()
        override suspend fun sendWakePacket(broadcasts: List<String>, ports: List<Int>, via: Network?): WakeSend {
            sends += broadcasts to ports
            return WakeSend(broadcasts.size * ports.size * 3, null)
        }
    }

    /** A helper that runs its action and answers `op` as the test says, counting each run. */
    private inner class Helper(id: String) : MachineClient by unusable<MachineClient>("asking a helper") {
        override val machine = Machine(id = id, name = id.replaceFirstChar { it.uppercase() }, endpoints = emptyList(), systems = emptyList(), wake = null)
        override val abilities = AgentAbilities(contract = 3)
        val runs = mutableListOf<String>()
        var answer: (String) -> Result<AgentActionResult> = { id -> Result.success(ran(id)) }
        var op: (String) -> Result<OpRecord> = { Result.failure(AgentFailure.Unreachable(machine.name, "no route")) }

        override suspend fun run(actionId: String, request: InterruptRequest, operationId: String?): Result<AgentActionResult> {
            runs += actionId
            return answer(operationId!!)
        }

        override suspend fun operation(operationId: String, waitSeconds: Int?): Result<OpRecord> = op(operationId)
    }

    private fun ran(id: String) = AgentActionResult(ok = true, contract = 3, op = finished(id, "ran"))

    private fun finished(id: String, action: String, reasonCode: String? = null) =
        OpRecord(id = id, state = "finished", result = OpResult(ok = reasonCode == null, action = action, reasonCode = reasonCode))

    private val target = Target()
    private val pi = Helper("pi")
    private val nas = Helper("nas")

    private val command = HelperState(HelperState.Kind.READY, mapOf("wake-tower" to "command"))
    private val wol = HelperState(HelperState.Kind.READY, mapOf("wake-tower" to "wol"))

    /** The rest of the fleet, as a press finds it. */
    private inner class Fleet : FleetContext by unusable<FleetContext>("the wake walk") {
        var addresses = listOf(LocalAddress("198.51.100.7", 24))
        val states = mutableMapOf("pi" to command, "nas" to command)
        val reads = mutableListOf<String>()

        /** Helpers taken out of the setup. */
        val removed = mutableSetOf<String>()

        override val machines: List<Machine> get() = listOf(tower, pi.machine, nas.machine).filter { it.id !in removed }
        override val sites: List<SiteConfig> get() = listOf(home)
        override val localAddresses: List<LocalAddress> get() = addresses
        override val currentSite: String? get() = null
        override val deviceName: String get() = "this phone"
        override val agentBundle: SignedTarball? get() = null
        override fun client(machineId: String): MachineClient? =
            listOf(pi, nas).firstOrNull { it.machine.id == machineId && machineId !in removed }
        override fun helperState(machineId: String): HelperState = states[machineId] ?: HelperState.UNKNOWN
        override suspend fun readHelper(machineId: String): HelperState {
            reads += machineId
            return helperState(machineId)
        }
    }

    private val fleet = Fleet()

    /** A clock that only moves when the walk sleeps, and the times at which the target was polled. */
    private var now = 0L
    private val polls = mutableListOf<Long>()

    private fun walk(operations: OperationCenter, answersOnPoll: Int? = null) = WakeWalk(
        target = target,
        fleet = fleet,
        operations = operations,
        poll = {
            polls += now
            answersOnPoll != null && polls.size >= answersOnPoll
        },
        progress = {},
        clock = { now },
        sleep = { now += it },
    )

    // MARK: - The hold scenario

    @Test
    fun `an ambiguous command holds every other command helper, across a restart, until its record is read`() = runBlocking {
        // The pi takes the command and the link drops with it running: nobody knows whether it ran.
        pi.answer = { Result.failure(AgentFailure.Unreachable("Pi", "connection reset", stage = DispatchStage.AMBIGUOUS)) }
        val first = walk(center()).run()
        assertFalse(first.answered)
        assertEquals(listOf("wake-tower"), pi.runs)
        assertEquals("the nas's command must not run beside one that may still be going", emptyList<String>(), nas.runs)

        // The app is closed and opened again, and the pi is out of reach. The hold is on disk.
        val restarted = center()
        assertEquals(1, restarted.wakeHolds("tower").size)
        fleet.states["pi"] = HelperState(HelperState.Kind.ASLEEP)
        val second = walk(restarted).run()
        assertFalse(second.answered)
        assertEquals("a helper last seen asleep is not run", listOf("wake-tower"), pi.runs)
        assertEquals("a command helper still waits", emptyList<String>(), nas.runs)
        assertTrue(second.lines.any { it.contains("may still be running") })

        // A declared wol action is a packet, and sending a packet twice is sending it once.
        fleet.states["nas"] = wol
        walk(restarted).run()
        assertEquals(listOf("wake-tower"), nas.runs)

        // The pi is back, and its record says the command finished. That releases the hold, and the
        // next press runs the nas's command again like any other.
        fleet.states["pi"] = command
        fleet.states["nas"] = command
        pi.op = { id -> Result.success(finished(id, "failed", "not-running")) }
        pi.answer = { id -> Result.success(AgentActionResult(ok = false, contract = 3, op = finished(id, "failed", "not-running"))) }
        val third = walk(restarted).run()
        assertTrue(restarted.wakeHolds("tower").isEmpty())
        assertEquals(listOf("wake-tower", "wake-tower"), nas.runs)
        assertFalse(third.answered)
    }

    @Test
    fun `trying the other helpers asks only the others, and does not run the held command again`() = runBlocking {
        // The button said the other helpers, and the phone used to release every hold and walk the
        // whole plan, so the pi's command ran again beside the first: with a command that toggles a
        // smart plug, the second run undoes the first.
        pi.answer = { Result.failure(AgentFailure.Unreachable("Pi", "connection reset", stage = DispatchStage.AMBIGUOUS)) }
        val operations = center()
        walk(operations).run()
        assertEquals(1, operations.wakeHolds("tower").size)
        pi.answer = { id -> Result.success(ran(id)) }

        // Running the held command again is not offered while another helper could be asked.
        val refused = walk(operations).run(HoldChoice.RUN_AGAIN)
        assertEquals(otherHelperFirst("Tower"), refused.refusal)
        assertEquals(1, operations.wakeHolds("tower").size)

        val ending = walk(operations).run(HoldChoice.OTHER_HELPERS)
        assertEquals("the held command is not run a second time", listOf("wake-tower"), pi.runs)
        assertEquals(listOf("wake-tower"), nas.runs)
        assertTrue("the next wake asks every helper", operations.wakeHolds("tower").isEmpty())
        assertTrue(ending.lines.toString(), ending.lines.any { it.startsWith("Pi was not asked again: its \"wake-tower\" was run to wake Tower earlier") })
    }

    @Test
    fun `with no other helper the hold stands, and only running the held command again goes past it`() = runBlocking {
        pi.answer = { Result.failure(AgentFailure.Unreachable("Pi", "connection reset", stage = DispatchStage.AMBIGUOUS)) }
        val operations = center()
        walk(operations).run()
        pi.answer = { id -> Result.success(ran(id)) }
        fleet.states["nas"] = HelperState(HelperState.Kind.ASLEEP)

        // Asking for the other helpers asks nobody new, so nothing is released.
        val refused = walk(operations).run(HoldChoice.OTHER_HELPERS)
        assertEquals(noOtherHelper("Tower"), refused.refusal)
        assertFalse(refused.tried)
        assertEquals(1, operations.wakeHolds("tower").size)
        assertEquals(listOf("wake-tower"), pi.runs)
        assertEquals("Run Pi's command again", runAgainLabel(listOf("Pi")))
        assertTrue(runAgainWarning(listOf("Pi"), "Tower").contains("can run it a second time beside the first"))

        walk(operations).run(HoldChoice.RUN_AGAIN)
        assertEquals("run again, because a person chose it knowing that", listOf("wake-tower", "wake-tower"), pi.runs)
        assertEquals(emptyList<String>(), nas.runs)
        assertTrue(operations.wakeHolds("tower").isEmpty())
    }

    @Test
    fun `the target answering releases a hold recorded before it`() = runBlocking {
        pi.answer = { Result.failure(AgentFailure.Unreachable("Pi", "connection reset", stage = DispatchStage.AMBIGUOUS)) }
        val operations = center()
        walk(operations).run()
        val hold = operations.wakeHolds("tower").single()

        // An answer to a status asked for before the hold was recorded says nothing about it.
        operations.releaseWakeHolds("tower", before = hold.startedAt, reason = "Tower answered.")
        assertEquals(listOf(hold.id), operations.wakeHolds("tower").map { it.id })
        operations.releaseWakeHolds("nas", before = Long.MAX_VALUE, reason = "Nas answered.")
        assertEquals("only the machine being woken releases its holds", 1, operations.wakeHolds("tower").size)

        operations.releaseWakeHolds("tower", before = hold.startedAt + 1, reason = "Tower answered.")
        assertTrue(operations.wakeHolds("tower").isEmpty())
        val held = operations.records.value.single { it.machineId == "pi" }
        assertFalse("the record is still unresolved, and still followed", held.isOver)
    }

    @Test
    fun `a held helper leaving the setup takes its hold with it`() = runBlocking {
        pi.answer = { Result.failure(AgentFailure.Unreachable("Pi", "connection reset", stage = DispatchStage.AMBIGUOUS)) }
        walk(center()).run()
        assertEquals(1, center().wakeHolds("tower").size)

        // The pi is taken out of the setup: its records go, and nothing holds the nas back.
        fleet.removed += "pi"
        OperationStore(file).retainOnly(setOf("tower", "nas"))
        val operations = center()
        assertTrue(operations.wakeHolds("tower").isEmpty())
        val ending = walk(operations).run()
        assertEquals(listOf("wake-tower"), pi.runs)
        assertEquals(listOf("wake-tower"), nas.runs)
        assertTrue(ending.lines.any { it.contains("is not a machine in this setup") })
    }

    @Test
    fun `an unread helper is read once just before its step`() = runBlocking {
        fleet.states.remove("pi")
        fleet.states.remove("nas")
        walk(center()).run()
        assertEquals(listOf("pi", "nas"), fleet.reads)
        assertEquals("still unknown after its read, it is of no use", emptyList<String>(), pi.runs)
    }

    // MARK: - The way past a hold the page offers

    /** Tower's page, as the app builds it over [operations]. */
    private fun page(operations: OperationCenter) = MachineModel(
        client = target,
        settings = RememberedSettings(MemoryPreferences()),
        scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined).also { scopes += it },
        operations = operations,
        setup = object : SetupSync by unusable<SetupSync>("the way past a hold") {
            override fun state(machineId: String) = SetupPeerState()
        },
        fleet = fleet,
    )

    @Test
    fun `the page offers the other helpers while one could be asked, and the held command again only when none could`() = runBlocking {
        // Which of the two the page shows is what decides whether a held command can run twice, and
        // nothing pinned it: only the walk each of them starts was tested.
        val operations = center()
        val tower = page(operations)
        assertNull("nothing is held, so there is nothing to go past", tower.wakeHoldOffer)

        pi.answer = { Result.failure(AgentFailure.Unreachable("Pi", "connection reset", stage = DispatchStage.AMBIGUOUS)) }
        walk(operations).run()
        assertEquals("the nas could be asked instead", MachineModel.WakeHoldOffer.OtherHelpers, tower.wakeHoldOffer)

        fleet.states["nas"] = HelperState(HelperState.Kind.ASLEEP)
        assertEquals(
            MachineModel.WakeHoldOffer.RunAgain(runAgainLabel(listOf("Pi")), runAgainWarning(listOf("Pi"), "Tower")),
            tower.wakeHoldOffer,
        )

        // With the held helper out of reach too, neither way would ask anybody, so neither is offered.
        fleet.states["pi"] = HelperState(HelperState.Kind.ASLEEP)
        assertNull(tower.wakeHoldOffer)

        fleet.states["pi"] = command
        operations.releaseWakeHolds("tower", before = Long.MAX_VALUE, reason = "Tower answered.")
        assertNull("a released hold offers nothing", tower.wakeHoldOffer)
    }

    // MARK: - Readiness

    @Test
    fun `after a send the target is polled at 5 s and then at growing intervals for 120 s`() = runBlocking {
        fleet.addresses = listOf(LocalAddress("192.168.1.23", 24))
        fleet.states["pi"] = HelperState(HelperState.Kind.ASLEEP)
        fleet.states["nas"] = HelperState(HelperState.Kind.ASLEEP)
        val ending = walk(center()).run()

        assertEquals(listOf(listOf("192.168.1.255") to listOf(9, 7)), target.sends)
        assertEquals(readinessSchedule(sentAnything = true), polls)
        assertEquals(listOf(5_000L, 13_000L, 25_800L, 45_800L, 65_800L, 85_800L, 105_800L), polls)
        assertFalse(ending.answered)
    }

    @Test
    fun `the target answering ends the wake, and nothing after it runs`() = runBlocking {
        fleet.addresses = listOf(LocalAddress("192.168.1.23", 24))
        val ending = walk(center(), answersOnPoll = 3).run()

        assertTrue(ending.answered)
        assertEquals(listOf(5_000L, 13_000L, 25_800L), polls)
        assertEquals(emptyList<String>(), pi.runs)
    }

    @Test
    fun `a helper that ran is followed by readiness, and one that did not answer moves on`() = runBlocking {
        val ending = walk(center()).run()
        assertEquals(listOf("wake-tower"), pi.runs)
        assertEquals(listOf("wake-tower"), nas.runs)
        assertEquals("one readiness window after each helper that ran", readinessSchedule(true).size * 2, polls.size)
        assertFalse(ending.answered)
    }

    @Test
    fun `a helper that finished anything but ran is not waited for`() = runBlocking {
        // A run the agent skipped succeeded as far as the agent goes, and sent no packet at all.
        pi.answer = { id -> Result.success(AgentActionResult(ok = true, contract = 3, op = finished(id, "skipped"))) }
        val ending = walk(center()).run()

        assertEquals(listOf("wake-tower"), nas.runs)
        assertEquals("only the nas's ran is followed by readiness", readinessSchedule(true).size, polls.size)
        assertTrue(ending.tried)
    }

    @Test
    fun `a wake with every step skipped says nothing was tried`() = runBlocking {
        fleet.states["pi"] = HelperState(HelperState.Kind.ASLEEP)
        fleet.states["nas"] = HelperState(HelperState.Kind.UNUSABLE)
        val ending = walk(center()).run()

        assertFalse(ending.answered)
        assertFalse("off the network with no helper to ask, nothing was sent or asked", ending.tried)
        assertEquals(emptyList<Long>(), polls)
        assertEquals("the pi has no wake of its own, so it is not offered to wake first", emptyList<String>(), ending.offerWakeFirst)
    }

    @Test
    fun `a step that sent nothing waits for nothing`() = runBlocking {
        fleet.addresses = listOf(LocalAddress("192.168.1.23", 24))
        fleet.states["pi"] = HelperState(HelperState.Kind.ASLEEP)
        fleet.states["nas"] = HelperState(HelperState.Kind.ASLEEP)
        val silent = object : MachineClient by target {
            override suspend fun sendWakePacket(broadcasts: List<String>, ports: List<Int>, via: Network?) =
                WakeSend(0, "no socket")
        }
        val ending = WakeWalk(silent, fleet, center(), poll = { polls += now; false }, progress = {}, clock = { now }, sleep = { now += it }).run()

        assertEquals(emptyList<Long>(), polls)
        assertTrue(ending.lines.first().contains("No wake packet left this device"))
    }
}
