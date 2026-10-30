package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.BindingsStore
import com.x1f4r.legioncontrol.data.CanonicalSetup
import com.x1f4r.legioncontrol.data.DocumentAcceptance
import com.x1f4r.legioncontrol.data.Intake
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Every shared fixture, walked from `contract/fixtures/index.json` and read by the app's own models
 * (contract section 14).
 *
 * The fixtures are the one place the agent, the Mac, the desktop and this app can disagree without
 * anybody noticing until a machine is in front of them. This used to glob the directory and pick a
 * decoder by file name, skip when the corpus was not there, and check that each file decoded and
 * nothing more, so a decoder that read the wrong key passed. Now every entry of the index is
 * handled by its kind and schema, an entry with no handler is a failure, and each reply is held to
 * what the index says of it: `ok`, `action`, `reasonCode`, `outcome`, the exit code it came with,
 * and the fields the screens are drawn from, read from the model and from the raw JSON alike.
 *
 * Compatibility rule 3 still holds underneath: every field is optional, so no reply may fail a whole
 * decode.
 */
class ContractFixturesTest {

    private val index = ContractCorpus.json("fixtures/index.json") as JsonObject
    private val entries = (index["fixtures"] as JsonArray).map { it as JsonObject }

    private fun JsonObject.text(key: String): String? = (this[key] as? JsonPrimitive)?.takeIf { it.isString }?.content

    private fun fixture(name: String): String = ContractCorpus.file("fixtures/$name").readText()

    @Test
    fun `the index is contract 3 and names the whole corpus`() {
        assertEquals(3, (index["contract"] as JsonPrimitive).intOrNull)
        assertTrue("the index names ${entries.size} fixtures", entries.size >= 110)
    }

    @Test
    fun `every index entry is handled`() {
        val failures = mutableListOf<String>()
        var handled = 0
        for (entry in entries) {
            val file = entry.text("file")!!
            try {
                when (val kind = entry.text("kind")) {
                    "reply" -> reply(entry, file)
                    "document" -> document(entry, file)
                    else -> throw AssertionError("no handler for kind=$kind schema=${entry.text("schema")}")
                }
                handled += 1
            } catch (failure: Throwable) {
                failures += "$file: ${failure.message ?: failure}"
            }
        }
        assertTrue(failures.joinToString("\n"), failures.isEmpty())
        assertEquals("every entry of the index is handled", entries.size, handled)
    }

    // MARK: - Replies

    /** What the app read out of one reply, through the decoder the reply goes through in production. */
    private data class Reading(
        val ok: Boolean?,
        val action: String?,
        val reasonCode: String?,
        /** The outcome the app decides on: the reply's own, or the reducer's. Null where there is none. */
        val outcome: Outcome? = null,
        /** The "speaks 3" gate, for a status. */
        val speaksV3: Boolean? = null,
        /** The fields of section 14's projection table, as read from the model. */
        val projection: JsonElement? = null,
    )

    private fun <T> decode(serializer: KSerializer<T>, text: String): T = AgentJson.decodeFromString(serializer, text)

    private fun mutation(text: String): Reading {
        val reply = decode(AgentActionResult.serializer(), text)
        return Reading(reply.ok, reply.action, reply.reasonCode, reply.effectiveOutcome)
    }

    /** A reply this app reads as a tree, read from the tree. */
    private fun tree(text: String): Reading {
        val reply = AgentJson.parseToJsonElement(text) as JsonObject
        return Reading(
            ok = (reply["ok"] as? JsonPrimitive)?.booleanOrNull,
            action = reply.text("action"),
            reasonCode = reply.text("reasonCode"),
            outcome = Outcome.reduce(reply.text("outcome"), null, (reply["ok"] as? JsonPrimitive)?.booleanOrNull, reply.text("action"), reply.text("reasonCode")),
        )
    }

