package com.x1f4r.legioncontrol.agent

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * Every shared fixture, decoded by the app's own models.
 *
 * The fixtures are the one place the agent, the Mac, the desktop and this app can disagree without
 * anybody noticing until a machine is in front of them. A decoder that quietly drops a field, or
 * throws on a reply variant nobody thought to try by hand, fails here instead.
 *
 * The rule being enforced is compatibility rule 3: every field is optional, so no reply may fail a
 * whole decode. A fixture this build has never seen has to load, even if half of it is ignored.
 */
class ContractFixturesTest {

    private fun fixtureDirectory(): File? = listOf(
        File("../../contract/fixtures"),
        File("../contract/fixtures"),
        File("contract/fixtures"),
    ).firstOrNull { it.isDirectory }

    private fun fixtures(): List<File> = fixtureDirectory()
        ?.listFiles { file -> file.name.endsWith(".json") && file.name != "index.json" }
        ?.sortedBy { it.name }
        .orEmpty()

    private fun read(name: String): JsonObject? = fixtureDirectory()
        ?.resolve(name)
        ?.takeIf { it.isFile }
        ?.let { AgentJson.parseToJsonElement(it.readText()) as JsonObject }

    @Test
    fun `every fixture decodes into the model its command belongs to`() {
        val files = fixtures()
        assumeTrue("contract/fixtures is not in this checkout", files.isNotEmpty())

        val failures = mutableListOf<String>()
        var decoded = 0
        for (file in files) {
            val text = file.readText()
            val outcome = runCatching {
                when {
                    file.name.startsWith("status.") || file.name == "legacy-2x.status.json" ->
                        AgentJson.decodeFromString(AgentStatus.serializer(), text)

                    file.name.startsWith("busy.") ->
                        AgentJson.decodeFromString(BusyStatus.serializer(), text)

                    file.name.startsWith("op.") ->
                        AgentJson.decodeFromString(OpEnvelope.serializer(), text)

                    file.name.startsWith("history.") ->
                        AgentJson.decodeFromString(HistoryReply.serializer(), text)

                    file.name.startsWith("logs.") ->
                        AgentJson.decodeFromString(LogsReply.serializer(), text)

                    file.name.startsWith("doctor.") ->
                        AgentJson.decodeFromString(DoctorReport.serializer(), text)

                    file.name.startsWith("version.") ->
                        AgentJson.decodeFromString(AgentVersion.serializer(), text)

                    file.name.startsWith("help.") ->
                        AgentJson.decodeFromString(HelpReply.serializer(), text)

                    file.name.startsWith("config.set.") ->
                        AgentJson.decodeFromString(ConfigSetReply.serializer(), text)

                    file.name.startsWith("config.meta") ->
                        AgentJson.decodeFromString(ConfigMetaReply.serializer(), text)

                    file.name.startsWith("config.") || file.name == "legacy-2x.config.json" ->
                        AgentJson.decodeFromString(JsonObject.serializer(), text)

                    file.name.startsWith("service-config") ->
                        AgentJson.decodeFromString(JsonObject.serializer(), text)

                    file.name.startsWith("policy.") ->
                        AgentJson.decodeFromString(AgentActionResult.serializer(), text)

                    file.name.startsWith("bundle.") ->
                        AgentJson.decodeFromString(JsonObject.serializer(), text)

                    // Everything else is a mutation reply or an error, which share one shape.
                    else -> AgentJson.decodeFromString(AgentActionResult.serializer(), text)
                }
            }
            outcome.onSuccess { decoded += 1 }
            outcome.onFailure { failures += "${file.name}: ${it.message}" }
        }

        assertTrue("failed to decode:\n${failures.joinToString("\n")}", failures.isEmpty())
        assertEquals("every fixture should have been decoded", files.size, decoded)
    }

