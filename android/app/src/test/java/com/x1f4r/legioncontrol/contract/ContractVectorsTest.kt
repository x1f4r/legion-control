package com.x1f4r.legioncontrol.contract

import com.x1f4r.legioncontrol.agent.Addresses
import com.x1f4r.legioncontrol.agent.AttemptClassifier
import com.x1f4r.legioncontrol.agent.AttemptReading
import com.x1f4r.legioncontrol.agent.AttemptWalk
import com.x1f4r.legioncontrol.agent.CommandLineRefusal
import com.x1f4r.legioncontrol.agent.CommandSafety
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind
import com.x1f4r.legioncontrol.agent.RemoteShell
import com.x1f4r.legioncontrol.agent.buildRemoteCommand
import com.x1f4r.legioncontrol.agent.grammarFor
import com.x1f4r.legioncontrol.agent.statusBudget
import com.x1f4r.legioncontrol.contract.VectorJson.boolean
import com.x1f4r.legioncontrol.contract.VectorJson.long
import com.x1f4r.legioncontrol.contract.VectorJson.string
import com.x1f4r.legioncontrol.contract.VectorJson.strings
import com.x1f4r.legioncontrol.contract.VectorJson.number
import com.x1f4r.legioncontrol.contract.VectorJson.text
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The shared conformance vectors, run through this app's own code.
 *
 * `contract/vectors/index.json` says which groups this client runs and exactly how many cases each
 * one has. Every group gets exactly one adapter, which turns a case's input into what this app does
 * with it, and the result has to be the case's `expect` in full. A group with a different number of
 * cases than the index says is a failure, and so is a missing file: nothing here is skipped.
 */
class ContractVectorsTest {

    @Test
    fun `identifier grammars match the contract`() = runFamily("identifiers")

    @Test
    fun `command lines, attempts, walks, budgets and addresses match the contract`() = runFamily("transport")

    /**
     * Every adapter is for a group this client consumes, and every consumed family is either adapted
     * in full or still listed as pending.
     */
    @Test
    fun `every consumed group has exactly one adapter`() {
        val consumed = VectorJson.consumedGroups()
        assertEquals("adapters for groups this client does not consume", emptySet<String>(), adapters.keys - consumed)

        val families = consumed.map { it.substringBefore('/') }.toSet()
        for (family in families) {
            val groups = consumed.filter { it.startsWith("$family/") }.toSet()
            val adapted = groups.filter { it in adapters }.toSet()
            if (family in PENDING_FAMILIES) {
                assertTrue("$family is adapted and still listed as pending", adapted.isEmpty())
            } else {
                assertEquals("$family groups without an adapter", emptySet<String>(), groups - adapted)
            }
        }
        assertEquals("pending families the index does not list", emptySet<String>(), PENDING_FAMILIES - families)
    }

    private fun runFamily(family: String) {
        val groups = VectorJson.consumedGroups().filter { it.startsWith("$family/") }
        assertTrue("vectors/index.json gives this client no $family groups", groups.isNotEmpty())
        val failures = mutableListOf<String>()
        for (key in groups) {
            val group = key.substringAfter('/')
            val adapter = adapters[key] ?: throw AssertionError("no adapter for $key")
            val cases = VectorJson.cases(family, group)
            assertEquals("$key case count", VectorJson.indexedCount(family, group), cases.size)
            for (case in cases) {
                val actual = try {
                    adapter(case.input)
                } catch (failure: Exception) {
                    failures += "$key ${case.name}: threw $failure"
                    continue
                }
                VectorJson.difference(case.expect, actual)?.let { failures += "$key ${case.name}: $it" }
            }
        }
        assertTrue(failures.joinToString("\n"), failures.isEmpty())
    }