    /** One decoder per schema, which is the one the reply goes through in production. */
    private val readers: Map<String, (String) -> Reading> = mapOf(
        "status.schema.json" to { text ->
            val status = decode(AgentStatus.serializer(), text)
            Reading(
                ok = status.ok,
                action = null,
                reasonCode = status.reasonCode,
                speaksV3 = status.speaksV3,
                projection = buildJsonObject {
                    put("services", ids(status.services?.map { it.id }))
                    put("controllerHash", JsonPrimitive(status.controller?.hash))
                    put("running", ids(status.operations?.running?.map { it.id }))
                    put("processUnknown", JsonArray(status.services.orEmpty().map { JsonPrimitive(it.process?.isUnknown == true) }))
                },
            )
        },
        "legacy-2x-status.schema.json" to { text ->
            val status = decode(AgentStatus.serializer(), text)
            Reading(status.ok, null, status.reasonCode, speaksV3 = status.speaksV3)
        },
        "busy.schema.json" to { text ->
            val busy = decode(BusyStatus.serializer(), text)
            Reading(
                ok = (AgentJson.parseToJsonElement(text) as JsonObject)["ok"].let { (it as? JsonPrimitive)?.booleanOrNull },
                action = null,
                reasonCode = null,
                projection = JsonArray((busy.services ?: listOf(busy)).map { evidence(it.monitored, it.evidence) }),
            )
        },
        "op.schema.json" to { text ->
            val raw = AgentJson.parseToJsonElement(text) as JsonObject
            val envelope = decode(OpEnvelope.serializer(), text)
            Reading(envelope.ok, null, envelope.reasonCode, projection = operation(raw))
        },
        "history.schema.json" to { text ->
            val history = decode(HistoryReply.serializer(), text)
            Reading(history.ok, null, null, projection = ids(history.entries.map { it.id }))
        },
        "logs.schema.json" to { text ->
            val logs = decode(LogsReply.serializer(), text)
            val raw = AgentJson.parseToJsonElement(text) as JsonObject
            Reading(logs.ok, null, raw.text("reasonCode"), projection = ids(logs.entries.map { it.line }))
        },
        "doctor.schema.json" to { text ->
            val doctor = decode(DoctorReport.serializer(), text)
            Reading(
                ok = doctor.ok,
                action = null,
                reasonCode = null,
                projection = buildJsonObject {
                    put("checks", ids(doctor.checks?.map { it.id }))
                    put("counts", doctor.counts?.let { counts -> JsonObject(counts.mapValues { JsonPrimitive(it.value) }) } ?: JsonNull)
                },
            )
        },
        "cycle.schema.json" to { text ->
            val cycle = decode(AgentActionResult.serializer(), text)
            Reading(
                ok = cycle.ok,
                action = cycle.action,
                reasonCode = cycle.reasonCode,
                outcome = cycle.effectiveOutcome,
                projection = children(cycle.children?.map { it.service to it.opId }),
            )
        },
        "config-read.schema.json" to { text ->
            val raw = AgentJson.parseToJsonElement(text) as JsonObject
            val fetched = readControllerReply(raw)
            val document = fetched as? ControllerFetch.Document
            // The bytes adopted are the stored bytes, when the agent sent them, and they hash to
            // what the machine says it holds (C1).
            raw.text("raw")?.let { stored ->
                assertNotNull("a reply with raw is a document", document)
                assertEquals("the adopted bytes are the stored bytes", stored, document!!.text)
                assertEquals("the stored bytes hash to the reply's hash", raw.text("hash"), CanonicalSetup.hashOf(document.text.toByteArray(Charsets.UTF_8)))
            }
            Reading(
                ok = (raw["ok"] as? JsonPrimitive)?.booleanOrNull,
                action = null,
                reasonCode = null,
                projection = JsonPrimitive(document?.replyHash),
            )
        },
        "legacy-2x-config.schema.json" to { text ->
            val raw = AgentJson.parseToJsonElement(text) as JsonObject
            readControllerReply(raw)
            tree(text)
        },
        "config-meta.schema.json" to { text ->
            val meta = decode(ConfigMetaReply.serializer(), text)
            Reading(
                ok = meta.ok,
                action = null,
                reasonCode = null,
                projection = buildJsonObject {
                    put("hash", JsonPrimitive(meta.meta?.hash))
                    put("lineage", ids(meta.meta?.lineage))
                },
            )
        },
        "config-set.schema.json" to { text ->
            val reply = decode(ConfigSetReply.serializer(), text)
            Reading(
                ok = reply.ok,
                action = reply.action,
                reasonCode = reply.reasonCode,
                outcome = reply.effectiveOutcome,
                projection = buildJsonObject {
                    put("hash", JsonPrimitive(reply.hash))
                    put("currentHash", JsonPrimitive(reply.current?.hash))
                    put("launchChanges", ids(reply.launchChanges))
                    put("problems", ids(reply.problems?.map { it.path }))
                },
            )
        },
        "version.schema.json" to { text ->
            val version = decode(AgentVersion.serializer(), text)
            val raw = AgentJson.parseToJsonElement(text) as JsonObject
            Reading(version.ok, null, raw.text("reasonCode"))
        },
        // Read by the decoder every mutation and refusal goes through.
        "update.schema.json" to ::mutation,
        "restart.schema.json" to ::mutation,
        "boot.schema.json" to ::mutation,
        "sleep.schema.json" to ::mutation,
        "run.schema.json" to ::mutation,
        "cancel.schema.json" to ::mutation,
        "self-update.schema.json" to ::mutation,
        "policy.schema.json" to ::mutation,
        "error.schema.json" to ::mutation,
        "legacy-2x-mutation.schema.json" to ::mutation,
        // Read as trees in production: nothing in this app decodes them into a class.
        "bundle.schema.json" to ::tree,
        "help.schema.json" to ::tree,
        "service-config.schema.json" to ::tree,
    )

