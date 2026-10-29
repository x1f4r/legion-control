package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.contract.VectorJson
import com.x1f4r.legioncontrol.contract.VectorJson.string
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.DEFAULT_GITHUB_REPO
import com.x1f4r.legioncontrol.data.LaunchApprovals
import com.x1f4r.legioncontrol.data.LaunchDocuments
import com.x1f4r.legioncontrol.data.LaunchDocuments.HELD
import com.x1f4r.legioncontrol.data.LaunchDocuments.LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.PI
import com.x1f4r.legioncontrol.data.LaunchDocuments.PI_LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.TOWER_LINUX
import com.x1f4r.legioncontrol.data.LaunchDocuments.WINDOWS
import com.x1f4r.legioncontrol.data.LaunchDocuments.descendant
import com.x1f4r.legioncontrol.data.LaunchDocuments.recorded
import com.x1f4r.legioncontrol.data.LaunchDocuments.store
import com.x1f4r.legioncontrol.data.LaunchDocuments.tuple
import com.x1f4r.legioncontrol.data.LaunchValues
import com.x1f4r.legioncontrol.data.MachineHints
import com.x1f4r.legioncontrol.data.MemoryPreferences
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.editedMachine
import com.x1f4r.legioncontrol.data.withSystemValue
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.CommandTransport
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.StdinSource
import com.x1f4r.legioncontrol.ui.EnrolmentPreview
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.File

/**
 * What this device runs when the shared setup changes under it (contract section 9.9).
 *
 * Every test here adopts a document the way a machine hands one over and then looks at what is
 * actually sent: the command lines a transport is given, the routes it dials, and the review a person
 * is shown. "The gate said not driven" is not the point; "no ssh line ever carried the offered argv"
 * is, so the lines are recorded and read.
 */
class LaunchGateTest {

    private val tower = "tower"

    // MARK: - A transport that records everything

    private data class Sent(val host: String, val line: String, val stdin: String?)

    private class Recording(private val status: String = V3_STATUS) : CommandTransport {
        val sent = mutableListOf<Sent>()

        override suspend fun run(endpoint: Endpoint, command: String, timeoutMillis: Long, stdin: StdinSource?): CommandOutcome {
            sent += Sent(endpoint.host, command, (stdin as? StdinSource.Bytes)?.bytes?.toString(Charsets.UTF_8))
            return CommandOutcome(0, status, "")
        }

        override suspend fun upload(endpoint: Endpoint, source: File, remotePath: String, timeoutMillis: Long) =
            fail("not used")
    }

    private class Hints : MachineHints {
        private fun flow() = MutableStateFlow<String?>(null)
        private val routes = mutableMapOf<String, MutableStateFlow<String?>>()
        private val systems = mutableMapOf<String, MutableStateFlow<String?>>()
        override fun lastGoodRoute(machineId: String): StateFlow<String?> = routes.getOrPut(machineId, ::flow)
        override fun rememberRoute(machineId: String, id: String) { routes.getOrPut(machineId, ::flow).value = id }
        override fun forgetRoute(machineId: String) { routes.getOrPut(machineId, ::flow).value = null }
        override fun lastKnownSystem(machineId: String): StateFlow<String?> = systems.getOrPut(machineId, ::flow)
        override fun rememberSystem(machineId: String, systemId: String) { systems.getOrPut(machineId, ::flow).value = systemId }
    }

    /** The agent client this device would build for one machine, over [transport]. */
    private fun ConfigStore.client(machineId: String, transport: CommandTransport): AgentClient {
        val machine = launch.value.toMachines().single { it.id == machineId }
        val hints = Hints()
        return AgentClient(machine, transport, RouteSelector(machine, hints) { 0L }, hints) { 0L }
    }

    private fun ConfigStore.paths(): List<String> = launch.value.review.map { it.path }

    private fun ConfigStore.adopt(text: String) =
        assertTrue(adoptFromMachine(text, null, "Tower") is SetupOutcome.Applied)

    private fun enrolled(prefs: MemoryPreferences = MemoryPreferences(), text: String = HELD): ConfigStore =
        store(prefs).also { assertEquals(null, it.applyEnrolment(text, "Test phone")) }