    @Test
    fun `a full status carries everything the app draws from`() {
        val fixture = read("status.full.json") ?: return
        val status = AgentJson.decodeFromString(AgentStatus.serializer(), fixture.toString())

        assertTrue("contract 3 is what gates every v3 feature", status.speaksV3)
        assertNotNull(status.systemId)
        assertTrue(status.effectiveServices.isNotEmpty())

        // The v3 fields, each of which used to be missing or conflated with another.
        val service = status.effectiveServices.first()
        assertNotNull("process and health are separate questions", service.process)
        assertNotNull(service.health)
        assertNotNull("the policy is per service now", service.updates)
        assertNotNull("busy says what it read, not just yes or no", service.busy?.evidence)
    }

    @Test
    fun `a partial status says which fields it could not fill in`() {
        val fixture = read("status.partial.json") ?: return
        val status = AgentJson.decodeFromString(AgentStatus.serializer(), fixture.toString())
        assertEquals(
            "a bounded status that ran out of budget has to admit it",
            true,
            status.timing?.partial,
        )
    }

    @Test
    fun `a status whose config will not load blocks every change`() {
        val fixture = read("status.config-invalid.json") ?: return
        val status = AgentJson.decodeFromString(AgentStatus.serializer(), fixture.toString())
        assertTrue("the app says this once at the top rather than per failed button", status.configBroken)
    }

    @Test
    fun `a 2 x status still folds into one service`() {
        val fixture = read("legacy-2x.status.json") ?: return
        val status = AgentJson.decodeFromString(AgentStatus.serializer(), fixture.toString())
        assertFalse("no contract key means the old paths stay", status.speaksV3)
        assertTrue(
            "the first agent spread one service across the top level; it is folded back here",
            status.effectiveServices.isNotEmpty(),
        )
    }

    @Test
    fun `an update reply that was replayed is the earlier record, not a second run`() {
        val fixture = read("update.replayed.json") ?: return
        val reply = AgentJson.decodeFromString(AgentActionResult.serializer(), fixture.toString())
        assertEquals(
            "retrying with the same id has to come back as the same operation",
            true,
            reply.replayed ?: reply.op?.replayed,
        )
    }

    @Test
    fun `a conflict names the operation holding the lock`() {
        val fixture = read("update.conflict.json") ?: return
        val reply = AgentJson.decodeFromString(AgentActionResult.serializer(), fixture.toString())
        assertEquals("conflict", reply.effectiveAction)
        assertNotNull("saying \"busy\" is not enough; the app names what is in the way", reply.conflict)
        assertNotNull(reply.conflict?.opId)
    }

    @Test
    fun `a forced update with nothing to verify says it was not verified`() {
        val fixture = read("update.forced-unverified.json") ?: return
        val reply = AgentJson.decodeFromString(AgentActionResult.serializer(), fixture.toString())
        assertEquals(
            "force bypasses the busy gate, never the postcondition",
            false,
            reply.effectiveVerified,
        )
    }

    @Test
    fun `every reasonCode in the fixtures is one the app renders`() {
        val files = fixtures()
        assumeTrue("contract/fixtures is not in this checkout", files.isNotEmpty())

        val unknown = mutableSetOf<String>()
        for (file in files) {
            val root = AgentJson.parseToJsonElement(file.readText()) as? JsonObject ?: continue
            collect(root, "reasonCode").forEach { code ->
                if (ReasonCode.fromWire(code) == null) unknown += "$code (${file.name})"
            }
        }
        assertTrue("these reason codes would be shown as raw text: $unknown", unknown.isEmpty())
    }