    /** The same projection, read off the raw JSON, for the schemas that have one. */
    private fun rawProjection(schema: String, raw: JsonObject): JsonElement? = when (schema) {
        "status.schema.json" -> buildJsonObject {
            val services = (raw["services"] as? JsonArray)?.map { it as JsonObject }
            put("services", ids(services?.map { it.text("id") }))
            put("controllerHash", JsonPrimitive((raw["controller"] as? JsonObject)?.text("hash")))
            put("running", ids(((raw["operations"] as? JsonObject)?.get("running") as? JsonArray)?.map { (it as JsonObject).text("id") }))
            put("processUnknown", JsonArray(services.orEmpty().map { service -> JsonPrimitive(((service["process"] as? JsonObject)?.get("error") ?: JsonNull) !is JsonNull) }))
        }

        "busy.schema.json" -> {
            val services = (raw["services"] as? JsonArray)?.map { it as JsonObject } ?: listOf(raw)
            JsonArray(services.map { evidence((it["monitored"] as? JsonPrimitive)?.booleanOrNull, it.text("evidence")) })
        }

        "op.schema.json" -> if (raw["op"] is JsonNull) {
            JsonPrimitive("no record")
        } else {
            val op = raw["op"] as JsonObject
            buildJsonObject {
                put("id", op.text("id"))
                put("state", op.text("state"))
            }
        }

        // A reply with no list has nothing in it, which is how the models read one.
        "history.schema.json" -> ids((raw["operations"] as? JsonArray).orEmpty().map { (it as JsonObject).text("id") })
        "logs.schema.json" -> ids((raw["lines"] as? JsonArray).orEmpty().map { (it as JsonObject).text("line") })
        "doctor.schema.json" -> buildJsonObject {
            put("checks", ids((raw["checks"] as? JsonArray)?.map { (it as JsonObject).text("id") }))
            put("counts", raw["counts"] ?: JsonNull)
        }

        "cycle.schema.json" -> children((raw["children"] as? JsonArray)?.map { (it as JsonObject).let { child -> child.text("service") to child.text("opId") } })
        "config-read.schema.json" -> JsonPrimitive(raw.text("hash"))
        "config-meta.schema.json" -> buildJsonObject {
            val meta = raw["meta"] as? JsonObject
            put("hash", JsonPrimitive(meta?.text("hash")))
            put("lineage", ids((meta?.get("lineage") as? JsonArray)?.map { (it as JsonPrimitive).contentOrNull }))
        }

        "config-set.schema.json" -> buildJsonObject {
            put("hash", JsonPrimitive(raw.text("hash")))
            put("currentHash", JsonPrimitive((raw["current"] as? JsonObject)?.text("hash")))
            put("launchChanges", ids((raw["launchChanges"] as? JsonArray)?.map { (it as JsonPrimitive).contentOrNull }))
            put("problems", ids((raw["problems"] as? JsonArray)?.map { (it as JsonObject).text("path") }))
        }

        else -> null
    }

    private fun ids(values: List<String?>?): JsonElement = values?.let { list -> JsonArray(list.map { JsonPrimitive(it) }) } ?: JsonNull

    private fun evidence(monitored: Boolean?, evidence: String?) = buildJsonObject {
        put("monitored", monitored)
        put("evidence", evidence)
    }

    private fun children(pairs: List<Pair<String?, String?>>?): JsonElement = pairs?.let { list ->
        JsonArray(
            list.map { (service, opId) ->
                buildJsonObject {
                    put("service", service)
                    put("opId", opId)
                }
            },
        )
    } ?: JsonNull

    /**
     * An `op` reply as this app reads it: the record, or no record when the reply carries `op: null`,
     * whatever the code beside it. 3.0.x sent the same null under `bad-argument`, so that is read too.
     */
    private fun operation(raw: JsonObject): JsonElement {
        fun read(reply: JsonObject): JsonElement = try {
            val record = readOperation("fixture", reply)
            buildJsonObject {
                put("id", record.id)
                put("state", record.state)
            }
        } catch (_: AgentFailure.NoSuchOperation) {
            JsonPrimitive("no record")
        }

        val read = read(raw)
        if (raw["op"] is JsonNull) {
            val older = JsonObject(raw + ("reasonCode" to JsonPrimitive("bad-argument")))
            assertEquals("op null is no record under bad-argument as well", read, read(older))
        }
        return read
    }