    /** [root] with a machine called Nas added by a peer, whose one system runs [agent]. */
    private fun withNas(root: JsonObject, agent: List<String>): JsonObject {
        val nas = buildJsonObject {
            put("id", "nas")
            put("name", "Nas")
            put("endpoints", JsonArray(listOf(buildJsonObject { put("id", "lan"); put("kind", "lan"); put("host", "192.0.2.50"); put("user", "me"); put("port", 22) })))
            put("systems", JsonArray(listOf(buildJsonObject { put("id", "linux"); put("platform", "linux"); put("agent", LaunchDocuments.argv(agent)) })))
        }
        return JsonObject(root + ("machines" to JsonArray(root["machines"]!!.jsonArray + nas)))
    }

    /** What [machineId] is refused with when it is asked for its status, and that nothing was sent. */
    private suspend fun ConfigStore.refusedStatus(machineId: String): AgentFailure.BadRequest {
        val transport = Recording()
        val failure = runCatching { client(machineId, transport).status() }.exceptionOrNull()
        assertEquals(emptyList<Sent>(), transport.sent)
        return failure as? AgentFailure.BadRequest
            ?: throw AssertionError("a machine with nothing driven was not refused: ${failure ?: "it answered"}")
    }

    // MARK: - The contract's own cases, through the store and the gate

    /**
     * Every `launch/review` vector, adopted by a real store and read back through the gate's own
     * machines: the review a person sees, and the command each system is driven with.
     *
     * The vector adapter checks the port of the reference. This checks the wiring around it: that the
     * record the store seeds is the one the reference seeds, that adopting leaves what the reference
     * says to leave, and that the typed configuration the transports read carries exactly the
     * effective tuples. A case whose offered document this build's own validation refuses is named
     * below with the reason, rather than skipped silently, and every other case has to be adopted.
     */
    @Test
    fun `the review vectors hold through the store, the gate and the machines built from it`() {
        val failures = mutableListOf<String>()
        val cases = VectorJson.cases("launch", "review")
        for (case in cases) {
            val prefs = MemoryPreferences()
            case.input.string("heldText")?.let { prefs.values["controller-config"] = it }
            (case.input["approvals"] as? JsonObject)?.let { prefs.values[LaunchApprovals.KEY] = it.toString() }
            val store = store(prefs)

            when (val outcome = store.adoptFromMachine(case.input.string("offeredText")!!, null, "Tower")) {
                is SetupOutcome.Applied -> Unit
                is SetupOutcome.Invalid -> {
                    val expected = REFUSED_HERE[case.name]
                    if (expected == null || expected !in outcome.reason) failures += "${case.name}: refused, ${outcome.reason}"
                    continue
                }

                else -> {
                    failures += "${case.name}: $outcome"
                    continue
                }
            }

            val review = store.launch.value.review.map { listOf(it.path, it.scope.wire, it.kind, it.approvable.toString()) }
            val expectedReview = case.expect["review"]!!.jsonArray.map { it.jsonObject }.map {
                listOf(it.string("path"), it.string("scope"), it.string("kind"), it["approvable"].toString())
            }
            if (review != expectedReview) failures += "${case.name}: review $review, expected $expectedReview"

            val machines = store.launch.value.toMachines()
            for (entry in case.expect["effective"]!!.jsonArray.map { it.jsonObject }) {
                when (entry.string("scope")) {
                    "system" -> {
                        val path = entry.string("path")!!
                        val system = machines.flatMap { m -> m.systems.map { LaunchValues.systemPath(m.id, it.id) to it } }
                            .single { it.first == path }.second
                        val runs = entry.string("state") in setOf("offered", "approved")
                        val agent = (entry["agent"] as? JsonArray)?.map { (it as JsonPrimitive).content }
                        if (runs) {
                            if (system.agent != agent || system.grammar?.wire != entry.string("grammar") || !system.isDriven) {
                                failures += "${case.name}: $path runs ${system.agent} as ${system.grammar}, expected $agent as ${entry.string("grammar")}"
                            }
                        } else if (system.isDriven || system.agent.isNotEmpty()) {
                            failures += "${case.name}: $path is driven with ${system.agent}, expected not driven"
                        }
                    }

                    "repo" -> {
                        val repo = store.launch.value.config?.appUpdates?.githubRepo
                        if (repo != (entry.string("text") ?: DEFAULT_GITHUB_REPO)) failures += "${case.name}: updates from $repo"
                    }
                }
            }
        }
        assertTrue(failures.joinToString("\n"), failures.isEmpty())
    }

