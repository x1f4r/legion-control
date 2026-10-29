package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.contract.ContractCorpus
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The agent's own log, as the machine page shows it.
 *
 * The text of a line is under `line`. This app read `text`, which no agent has ever sent, so the log
 * view was empty on every machine however much the agent had written.
 */
class LogsReplyTest {

    @Test
    fun `every line of a log reply is read`() {
        for (name in listOf("logs.agent.json", "logs.operation.json")) {
            val raw = ContractCorpus.json("fixtures/$name") as JsonObject
            val expected = (raw["lines"] as JsonArray).map { ((it as JsonObject)["line"] as JsonPrimitive).content }
            val reply = AgentJson.decodeFromString(LogsReply.serializer(), raw.toString())

            assertTrue("$name holds no lines to read", expected.isNotEmpty())
            assertEquals(name, expected, reply.entries.map { it.line })
        }
    }
}
