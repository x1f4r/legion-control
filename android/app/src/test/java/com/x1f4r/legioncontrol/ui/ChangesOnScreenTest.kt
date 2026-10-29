package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.snapshots.Snapshot
import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OperationEvent
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.SetupPeerState
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.OperationStore
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.job
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/**
 * What the machine page draws of the changes asked of it, and what drawing it leaves behind.
 *
 * Changes and Recent changes were read straight off the record store, which Compose never hears
 * about, so a change that moved stayed drawn as it was until something else redrew the page (A7).
 * And every machine model collected the records for itself, on the app's scope, so each rebuild of
 * the machines added collectors nobody ever stopped (A11).
 */
class ChangesOnScreenTest {

    private val directory: File = Files.createTempDirectory("screen").toFile()
    private val store = OperationStore(File(directory, "operations.json"))
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
    private val center = OperationCenter(store, scope, SilentNotifier)

    @After
    fun tearDown() {
        scope.cancel()
        directory.deleteRecursively()
    }

    private fun client(id: String) = object : MachineClient by unusable<MachineClient>("drawing the changes") {
        override val machine = Machine(id = id, name = id.replaceFirstChar { it.uppercase() }, endpoints = emptyList(), systems = emptyList(), wake = null)
        override val abilities = AgentAbilities(contract = 3)
    }

    private val setup = object : SetupSync by unusable<SetupSync>("drawing the changes") {
        override fun state(machineId: String) = SetupPeerState()
    }

    private fun model(id: String = "tower") = MachineModel(
        client = client(id),
        settings = RememberedSettings(MemoryPreferences()),
        scope = scope,
        operations = center,
        setup = setup,
        fleet = unusable("drawing the changes"),
    )

    /** The state objects a composable reading [read] would be redrawn for. */
    private fun statesReadBy(read: () -> Unit): Set<Any> {
        val states = mutableSetOf<Any>()
        Snapshot.observe(readObserver = { states += it }) { read() }
        return states
    }

    /** Whether a change made by [change] reaches anything that read [states]. */
    private fun redraws(states: Set<Any>, change: () -> Unit): Boolean {
        var redrawn = false
        val observer = Snapshot.registerApplyObserver { changed, _ -> if (changed.any { it in states }) redrawn = true }
        try {
            change()
            Snapshot.sendApplyNotifications()
        } finally {
            observer.dispose()
        }
        return redrawn
    }

    @Test
    fun `a change asked for redraws Changes`() {
        val model = model()
        val reading = statesReadBy { model.pending }

        lateinit var id: String
        assertTrue(
            "what draws Changes is told a change was recorded",
            redraws(reading) { id = center.record(client("tower"), OperationIntent(OpKind.RESTART, service = "demo"), "Demo", "this phone").id },
        )
        assertEquals(listOf(id), model.pending.map { it.id })
    }

    @Test
    fun `a change that ends moves from Changes to Recent changes on screen`() {
        val model = model()
        val record = center.record(client("tower"), OperationIntent(OpKind.RESTART, service = "demo"), "Demo", "this phone")
        val reading = statesReadBy {
            model.pending
            model.history
        }

        assertTrue(
            redraws(reading) { center.apply(record.id, OperationEvent.NotRecorded(System.currentTimeMillis())) },
        )
        assertTrue("no longer in Changes", model.pending.isEmpty())
        assertTrue("now in Recent changes", model.history.single().isOver)
    }

    @Test
    fun `each machine draws only its own changes`() {
        val tower = model("tower")
        val pi = model("pi")
        center.record(client("pi"), OperationIntent(OpKind.SLEEP), "Pi", "this phone")

        assertTrue(tower.history.isEmpty())
        assertEquals(1, pi.pending.size)
    }

    @Test
    fun `building the machines again leaves nothing running behind them`() {
        val running = scope.coroutineContext.job.children.count()

        // As every change to the setup does: a model for each machine, again and again.
        repeat(3) { listOf(model("tower"), model("pi")) }

        assertEquals(running, scope.coroutineContext.job.children.count())
    }
}