    private fun reply(entry: JsonObject, file: String) {
        val schema = entry.text("schema")!!
        val reader = readers[schema] ?: throw AssertionError("no handler for kind=reply schema=$schema")
        val text = fixture(file)
        val raw = AgentJson.parseToJsonElement(text) as JsonObject
        val reading = reader(text)

        assertEquals("ok", (entry["ok"] as? JsonPrimitive)?.booleanOrNull, reading.ok)
        assertEquals("action", entry.text("action"), reading.action)
        assertEquals("reasonCode", entry.text("reasonCode"), reading.reasonCode)

        collect(raw, "reasonCode").forEach { code ->
            assertNotNull("reasonCode $code would be shown as raw text", ReasonCode.fromWire(code))
        }

        entry.text("outcome")?.let { expected ->
            assertEquals("outcome", expected, reading.outcome?.wire)
        }

        if (schema == "status.schema.json" || schema == "legacy-2x-status.schema.json") {
            assertEquals("the contract 3 gate", (entry["contract"] as? JsonPrimitive)?.intOrNull == 3, reading.speaksV3)
        }

        // The exit code the reply came with, through the classifier every attempt goes through: a
        // reply is a reply whatever the code, and it reads back the same.
        val exitCode = (entry["exitCode"] as JsonPrimitive).intOrNull
        val classified = AttemptClassifier.classify(timedOut = false, exitStatus = exitCode, stdout = text, stderr = "")
        assertTrue("exit $exitCode with this on stdout is a reply", classified.isReply)
        val again = reader(AttemptClassifier.jsonObjectSlice(text)!!.toString())
        assertEquals("read back after classifying", Triple(reading.ok, reading.action, reading.reasonCode), Triple(again.ok, again.action, again.reasonCode))

        rawProjection(schema, raw)?.let { expected ->
            assertEquals("projection", expected, reading.projection)
        }
    }

    // MARK: - Documents

    /**
     * Bindings files a phone has no use for. The phone keeps `deviceName` and `currentSite` as app
     * preferences (data/Bindings.kt) and reads no bindings file at all.
     */
    private val notStored = mapOf(
        "bindings.schema.json" to "Android keeps deviceName and currentSite as app preferences (data/Bindings.kt) and reads no bindings file",
    )

    /** Invalid bindings a phone has nothing to refuse in, and why. */
    private val accountedInvalid = mapOf(
        "bindings.invalid-local-agent-shell.json" to "localAgent does not exist on a phone",
    )

    private fun document(entry: JsonObject, file: String) {
        val valid = entry.text("expect") == "valid"
        when (val schema = entry.text("schema")) {
            // A setup document is held to section 9.7 under peer intake, and comes out valid or
            // invalid exactly as the index says (C3).
            "controller-document.schema.json" -> {
                val verdict = DocumentAcceptance.check(ContractCorpus.file("fixtures/$file").readBytes(), Intake.PEER)
                assertEquals("expected ${entry.text("expect")}, got ${verdict.errors.map { it.code }}", valid, verdict.valid)
            }

            "bindings.schema.json" -> {
                assertNotNull(notStored[schema])
                val bindings = AgentJson.parseToJsonElement(fixture(file)) as JsonObject
                if (!valid) {
                    assertNotNull("an invalid bindings fixture nobody accounted for", accountedInvalid[file])
                    return
                }
                val properties = (ContractCorpus.json("schemas/bindings.schema.json") as JsonObject)["properties"] as JsonObject
                assertTrue("deviceName and currentSite are keys of the bindings schema", "deviceName" in properties && "currentSite" in properties)
                bindings.text("deviceName")?.let { assertTrue("deviceName $it", BindingsStore.keepsDeviceName(it)) }
                bindings.text("currentSite")?.let { assertTrue("currentSite $it", BindingsStore.keepsCurrentSite(it)) }
            }

            else -> throw AssertionError("no handler for kind=document schema=$schema")
        }
    }

    // MARK: - What the screens are drawn from

    @Test
    fun `a full status carries everything the app draws from`() {
        val status = decode(AgentStatus.serializer(), fixture("status.full.json"))

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
        val status = decode(AgentStatus.serializer(), fixture("status.partial.json"))
        assertEquals("a bounded status that ran out of budget has to admit it", true, status.timing?.partial)
    }