    // MARK: - Acceptance

    @Test
    fun `a rename and a reroute from a restricted publisher are adopted and dialled with no review`() = runBlocking {
        val store = enrolled()
        store.adopt(
            descendant(store.text.value) { root ->
                root.editedMachine(tower) { machine ->
                    JsonObject(
                        machine + ("name" to JsonPrimitive("Desk tower")) + (
                            "endpoints" to JsonArray(
                                listOf(
                                    buildJsonObject {
                                        put("id", "lan")
                                        put("kind", "lan")
                                        put("host", "192.0.2.10")
                                        put("user", "me")
                                        put("port", 2222)
                                    },
                                ),
                            )
                            ),
                    )
                }
            },
        )

        assertEquals(emptyList<String>(), store.paths())
        val machine = store.launch.value.toMachines().single { it.id == tower }
        assertEquals("Desk tower", machine.name)
        assertEquals(LINUX, machine.system("linux")!!.agent)
        val transport = Recording()
        store.client(tower, transport).status()
        assertEquals(listOf("192.0.2.10"), transport.sent.map { it.host })
        assertEquals(buildRemoteCommand(RemoteShell.POSIX, LINUX + "status"), transport.sent.single().line)
    }

    @Test
    fun `a malicious descendant is adopted and published, and never run`() = runBlocking {
        val store = enrolled()
        val malicious = descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("sh", "-c", "touch /tmp/x")) }
        store.adopt(malicious)

        // Adopted: the hash advances, and what this device publishes to a machine that is behind is
        // the document it now holds.
        assertEquals(ControllerDocument.parse(malicious).getOrThrow().hash, store.provenance.value?.hash)
        assertTrue("touch /tmp/x" in store.document.value!!.canonicalText)
        assertEquals(listOf(TOWER_LINUX), store.paths())

        val transport = Recording()
        val client = store.client(tower, transport)
        client.status()
        client.config()
        // Published the way reconciliation publishes it: the held document on standard input, under
        // the command this device approved.
        val held = store.document.value!!
        client.configSet(held.canonicalText, held.identity!!.id, held.identity!!.revision)
        val approved = buildRemoteCommand(RemoteShell.POSIX, LINUX)
        val published = transport.sent.last()
        assertTrue(published.line, published.line.startsWith("$approved config set "))
        assertEquals(malicious, published.stdin)
        assertEquals(3, transport.sent.size)
        for (sent in transport.sent) {
            assertTrue(sent.line, sent.line.startsWith("$approved "))
            assertFalse(sent.line, "touch" in sent.line || sent.line.startsWith("sh "))
        }
    }

    @Test
    fun `a verb injected into a restricted launcher never reaches the dispatcher`() = runBlocking {
        val injected = PI + listOf("self-update", "--rollback")
        // Once with the key still restricted, where the shape has no grammar at all, and once with
        // the restriction cleared, where it is a valid POSIX command nobody approved.
        for (restricted in listOf(true, false)) {
            val store = enrolled()
            store.adopt(
                descendant(store.text.value) {
                    it.withSystemValue("pi", "linux", "agent", injected).withSystemValue("pi", "linux", "restricted", restricted)
                },
            )
            assertEquals(listOf(PI_LINUX), store.paths())
            assertEquals(!restricted, store.launch.value.review.single().approvable)

            val transport = Recording(DISPATCH_STATUS)
            store.client("pi", transport).status()
            assertEquals(listOf(buildRemoteCommand(RemoteShell.DISPATCH, PI + "status")), transport.sent.map { it.line })
        }
    }

    @Test
    fun `a restricted system stays on the dispatcher grammar when a peer clears the restriction`() = runBlocking {
        val store = enrolled()
        store.adopt(
            descendant(store.text.value) {
                it.withSystemValue("pi", "linux", "restricted", false).withSystemValue("pi", "linux", "shell", "powershell")
            },
        )

        assertEquals(listOf(PI_LINUX), store.paths())
        val system = store.launch.value.toMachines().single { it.id == "pi" }.system("linux")!!
        assertEquals(RemoteShell.DISPATCH, system.grammar)
        assertEquals(LaunchState.APPROVED_WITH_CHANGE_WAITING, system.launchState)
        val transport = Recording(DISPATCH_STATUS)
        store.client("pi", transport).status()
        assertEquals("node /home/me/.legion-control/bin/launcher.mjs status", transport.sent.single().line)
    }

    @Test
    fun `an approved custom launcher that stays the same is never reviewed`() {
        val custom = listOf("C:\\Program Files\\nodejs\\node.exe", "D:\\lc\\bin\\launcher.mjs")
        val store = enrolled(text = descendant(HELD) { it.withSystemValue(tower, "windows", "agent", custom) })
        repeat(3) { revision ->
            store.adopt(
                descendant(store.text.value) {
                    it.editedMachine(tower) { m -> JsonObject(m + ("name" to JsonPrimitive("Tower ${revision + 2}"))) }
                },
            )
            assertEquals(emptyList<String>(), store.paths())
        }
        assertEquals(custom, store.launch.value.toMachines().single { it.id == tower }.system("windows")!!.agent)
    }

    @Test
    fun `enrolment shows every launch value, approves them all, and a peer's new machine stays undriven`() = runBlocking {
        val everything = descendant(HELD) { root ->
            JsonObject(
                root.editedMachine(tower) { JsonObject(it + ("ssh" to buildJsonObject { put("identityFile", "~/.ssh/id_tower") })) } +
                    ("appUpdates" to buildJsonObject { put("githubRepo", "owner/fork") }) +
                    ("local" to buildJsonObject { put("agent", "/Users/me/.legion-control/agent/src/index.mjs") }),
            )
        }
        val preview = EnrolmentPreview.of(everything, "desk-tower.local", approves = true).getOrThrow()
        assertEquals(listOf("Tower", "Beacon"), preview.machines.map { it.name })
        assertEquals(
            listOf(tuple(LINUX, "posix"), tuple(WINDOWS, "cmd"), tuple(PI, "dispatch")),
            preview.machines.flatMap { m -> m.systems.map { it.tuple } },
        )
        assertEquals("~/.ssh/id_tower", preview.machines.first().identityFile)
        assertEquals("owner/fork", preview.repo)
        assertEquals("/Users/me/.legion-control/agent/src/index.mjs", preview.local?.agent)

        // Nothing is applied by looking. Confirm is what applies it, and approves all of it.
        val prefs = MemoryPreferences()
        val store = store(prefs)
        assertEquals(null, store.document.value)
        assertEquals(null, store.applyEnrolment(preview.text, "Test phone"))
        assertEquals(emptyList<String>(), store.paths())
        assertEquals("owner/fork", store.launch.value.config?.appUpdates?.githubRepo)

        // A later change from a peer waits.
        store.adopt(descendant(store.text.value) { it.withSystemValue(tower, "linux", "agent", listOf("sh", "-c", "id")) })
        assertEquals(listOf(TOWER_LINUX), store.paths())

        // A machine a peer adds with a launcher nobody here approved is not driven, and nothing is
        // dialled for it at all.
        store.adopt(descendant(store.text.value) { withNas(it, listOf("/usr/bin/node", "/srv/nas/bin/launcher.mjs")) })
        assertTrue("machines[nas].systems[linux]" in store.paths())
        val nas = store.launch.value.toMachines().single { it.id == "nas" }
        assertEquals(LaunchState.NOT_DRIVEN, nas.system("linux")!!.launchState)
        assertEquals("Not driven from this device until its command is approved.", store.refusedStatus("nas").detail)
    }

    @Test
    fun `a machine with nothing driven is refused with what would change that`() = runBlocking {
        // Recorded at enrolment and refused by section 15.1: a restricted key whose command is not an
        // interpreter and an entry. Nothing waits, so there is nothing to approve; the shape of the
        // command is what is wrong, and that is what is said.
        val store = enrolled(text = descendant(HELD) { it.withSystemValue("pi", "linux", "agent", PI + "--verbose") })
        assertEquals(emptyList<String>(), store.paths())
        val pi = store.launch.value.toMachines().single { it.id == "pi" }.system("linux")!!
        assertEquals(LaunchState.NOT_DRIVEN, pi.launchState)
        assertEquals(RESTRICTED_AGENT_SHAPE, pi.grammarRefusal)
        assertFalse(pi.awaitsApproval)
        val shaped = store.refusedStatus("pi")
        assertEquals("This app cannot write commands for Raspberry Pi OS.", shaped.summary)
        assertEquals(describeRefusal(RESTRICTED_AGENT_SHAPE, null), shaped.detail)

        // A machine a peer adds with a command that can only be reverted waits for review, and
        // approving is still not what would drive it.
        store.adopt(descendant(store.text.value) { withNas(it, listOf("/usr/bin/node", "/srv/nas/\u202Esjm.rehcnual/nib")) })
        assertEquals(listOf(false), store.launch.value.review.filter { it.machineId == "nas" }.map { it.approvable })
        val nas = store.launch.value.toMachines().single { it.id == "nas" }.system("linux")!!
        assertFalse(nas.awaitsApproval)
        assertEquals(null, nas.grammarRefusal)
        val unshowable = store.refusedStatus("nas")
        assertEquals("Nothing on Nas is driven from this device.", unshowable.summary)
        assertEquals("The setup gives it no command this device can run.", unshowable.detail)
    }

    @Test
    fun `the launcher migration made on one device is taken by another without asking, and nothing more is`() {
        val entry = listOf("/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs")
        val before = descendant(HELD) { it.withSystemValue(tower, "linux", "agent", entry) }

        // Device A presses "Update command": an edit, recorded in the same write as the document.
        val prefsA = MemoryPreferences()
        val a = enrolled(prefsA, before)
        val writes = prefsA.writes.size
        a.applyEdit(ControllerDocument.of(a.document.value!!.root.withSystemValue(tower, "linux", "agent", LINUX)), "Phone A")
        assertEquals(tuple(LINUX, "posix"), recorded(prefsA, tower, "linux"))
        assertTrue(prefsA.writes.drop(writes).single().containsAll(setOf("controller-config", LaunchApprovals.KEY)))

        // Device B adopts it with no prompt, and records it where it now runs.
        val prefsB = MemoryPreferences()
        val b = enrolled(prefsB, before)
        b.adopt(a.text.value)
        assertEquals(emptyList<String>(), b.paths())
        assertEquals(tuple(LINUX, "posix"), recorded(prefsB, tower, "linux"))

        // The same migration with a new interpreter, a new base or a new grammar is not one.
        val notMigrations = listOf(
            listOf("/usr/local/bin/node", "/home/me/.legion-control/bin/launcher.mjs") to "posix",
            listOf("/usr/bin/node", "/srv/legion-control/bin/launcher.mjs") to "posix",
            LINUX to "powershell",
        )
        for ((argv, shell) in notMigrations) {
            val c = enrolled(text = before)
            c.adopt(descendant(c.text.value) { it.withSystemValue(tower, "linux", "agent", argv).withSystemValue(tower, "linux", "shell", shell) })
            assertEquals("$argv $shell", listOf(TOWER_LINUX), c.paths())
        }
    }

    @Test
    fun `serialisation reads only the gate's command and grammar`() {
        // A configuration read straight from a document, which never went through the gate, has a
        // command in every system and drives none of them.
        val config = ControllerDocument.parse(HELD).getOrThrow().typed().getOrThrow()
        val ungated = LaunchGate.Effective(config, emptyList()).toMachines()
        assertTrue(ungated.flatMap { it.systems }.none { it.isDriven || it.agent.isNotEmpty() })
    }

    private companion object {
        const val V3_STATUS = """{"ok":true,"contract":3,"system":{"id":"linux"}}"""
        const val DISPATCH_STATUS = """{"ok":true,"contract":3,"system":{"id":"linux"},"agent":{"restrictedSession":true}}"""

        /**
         * The review cases whose offered document this build's validation refuses, and why. Stricter
         * than contract section 9.7, which keeps a system with no agent and takes a wake block's
         * broadcast from its site; the setup-acceptance port lifts both. A case listed here that is
         * adopted is checked like any other.
         */
        val REFUSED_HERE = mapOf(
            "agent-removed" to "has no agent command",
            "agent-emptied" to "has no agent command",
            "display-and-routing-only" to "no broadcast address",
        )
    }
}
