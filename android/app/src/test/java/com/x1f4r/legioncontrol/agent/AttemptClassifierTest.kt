package com.x1f4r.legioncontrol.agent

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * What counts as a reply, held to `JSON.parse`, which is what the reference and the other clients
 * read the slice with.
 *
 * Every case below was checked against `jsonObjectSlice` in `contract/tools/transport.mjs`. A slice
 * the reference refuses is not the agent's account of anything, so here it has to fall through to
 * the rest of the table like any other output, and a mutation that printed it stays unknown rather
 * than being read as a success.
 */
class AttemptClassifierTest {

    private fun read(stdout: String) = AttemptClassifier.classify(timedOut = false, exitStatus = 0, stdout = stdout, stderr = "")

    @Test
    fun `a value JSON does not have is not a reply`() {
        // kotlinx reads a bare word where a value goes as a literal of that spelling, so these all
        // used to parse, and the lenient decoder behind it turned some into a success.
        val refused = listOf(
            """{"ok":tru}""",
            """{"ok":True}""",
            """{"a":01}""",
            """{"a":-01}""",
            """{"a":00}""",
            """{"a":NaN}""",
            """{"a":Infinity}""",
            """{"a":-Infinity}""",
            """{"a":abc}""",
            """{"a":'x'}""",
            """{"a":+1}""",
            """{"a":.5}""",
            """{"a":-1.}""",
            """{"a":1e}""",
            """{"a":1E+}""",
            """{"a":0x10}""",
            """{"a":-}""",
            """{"a":nulll}""",
            "{\"a\": 1}",
            "{\"a\":1\u000b}",
            """{"ok":true,"contract":3,"action":restarted}""",
            // What the parse itself refuses, which the comment on it promises.
            """{ok:true}""",
            """{'ok':true}""",
            """{"a":1,}""",
            """{"a":[1,]}""",
            """{"a":1 /* note */}""",
            """{"a":"\x"}""",
        )
        for (stdout in refused) {
            assertEquals(stdout, AttemptReading.failure(FailureKind.UNREADABLE_OUTPUT, DispatchStage.AMBIGUOUS), read(stdout))
        }
    }

    @Test
    fun `a control character left raw inside a string is not a reply`() {
        // Escaped it is any other character. Raw, JSON.parse refuses the whole text, and kotlinx kept it.
        listOf("{\"a\":\"line\nbreak\"}", "{\"a\":\"tab\there\"}", "{\"a\":\"nul\u0000\"}", "{\"line\nbreak\":1}").forEach { stdout ->
            assertEquals(stdout, AttemptReading.failure(FailureKind.UNREADABLE_OUTPUT, DispatchStage.AMBIGUOUS), read(stdout))
        }
    }

    @Test
    fun `plain JSON is still a reply, however it is spaced and whatever its strings hold`() {
        listOf(
            """{"a":-0.5e+10,"b":[true,false,null],"c":{"d":"é \"q\" \\"}}""",
            """{"a":1e999,"b":-0,"c":0.0e-0,"d":10,"e":2E5}""",
            "{ \"a\" :\t1\r\n,\n\"b\": [ ] , \"c\" : { } }",
            "{\"a\":\"line\\nbreak\\ttab\\u0000\"}",
            "{\"a\":\"del\u007f and  \"}",
            """{"a":1,"a":2}""",
            """{"":"empty key"}""",
            "#< CLIXML\r\n{\"ok\":true}\r\n",
        ).forEach { stdout -> assertEquals(stdout, AttemptReading.REPLY, read(stdout)) }
    }

    @Test
    fun `nesting past what this app reads is unreadable output, not an error out of the parser`() {
        // The one place this reads differently from the reference, which takes any depth. kotlinx
        // reads arrays by recursion, and this used to end in a StackOverflowError that no catch
        // around the parse saw.
        fun nested(depth: Int, inner: String = "") = "{\"a\":" + "[".repeat(depth - 1) + inner + "]".repeat(depth - 1) + "}"
        assertTrue(read(nested(512)).isReply)
        assertEquals(FailureKind.UNREADABLE_OUTPUT, read(nested(512, "tru")).kind)
        assertEquals(AttemptReading.failure(FailureKind.UNREADABLE_OUTPUT, DispatchStage.AMBIGUOUS), read(nested(513)))
        assertEquals(AttemptReading.failure(FailureKind.UNREADABLE_OUTPUT, DispatchStage.AMBIGUOUS), read(nested(100_000)))
        // Brackets inside a string are text, and do not count.
        assertTrue(read("{\"a\":\"" + "[".repeat(100_000) + "\"}").isReply)
    }
}
