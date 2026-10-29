package com.x1f4r.legioncontrol.contract

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.longOrNull

/**
 * Reading vector cases, and comparing what an adapter produced with what a case expects.
 *
 * The comparison is the one `vectors/index.json` states for every client: objects by key set in any
 * order, arrays element by element, strings by code point, booleans and null by identity, numbers as
 * integers, and a field whose name ends in `Text` as the JSON document it holds, parsed, with object
 * key order ignored and number lexemes exact.
 */
object VectorJson {

    /** One case of one group, as the family file has it. */
    data class Case(val name: String, val consumers: List<String>, val input: JsonObject, val expect: JsonObject)

    /**
     * The fields every implementation reproduces for a vector of a family in the flat layout, which
     * is the `hash` family's: each vector is its own input, and these are its expect.
     */
    private val FLAT_EXPECT = listOf("canonicalBase64", "sha256", "accepted", "reject")

    /** Every case of [group] in [family] that names [consumer]. */
    fun cases(family: String, group: String, consumer: String = CONSUMER): List<Case> {
        val entry = familyEntry(family)
        val file = (entry["file"] as JsonPrimitive).content
        if ((entry["layout"] as? JsonPrimitive)?.content == "flat") {
            // Every consumer runs every vector of a flat file, which has no consumers of its own.
            val vectors = (ContractCorpus.json(file) as JsonObject)[group] as? JsonArray
                ?: throw AssertionError("$file has no $group")
            return vectors.map { it as JsonObject }.map { vector ->
                Case(
                    name = (vector["name"] as JsonPrimitive).content,
                    consumers = listOf(consumer),
                    input = vector,
                    expect = JsonObject(FLAT_EXPECT.associateWith { key -> vector[key] ?: throw AssertionError("$file vector has no $key") }),
                )
            }
        }
        val groups = (ContractCorpus.json(file) as JsonObject)["groups"] as? JsonObject
            ?: throw AssertionError("$file has no groups")
        val cases = groups[group] as? JsonArray ?: throw AssertionError("$file has no group $group")
        return cases.map { it as JsonObject }.map { case ->
            Case(
                name = (case["name"] as JsonPrimitive).content,
                consumers = (case["consumers"] as JsonArray).map { (it as JsonPrimitive).content },
                input = case["input"] as JsonObject,
                expect = case["expect"] as JsonObject,
            )
        }.filter { consumer in it.consumers }
    }

    /** How many cases of [group] in [family] the index says [consumer] runs, or null when none. */
    fun indexedCount(family: String, group: String, consumer: String = CONSUMER): Int? {
        val groups = familyEntry(family)["groups"] as JsonArray
        val entry = groups.map { it as JsonObject }.firstOrNull { (it["group"] as JsonPrimitive).content == group }
            ?: return null
        return ((entry["consumers"] as JsonObject)[consumer] as? JsonPrimitive)?.intOrNull
    }

    /** Every `family/group` the index lists [consumer] for. */
    fun consumedGroups(consumer: String = CONSUMER): Set<String> =
        (ContractCorpus.vectorIndex["families"] as JsonArray).map { it as JsonObject }.flatMap { family ->
            val name = (family["family"] as JsonPrimitive).content
            (family["groups"] as JsonArray).map { it as JsonObject }
                .filter { ((it["consumers"] as JsonObject)[consumer]) != null }
                .map { "$name/${(it["group"] as JsonPrimitive).content}" }
        }.toSet()

    /** A difference between [expected] and [actual], described with its path, or null when they agree. */
    fun difference(expected: JsonElement, actual: JsonElement, path: String = "expect"): String? = when {
        expected is JsonObject && actual is JsonObject -> {
            if (expected.keys != actual.keys) {
                "$path has keys ${actual.keys.sorted()}, expected ${expected.keys.sorted()}"
            } else {
                expected.keys.firstNotNullOfOrNull { key ->
                    val e = expected.getValue(key)
                    val a = actual.getValue(key)
                    if (key.endsWith("Text") && e is JsonPrimitive && e.isString && a is JsonPrimitive && a.isString) {
                        difference(Json.parseToJsonElement(e.content), Json.parseToJsonElement(a.content), "$path.$key")
                    } else {
                        difference(e, a, "$path.$key")
                    }
                }
            }
        }

        expected is JsonArray && actual is JsonArray ->
            if (expected.size != actual.size) {
                "$path has ${actual.size} elements, expected ${expected.size}"
            } else {
                expected.indices.firstNotNullOfOrNull { difference(expected[it], actual[it], "$path[$it]") }
            }

        expected is JsonNull || actual is JsonNull ->
            if (expected is JsonNull && actual is JsonNull) null else "$path is $actual, expected $expected"

        expected is JsonPrimitive && actual is JsonPrimitive ->
            if (expected.isString == actual.isString && expected.content == actual.content) {
                null
            } else {
                "$path is $actual, expected $expected"
            }

        else -> "$path is $actual, expected $expected"
    }

    // Small readers for adapters.

    fun JsonObject.string(key: String): String? = (this[key] as? JsonPrimitive)?.takeIf { it.isString }?.content

    fun JsonObject.boolean(key: String): Boolean? = (this[key] as? JsonPrimitive)?.takeUnless { it.isString }?.booleanOrNull

    fun JsonObject.long(key: String): Long? = (this[key] as? JsonPrimitive)?.takeUnless { it.isString }?.longOrNull

    fun JsonObject.strings(key: String): List<String> =
        (this[key] as JsonArray).map { (it as JsonPrimitive).contentOrNull ?: throw AssertionError("$key holds a null") }

    fun text(value: String?): JsonElement = value?.let(::JsonPrimitive) ?: JsonNull

    fun number(value: Long?): JsonElement = value?.let(::JsonPrimitive) ?: JsonNull

    fun number(value: Int?): JsonElement = value?.let(::JsonPrimitive) ?: JsonNull

    private fun familyEntry(family: String): JsonObject =
        (ContractCorpus.vectorIndex["families"] as JsonArray).map { it as JsonObject }
            .firstOrNull { (it["family"] as JsonPrimitive).content == family }
            ?: throw AssertionError("vectors/index.json lists no family $family")

    /** This client's name in every `consumers` list. */
    const val CONSUMER = "android"
}
