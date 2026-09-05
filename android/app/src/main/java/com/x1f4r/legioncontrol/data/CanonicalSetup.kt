package com.x1f4r.legioncontrol.data

import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.security.MessageDigest

/**
 * The one byte representation of a controller document, and its hash.
 *
 * There are four implementations of this rule in this repository, in four languages, and
 * `contract/hash-vectors.json` is what proves they agree. Getting it wrong is not subtle: one side
 * hashing the raw file while another hashes the trimmed form means a document without a final
 * newline has two hashes, and the two sides then disagree about which document they hold forever.
 *
 * Every step below exists because some editor, some shell redirect or some phone keyboard produces
 * bytes a person would call the same document and a hash would not.
 *
 * The trim set is spelled out rather than delegated to Kotlin's own `trim`, and that is the single
 * most important line in this file. `String.trim` follows `Char.isWhitespace`, which also strips a
 * non-breaking space, and an implementation that used it would produce the plain document's hash for
 * a document that is not the plain document and cannot even be parsed.
 */
object CanonicalSetup {

    /** Tab, line feed, vertical tab, form feed, carriage return, space. Nothing else, ever. */
    private val TRIM = charArrayOf('\u0009', '\u000A', '\u000B', '\u000C', '\u000D', '\u0020')
    private const val BOM = '\uFEFF'

    /** The document is not valid UTF-8, so it has no canonical form and no hash. */
    class NotUtf8(message: String) : IllegalArgumentException(message)

    /**
     * The canonical bytes of a document.
     *
     * Throws [NotUtf8] on input that is not valid UTF-8. Decoding it lossily would hash a document
     * full of replacement characters, hand back a stable hash for something nobody can read, and let
     * the two sides agree forever about rubbish.
     */
    fun canonicalBytes(bytes: ByteArray): ByteArray = encode(canonicalText(decodeStrictly(bytes)))

    /**
     * The canonical form of text that is already a valid Kotlin string.
     *
     * The UTF-8 question cannot arise here, so this is the whole of the rule and nothing else.
     */
    fun canonicalText(text: String): String {
        // 1. One leading byte order mark, and only a leading one. A BOM further in is a zero width
        //    no-break space inside the document and stays exactly where it is.
        val withoutBom = if (text.isNotEmpty() && text[0] == BOM) text.substring(1) else text

        // 2. Line endings. CRLF first, then any remaining lone CR, so a CRLF file does not turn into
        //    a blank line between every pair of lines.
        val unixEndings = withoutBom.replace("\r\n", "\n").replace('\r', '\n')

        // 3. Both ends, using the fixed set above and no language's idea of whitespace.
        var start = 0
        var end = unixEndings.length
        while (start < end && unixEndings[start].isTrimmable()) start += 1
        while (end > start && unixEndings[end - 1].isTrimmable()) end -= 1

        // 4. Exactly one trailing newline, whether the editor left none or twelve.
        return unixEndings.substring(start, end) + "\n"
    }

    /** Lowercase hex sha256 of the canonical bytes. */
    fun hashOf(bytes: ByteArray): String = digest(canonicalBytes(bytes))

    fun hashOf(text: String): String = digest(encode(canonicalText(text)))

    /**
     * The canonical text of raw bytes, or null when they are not UTF-8.
     *
     * The nullable form, for the places that have to carry on: a machine that served an unreadable
     * document is a machine with a problem, not a reason to take the app down.
     */
    fun canonicalTextOrNull(bytes: ByteArray): String? =
        runCatching { canonicalText(decodeStrictly(bytes)) }.getOrNull()

    private fun Char.isTrimmable(): Boolean {
        for (candidate in TRIM) if (this == candidate) return true
        return false
    }

    private fun decodeStrictly(bytes: ByteArray): String {
        val decoder = StandardCharsets.UTF_8.newDecoder()
            .onMalformedInput(CodingErrorAction.REPORT)
            .onUnmappableCharacter(CodingErrorAction.REPORT)
        return try {
            decoder.decode(ByteBuffer.wrap(bytes)).toString()
        } catch (failure: CharacterCodingException) {
            throw NotUtf8("the document is not valid UTF-8: ${failure.message ?: "malformed input"}")
        }
    }

    /**
     * UTF-8 bytes, refusing anything that cannot be encoded.
     *
     * A Kotlin string can hold an unpaired surrogate, which came from somewhere lossy and which the
     * default encoder would quietly turn into a question mark. Refusing is the same decision as
     * refusing invalid input bytes, for the same reason.
     */
    private fun encode(text: String): ByteArray {
        val encoder = StandardCharsets.UTF_8.newEncoder()
            .onMalformedInput(CodingErrorAction.REPORT)
            .onUnmappableCharacter(CodingErrorAction.REPORT)
        return try {
            val buffer = encoder.encode(java.nio.CharBuffer.wrap(text))
            ByteArray(buffer.remaining()).also(buffer::get)
        } catch (failure: CharacterCodingException) {
            throw NotUtf8("the document contains text that is not valid Unicode: ${failure.message}")
        }
    }

    private fun digest(bytes: ByteArray): String =
        MessageDigest.getInstance("SHA-256").digest(bytes)
            .joinToString("") { "%02x".format(it) }
}

/**
 * The hash a machine would report for this document.
 *
 * Kept as a free function because it is what every caller in the app actually wants, and because the
 * name says what it is for rather than how it works.
 */
internal fun setupHashOf(text: String): String = CanonicalSetup.hashOf(text)

/** The bytes the agent stores, which are the bytes that get hashed. */
internal fun canonicalSetupBytes(text: String): ByteArray =
    CanonicalSetup.canonicalText(text).toByteArray(Charsets.UTF_8)
