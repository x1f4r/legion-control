package com.x1f4r.legioncontrol.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The canonical bytes of a setup document, in the cases worth stating on their own.
 *
 * There are four implementations of this rule in this repository, in four languages, and
 * `contract/hash-vectors.json` is what proves they agree: the `hash` family of the contract vector
 * test runs every one of them through [CanonicalSetup]. It matters because the hash is how a
 * machine and a phone decide whether they are carrying the same document: a rule that differs by one
 * space means the Mac retrying a successful write every ten minutes forever.
 */
class CanonicalSetupTest {

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

}