    @Test
    fun `a status whose config will not load blocks every change`() {
        val status = decode(AgentStatus.serializer(), fixture("status.config-invalid.json"))
        assertTrue("the app says this once at the top rather than per failed button", status.configBroken)
    }

    @Test
    fun `a process the agent could not read is unknown, never stopped`() {
        val status = decode(AgentStatus.serializer(), fixture("status.process-unknown.json"))
        val service = status.effectiveServices.single { it.process?.error != null }
        assertTrue(service.process!!.isUnknown)
        assertEquals("the mirrored running key says false and is no evidence", false, service.running)
    }

    @Test
    fun `a 2 x status still folds into one service`() {
        val status = decode(AgentStatus.serializer(), fixture("legacy-2x.status.json"))
        assertFalse("no contract key means the old paths stay", status.speaksV3)
        assertTrue(
            "the first agent spread one service across the top level; it is folded back here",
            status.effectiveServices.isNotEmpty(),
        )
    }

    @Test
    fun `an update reply that was replayed is the earlier record, not a second run`() {
        val reply = decode(AgentActionResult.serializer(), fixture("update.replayed.json"))
        assertEquals("retrying with the same id has to come back as the same operation", true, reply.replayed ?: reply.op?.replayed)
        assertEquals("it is followed exactly as an accepted one is", Outcome.PENDING, reply.op!!.effectiveOutcome)
    }

    @Test
    fun `a conflict names the operation holding the lock`() {
        val reply = decode(AgentActionResult.serializer(), fixture("update.conflict.json"))
        assertEquals("conflict", reply.effectiveAction)
        assertNotNull("saying \"busy\" is not enough; the app names what is in the way", reply.conflict)
        assertNotNull(reply.conflict?.opId)
    }

    @Test
    fun `a forced update with nothing to verify says it was not verified`() {
        val reply = decode(AgentActionResult.serializer(), fixture("update.forced-unverified.json"))
        assertEquals("force bypasses the busy gate, never the postcondition", false, reply.effectiveVerified)
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
        val unknownActions = mutableSetOf<String>()
        val unknownPhases = mutableSetOf<String>()

        fun checkRecord(record: OpRecord?, where: String) {
            val record = record ?: return
            record.result?.action?.let { if (OpAction.fromWire(it) == null) unknownActions += "$it ($where)" }
            record.phase?.let { if (OpPhase.fromWire(it) == null) unknownPhases += "$it ($where)" }
        }

        fun checkSummary(summary: OpSummary?, where: String) {
            val summary = summary ?: return
            summary.action?.let { if (OpAction.fromWire(it) == null) unknownActions += "$it ($where)" }
            summary.phase?.let { if (OpPhase.fromWire(it) == null) unknownPhases += "$it ($where)" }
        }

        val mutations = setOf(
            "update.schema.json", "restart.schema.json", "boot.schema.json", "sleep.schema.json", "run.schema.json",
            "cycle.schema.json", "cancel.schema.json", "self-update.schema.json", "error.schema.json",
            "legacy-2x-mutation.schema.json",
        )
        for (entry in entries.filter { it.text("kind") == "reply" }) {
            val file = entry.text("file")!!
            val text = fixture(file)
            when (entry.text("schema")) {
                "op.schema.json" -> {
                    val raw = AgentJson.parseToJsonElement(text) as JsonObject
                    if (raw["op"] !is JsonNull) checkRecord(readOperation(file, raw), file)
                }

                "status.schema.json" -> {
                    val block = decode(AgentStatus.serializer(), text).operations
                    (block?.running.orEmpty() + block?.queued.orEmpty() + block?.recent.orEmpty()).forEach { checkSummary(it, file) }
                }

                "history.schema.json" -> decode(HistoryReply.serializer(), text).entries.forEach { checkSummary(it, file) }

                in mutations -> {
                    val reply = decode(AgentActionResult.serializer(), text)
                    reply.action?.let { if (OpAction.fromWire(it) == null) unknownActions += "$it ($file)" }
                    checkRecord(reply.op, file)
                }
            }
        }

        assertTrue("unknown actions: $unknownActions", unknownActions.isEmpty())
        assertTrue("unknown phases: $unknownPhases", unknownPhases.isEmpty())
    }

    /** Every string value under [key], anywhere in the tree. */
    private fun collect(element: JsonElement, key: String): List<String> = when (element) {
        is JsonObject -> element.flatMap { (name, value) ->
            val here = if (name == key) listOfNotNull((value as? JsonPrimitive)?.contentOrNull?.takeIf { it.isNotBlank() }) else emptyList()
            here + collect(value, key)
        }

        is JsonArray -> element.flatMap { collect(it, key) }
        else -> emptyList()
    }
}
