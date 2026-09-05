package com.x1f4r.legioncontrol.ui

import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test

class ServiceConfigDraftTest {
    private val document = Json.parseToJsonElement("""{"services":[],"future":{"array":[1,true,null]},"telemetry":{"probes":[]}}""").jsonObject
    private val draft = ServiceConfigDraft(JsonPrimitive("old-hash"), document)

    @Test fun `validate and save payload retain original expected hash and unknown fields`() {
        val payload = draft.request(document.toString())
        assertEquals(JsonPrimitive("old-hash"), payload["expectedHash"])
        assertEquals(document, payload["document"])
    }

    @Test fun `template appends one draft without dropping unrelated configuration`() {
        val template = Json.parseToJsonElement("""{"id":"demo","service":{"id":"demo","type":"command","extra":["kept"]}}""").jsonObject
        val text = draft.withTemplate(document.toString(), template)
        val after = Json.parseToJsonElement(text).jsonObject
        assertEquals(document["future"], after["future"])
        assertEquals(document["telemetry"], after["telemetry"])
        assertEquals(template["service"], after["services"]!!.jsonArray.single())
        assertThrows(IllegalArgumentException::class.java) { draft.withTemplate(text, template) }
    }

    @Test fun `size limit measures UTF8 bytes and refuses oversized drafts`() {
        val limited = ServiceConfigDraft(JsonNull, document, 10)
        assertThrows(IllegalArgumentException::class.java) { limited.request("""{"x":"üü"}""") }
    }

    @Test fun `configured telemetry preserves unavailable readings and no implicit metric list`() {
        val json = Json { ignoreUnknownKeys = true }
        val status = json.decodeFromString<com.x1f4r.legioncontrol.agent.AgentStatus>("""{"ok":true,"metrics":[{"id":"temp","name":"Temperature","value":null,"unit":"C","checkedAt":null,"error":"probe unavailable"}]}""")
        assertNull(status.metrics!!.single().value)
        assertEquals("probe unavailable", status.metrics.single().error)
        assertNull(json.decodeFromString<com.x1f4r.legioncontrol.agent.AgentStatus>("""{"ok":true}""").metrics)
    }
    @Test fun `AI profile requires detection and explicitly available management`() {
        val profile = Json.parseToJsonElement("""{"id":"assistant","detected":true,"availability":"available","service":{"id":"assistant","updates":{"automatic":true,"future":"preserved"},"opaque":[1,true]}}""").jsonObject
        assertTrue(canAddAiProfile(profile))
        val after = Json.parseToJsonElement(draft.withProfile(document.toString(), profile)).jsonObject
        val service = after["services"]!!.jsonArray.single().jsonObject
        assertEquals(JsonPrimitive(false), service["updates"]!!.jsonObject["automatic"])
        assertEquals(JsonPrimitive("preserved"), service["updates"]!!.jsonObject["future"])
        assertEquals(profile["service"]!!.jsonObject["opaque"], service["opaque"])
        for (availability in listOf("not-installed", "unavailable")) {
            val unavailable = JsonObject(profile + ("availability" to JsonPrimitive(availability)))
            assertFalse(canAddAiProfile(unavailable))
            assertThrows(IllegalArgumentException::class.java) { draft.withProfile(document.toString(), unavailable) }
        }
        assertFalse(canAddAiProfile(JsonObject(profile + ("detected" to JsonPrimitive(false)))))
        assertFalse(canAddAiProfile(JsonObject(profile + ("service" to JsonNull))))
    }
    @Test fun `detected manual profile adds monitoring without inventing updater commands`() {
        val profile = Json.parseToJsonElement("""{"detected":true,"availability":"manual","message":"Updates managed by application","service":{"id":"desktop","installedVersion":["version"],"updates":{"automatic":false}}}""").jsonObject
        assertFalse(canAddAiProfile(profile))
        assertTrue(canMonitorAiProfile(profile))
        val after = Json.parseToJsonElement(draft.withProfile(document.toString(), profile)).jsonObject
        assertEquals(profile["service"], after["services"]!!.jsonArray.single())
        assertFalse(canMonitorAiProfile(JsonObject(profile + ("detected" to JsonPrimitive(false)))))
        assertFalse(canMonitorAiProfile(JsonObject(profile + ("service" to JsonNull))))
    }

}
