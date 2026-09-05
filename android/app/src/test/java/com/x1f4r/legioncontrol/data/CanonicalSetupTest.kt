package com.x1f4r.legioncontrol.data

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import java.io.File
import java.util.Base64
import org.junit.Test

/**
 * The canonical bytes of a setup document, against the shared vectors.
 *
 * There are four implementations of this rule in this repository, in four languages, and
 * `contract/hash-vectors.json` is what proves they agree. It matters because the hash is how a
 * machine and a phone decide whether they are carrying the same document: a rule that differs by one
 * space means the Mac retrying a successful write every ten minutes forever, which is exactly what
 * every ten minutes forever.
 *
 * The vectors are read from the repository rather than copied in here. A copy would be a fifth
 * implementation of the thing being tested.
 */
class CanonicalSetupTest {

    private val json = Json { ignoreUnknownKeys = true }

    private fun vectorsFile(): File? = listOf(
        // Gradle runs unit tests with the module directory as the working directory.
        File("../../contract/hash-vectors.json"),
        File("../contract/hash-vectors.json"),
        File("contract/hash-vectors.json"),
    ).firstOrNull { it.isFile }

    @Test
    fun `every shared hash vector produces the same bytes and the same hash`() {
        val file = vectorsFile()
        assumeTrue(
            "contract/hash-vectors.json is not in this checkout, so there is nothing to compare with",
            file != null,
        )
        val root = json.parseToJsonElement(file!!.readText()) as JsonObject
        val vectors = (root["vectors"] as JsonArray).filterIsInstance<JsonObject>()
        assertTrue("the vector file should not be empty", vectors.isNotEmpty())

        var checked = 0
        var refused = 0
        for (vector in vectors) {
            val name = vector.string("name")!!
            val input = Base64.getDecoder().decode(vector.string("inputBase64").orEmpty())
            val expectedHash = vector.string("sha256")
            val expectedBytes = vector.string("canonicalBase64")

            if (expectedHash == null) {
                // A document that is not valid UTF-8 has no canonical form and no hash. An
                // implementation that decoded it lossily would hand back a stable hash for a
                // document full of replacement characters and agree with nobody.
                var threw = false
                try {
                    CanonicalSetup.hashOf(input)
                } catch (_: CanonicalSetup.NotUtf8) {
                    threw = true
                }
                assertTrue("$name should be refused as invalid UTF-8", threw)
                assertNull("$name should have no canonical text", CanonicalSetup.canonicalTextOrNull(input))
                refused += 1
                continue
            }

            assertEquals(
                "$name canonical bytes",
                expectedBytes,
                Base64.getEncoder().encodeToString(CanonicalSetup.canonicalBytes(input)),
            )
            assertEquals("$name hash", expectedHash, CanonicalSetup.hashOf(input))
            checked += 1
        }
        assertTrue("at least one vector should have been hashed", checked > 0)
        assertTrue("at least one vector should have been refused", refused > 0)
    }

    /**
     * The single most important line in the implementation, given its own test.
     *
     * Kotlin's own `String.trim` follows `Char.isWhitespace`, which strips a non-breaking space.
     * Using it would make a document beginning with one hash the same as the document without it,
     * which is a different document that cannot even be parsed. The vector file catches this too;
     * this states it in one line so a future edit that reaches for `trim()` fails here with the
     * reason attached.
     */
    @Test
    fun `a non-breaking space is not whitespace for this purpose`() {
        val document = "{\"version\":1,\"machines\":[]}"
        val withNbsp = " $document"
        assertTrue(withNbsp.trim() == document)
        assertTrue(
            "the non-breaking space has to survive canonicalisation",
            CanonicalSetup.canonicalText(withNbsp).startsWith(" "),
        )
    }

    @Test
    fun `line endings and trailing blank lines collapse to one newline`() {
        val document = "{\"version\":1,\"machines\":[]}"
        val variants = listOf(
            document,
            "$document\n",
            "$document\n\n\n",
            document.replace("\n", "\r\n") + "\r\n",
            "﻿$document",
            "  \t\n$document\n \t ",
        )
        val hashes = variants.map { CanonicalSetup.hashOf(it) }.toSet()
        assertEquals("every variant is the same document", 1, hashes.size)
        assertTrue(CanonicalSetup.canonicalText(document).endsWith("\n"))
        assertEquals(1, CanonicalSetup.canonicalText("$document\n\n\n").count { it == '\n' })
    }

    @Test
    fun `internal whitespace is part of the document`() {
        val spaced = "{ \"version\": 1, \"machines\": [] }"
        val compact = "{\"version\":1,\"machines\":[]}"
        assertNotNull(CanonicalSetup.hashOf(spaced))
        assertTrue(CanonicalSetup.hashOf(spaced) != CanonicalSetup.hashOf(compact))
    }

    private fun JsonObject.string(key: String): String? =
        (this[key] as? JsonPrimitive)?.contentOrNull
}
