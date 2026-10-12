package com.x1f4r.legioncontrol.contract

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import java.io.File

/**
 * The shared contract corpus: `contract/` at the root of the repository.
 *
 * Found through the `legion.contract` system property that the Gradle test task sets, and never by
 * guessing at working directories. A corpus that cannot be found is a failure, not a skip: a test
 * that quietly passes because the files were not there is the reason the fixture checks used to prove
 * nothing.
 */
object ContractCorpus {
    val root: File by lazy {
        val path = System.getProperty(PROPERTY)
            ?: throw AssertionError(
                "$PROPERTY is not set, so the shared contract corpus cannot be found. Run the tests " +
                    "through Gradle, which points it at the repository's contract directory.",
            )
        File(path).also {
            if (!it.isDirectory) throw AssertionError("$PROPERTY points at $it, which is not a directory.")
        }
    }

    /** A file in the corpus, by its path relative to `contract/`. Missing is a failure. */
    fun file(relative: String): File = root.resolve(relative).also {
        if (!it.isFile) throw AssertionError("contract/$relative is missing; the Android tests need the shared corpus.")
    }

    fun json(relative: String): JsonElement = Plain.parseToJsonElement(file(relative).readText())

    /** `vectors/index.json`, which says which groups this client runs and how many cases each has. */
    val vectorIndex: JsonObject by lazy { json("vectors/index.json") as JsonObject }

    private const val PROPERTY = "legion.contract"

    /** Strict on purpose: the corpus is generated, and anything lenient would hide a broken file. */
    private val Plain = Json
}