    /**
     * Every operation action and phase the fixtures use, read through the models rather than off the
     * raw tree.
     *
     * Off the raw tree this would also pick up `action` where it names a configured action id, as a
     * wake helper does, and `config set`'s own stored/noop/replaced, neither of which is an operation
     * action at all.
     */
    @Test
    fun `every operation action and phase in the fixtures is one the app understands`() {
        val files = fixtures()
        assumeTrue("contract/fixtures is not in this checkout", files.isNotEmpty())

        val unknownActions = mutableSetOf<String>()
        val unknownPhases = mutableSetOf<String>()

        fun checkRecord(record: OpRecord?, where: String) {
            val record = record ?: return
            record.result?.action?.let {
                if (OpAction.fromWire(it) == null) unknownActions += "$it ($where)"
            }
            record.phase?.let {
                if (OpPhase.fromWire(it) == null) unknownPhases += "$it ($where)"
            }
        }

        fun checkSummary(summary: OpSummary?, where: String) {
            val summary = summary ?: return
            summary.action?.let {
                if (OpAction.fromWire(it) == null) unknownActions += "$it ($where)"
            }
            summary.phase?.let {
                if (OpPhase.fromWire(it) == null) unknownPhases += "$it ($where)"
            }
        }

        for (file in files) {
            val text = file.readText()
            when {
                file.name.startsWith("op.") -> {
                    val envelope = AgentJson.decodeFromString(OpEnvelope.serializer(), text)
                    if (envelope.ok == false) {
                        org.junit.Assert.assertThrows(AgentFailure.Reported::class.java) { envelope.record() }
                    } else checkRecord(envelope.record(), file.name)
                }

                file.name.startsWith("status.") -> {
                    val status = AgentJson.decodeFromString(AgentStatus.serializer(), text)
                    val block = status.operations
                    (block?.running.orEmpty() + block?.queued.orEmpty() + block?.recent.orEmpty())
                        .forEach { checkSummary(it, file.name) }
                }

                file.name.startsWith("history.") -> {
                    AgentJson.decodeFromString(HistoryReply.serializer(), text)
                        .entries.forEach { checkSummary(it, file.name) }
                }

                // config.set answers stored | noop | replaced, which are its own outcomes and not
                // operation actions. Checked here in their own right.
                file.name.startsWith("config.set.") -> {
                    val reply = AgentJson.decodeFromString(ConfigSetReply.serializer(), text)
                    reply.action?.let {
                        assertTrue(
                            "${file.name} answers with an unexpected config set action: $it",
                            it in setOf("stored", "noop", "replaced"),
                        )
                    }
                }

                file.name.startsWith("config.") || file.name.startsWith("busy.") ||
                    file.name.startsWith("doctor.") || file.name.startsWith("bundle.") ||
                    file.name.startsWith("help.") || file.name.startsWith("version.") ||
                    file.name.startsWith("logs.") || file.name.startsWith("policy.") ||
                    file.name.startsWith("legacy-2x.config") || file.name.startsWith("legacy-2x.status") ||
                    file.name.startsWith("service-config") -> Unit

                else -> {
                    val reply = AgentJson.decodeFromString(AgentActionResult.serializer(), text)
                    reply.action?.let {
                        if (OpAction.fromWire(it) == null) unknownActions += "$it (${file.name})"
                    }
                    checkRecord(reply.op, file.name)
                }
            }
        }

        assertTrue("unknown actions: $unknownActions", unknownActions.isEmpty())
        assertTrue("unknown phases: $unknownPhases", unknownPhases.isEmpty())
    }

    /** Every string value under [key], anywhere in the tree. */
    private fun collect(element: kotlinx.serialization.json.JsonElement, key: String): List<String> =
        when (element) {
            is JsonObject -> element.flatMap { (name, value) ->
                val here = if (name == key) {
                    listOfNotNull((value as? kotlinx.serialization.json.JsonPrimitive)?.contentOrNull())
                } else {
                    emptyList()
                }
                here + collect(value, key)
            }

            is kotlinx.serialization.json.JsonArray -> element.flatMap { collect(it, key) }
            else -> emptyList()
        }

    private fun kotlinx.serialization.json.JsonPrimitive.contentOrNull(): String? =
        if (this is kotlinx.serialization.json.JsonNull) null else content.takeIf { it.isNotBlank() }
}