    /** One adapter per consumed group, keyed `family/group`: a case's input to its full expect. */
    private val adapters: Map<String, (JsonObject) -> JsonObject> = mapOf(
        "identifiers/token" to identifier(Contract::isToken),
        "identifiers/configuredId" to identifier(Contract::isConfiguredId),
        "identifiers/setupId" to identifier(Contract::isSetupId),
        "identifiers/opId" to identifier(Contract::isOpId),
        "identifiers/duration" to identifier(Contract::isDuration),
        "identifiers/clockTime" to identifier(Contract::isClockTime),
        "identifiers/sha256" to identifier(Contract::isSha256Hex),

        "transport/grammars" to { input ->
            val choice = grammarFor(
                platform = input.string("platform"),
                shell = input.string("shell"),
                restricted = input.boolean("restricted"),
                restrictedSession = input.boolean("restrictedSession") == true,
                agentLength = input.long("agentLength")!!.toInt(),
            )
            buildJsonObject {
                put("grammar", text(choice.grammar?.wire))
                put("refusal", text(choice.refusal))
            }
        },

        "transport/commandLines" to { input ->
            val grammar = RemoteShell.entries.single { it.wire == input.string("grammar") }
            try {
                val line = buildRemoteCommand(grammar, input.strings("argv"))
                buildJsonObject {
                    put("line", line)
                    put("refusal", text(null))
                    put("index", number(null as Int?))
                }
            } catch (refusal: CommandLineRefusal) {
                buildJsonObject {
                    put("line", text(null))
                    put("refusal", refusal.code)
                    put("index", number(refusal.index))
                }
            }
        },

        // Android has no ssh log and cannot fail to start ssh, so the index gives it only the cases
        // with an empty log that were not a launch failure, through the post-channel classifier.
        "transport/classifications" to { input ->
            assertEquals("", input.string("sshLog"))
            assertEquals(false, input.boolean("launchFailed"))
            reading(
                AttemptClassifier.classify(
                    timedOut = input.boolean("timedOut")!!,
                    exitStatus = input.long("exitCode")?.toInt(),
                    stdout = input.string("stdout")!!,
                    stderr = input.string("stderr")!!,
                ),
            )
        },

        "transport/walks" to { input ->
            val safety = CommandSafety.entries.single { it.wire == input.string("safety") }
            val attempts = (input["attempts"] as JsonArray).map { it as JsonObject }.map { attempt ->
                val result = attempt["result"] as JsonObject
                val planned = AttemptWalk.Planned(
                    route = attempt.string("route")!!,
                    system = attempt.string("system")!!,
                    reading = AttemptReading(
                        kind = result.string("kind")?.let { kind -> FailureKind.entries.single { it.wire == kind } },
                        dispatch = DispatchStage.entries.single { it.wire == result.string("dispatch") },
                    ),
                )
                // The reading's own idea of route level has to be the case's, or the walk below is
                // being fed something the classifier would never produce.
                assertEquals(result.boolean("routeLevel"), planned.reading.routeLevel)
                planned
            }
            val simulation = AttemptWalk.simulate(safety, attempts)
            buildJsonObject {
                put("sent", JsonArray(simulation.sent.map(::JsonPrimitive)))
                put("ending", simulation.ending.wire)
            }
        },

        "transport/statusBudgets" to { input ->
            val budget = statusBudget(input.long("remainingMs")!!, input.long("attemptsMade")!!.toInt())
            buildJsonObject {
                put("attempt", budget != null)
                put("budgetMs", number(budget))
            }
        },

        "transport/hosts" to { input ->
            val form = input.string("value")?.let(Addresses::hostForm)
            buildJsonObject {
                put("valid", form != null)
                put("form", text(form?.wire))
            }
        },

        "transport/users" to { input ->
            buildJsonObject { put("valid", input.string("value")?.let(Addresses::isUser) == true) }
        },

        // A port is an integer: a string or a fraction is not one, whatever it would parse to.
        "transport/ports" to { input ->
            val value = (input["value"] as? JsonPrimitive)?.takeUnless { it.isString }?.content
            val port = value?.takeIf { Regex("^-?[0-9]+$").matches(it) }?.toIntOrNull()
            buildJsonObject { put("valid", port != null && Addresses.isPort(port)) }
        },
    )

    private fun identifier(check: (String?) -> Boolean): (JsonObject) -> JsonObject = { input ->
        buildJsonObject { put("valid", check(input.string("value"))) }
    }

    private fun reading(reading: AttemptReading): JsonObject = buildJsonObject {
        put("outcome", if (reading.isReply) "reply" else "failure")
        put("kind", text(reading.kind?.wire))
        put("dispatch", reading.dispatch.wire)
        put("routeLevel", reading.routeLevel)
    }

    private companion object {
        /**
         * Families this client consumes whose adapters arrive with the work that implements them.
         *
         * A debt, listed so it cannot be forgotten: the accounting test fails when a family here is
         * adapted, or when the index stops listing it, and the list is empty once the Android port
         * of every reference has landed.
         */
        val PENDING_FAMILIES = setOf(
            "hash", "outcome", "setup-acceptance", "launch", "setup-reconcile", "setup-merge", "wake",
        )
    }
}
