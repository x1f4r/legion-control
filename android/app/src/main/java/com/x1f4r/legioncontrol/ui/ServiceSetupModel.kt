package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.*
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import kotlinx.serialization.json.*

/** A draft binds the full unmodified agent document to the hash returned by its last explicit read. */
data class ServiceConfigDraft(val expectedHash: JsonElement, val document: JsonObject, val maxBytes: Int = 1_048_576) {
    fun request(text: String): JsonObject {
        require(text.toByteArray(Charsets.UTF_8).size <= maxBytes) { "Service setup exceeds the agent's size limit." }
        val proposed = Json.parseToJsonElement(text) as? JsonObject ?: error("Service setup must be a JSON object.")
        val payload = buildJsonObject { put("expectedHash", expectedHash); put("document", proposed) }
        require(payload.toString().toByteArray(Charsets.UTF_8).size <= maxBytes) { "Service setup request exceeds the agent's size limit." }
        return payload
    }

    fun withProfile(currentText: String, profile: JsonObject): String {
        require(canAddAiProfile(profile) || canMonitorAiProfile(profile)) { "This AI tool has no usable service draft. Check its availability message." }
        val service = profile["service"]!!.jsonObject
        val updates = service["updates"] as? JsonObject ?: JsonObject(emptyMap())
        val disabled = JsonObject(service + ("updates" to JsonObject(updates + ("automatic" to JsonPrimitive(false)))))
        return withTemplate(currentText, JsonObject(profile + ("service" to disabled)))
    }

    fun withTemplate(currentText: String, template: JsonObject): String {
        val proposed = request(currentText)["document"]!!.jsonObject
        val service = template["service"] as? JsonObject ?: error("This template has no service draft.")
        val id = service["id"]?.jsonPrimitive?.contentOrNull ?: error("The template needs a service ID.")
        val services = proposed["services"] as? JsonArray ?: JsonArray(emptyList())
        require(services.none { (it as? JsonObject)?.get("id")?.jsonPrimitive?.contentOrNull == id }) {
            "A service with ID $id already exists. Edit that service or choose another template."
        }
        return PrettyServiceJson.encodeToString(JsonObject.serializer(), JsonObject(proposed + ("services" to JsonArray(services + service))))
    }
}

fun canAddAiProfile(profile: JsonObject): Boolean =
    (profile["detected"] as? JsonPrimitive)?.booleanOrNull == true &&
        (profile["availability"] as? JsonPrimitive)?.contentOrNull == "available" && profile["service"] is JsonObject

fun canMonitorAiProfile(profile: JsonObject): Boolean =
    (profile["detected"] as? JsonPrimitive)?.booleanOrNull == true &&
        (profile["availability"] as? JsonPrimitive)?.contentOrNull == "manual" && profile["service"] is JsonObject

internal val PrettyServiceJson = Json { prettyPrint = true }

@Stable
class ServiceSetupModel(private val client: MachineClient, private val scope: CoroutineScope, private val onSaved: () -> Unit) {
    var isOpen by mutableStateOf(false)
        private set
    var working by mutableStateOf(false)
        private set
    var text by mutableStateOf("")
        private set
    var error by mutableStateOf<String?>(null)
        private set
    var preview by mutableStateOf<String?>(null)
        private set
    var templates by mutableStateOf<List<JsonObject>>(emptyList())
        private set
    var profiles by mutableStateOf<List<JsonObject>>(emptyList())
        private set
    private var draft: ServiceConfigDraft? = null
    private var validatedText: String? = null
    val canSave: Boolean get() = !working && validatedText == text && draft != null

    fun edit(value: String) { text = value; validatedText = null; preview = null; error = null }
    fun close() { if (!working) isOpen = false }

    fun open() {
        if (working) return
        isOpen = true
        working = true
        error = null
        validatedText = null
        preview = null
        scope.launch {
            try {
                val reply = client.serviceConfigGet().getOrThrow()
                requireReply(reply)
                val document = reply["document"] as? JsonObject ?: error("The agent returned no editable document.")
                val hash = reply["hash"] ?: error("The agent returned no configuration hash.")
                val limit = ((reply["limits"] as? JsonObject)?.get("maxBytes") as? JsonPrimitive)?.intOrNull ?: 1_048_576
                draft = ServiceConfigDraft(hash, document, limit.coerceIn(1, 1_048_576))
                text = PrettyServiceJson.encodeToString(JsonObject.serializer(), document)
                templates = (reply["templates"] as? JsonArray)?.mapNotNull { it as? JsonObject }.orEmpty()
                profiles = (reply["profiles"] as? JsonArray)?.mapNotNull { it as? JsonObject }.orEmpty()
            } catch (failure: Exception) { error = failure.message ?: "Could not read service setup." }
            finally { working = false }
        }
    }

    fun useProfile(profile: JsonObject) {
        if (working) return
        try { edit(draft?.withProfile(text, profile) ?: error("Read the current service setup first.")) }
        catch (failure: Exception) { error = failure.message }
    }

    fun useTemplate(template: JsonObject) {
        if (working) return
        try { edit(draft?.withTemplate(text, template) ?: error("Read the current service setup first.")) }
        catch (failure: Exception) { error = failure.message }
    }

    fun validate() = send(save = false)
    fun save() { if (canSave) send(save = true) }

    private fun send(save: Boolean) {
        if (working) return
        val base = draft ?: return
        val captured = text
        val payload = try { base.request(captured) } catch (failure: Exception) { error = failure.message; return }
        working = true
        error = null
        scope.launch {
            try {
                val reply = client.serviceConfigWrite(if (save) "set" else "validate", payload).getOrThrow()
                requireReply(reply)
                check((reply["valid"] as? JsonPrimitive)?.booleanOrNull == true) { "The agent did not validate this service setup." }
                if (save) {
                    isOpen = false
                    onSaved()
                } else {
                    validatedText = captured
                    val changes = reply["changes"] as? JsonObject
                    preview = buildList {
                        for (key in listOf("added", "removed", "changed")) {
                            val value = changes?.get(key)
                            if (value != null) add("${key.replaceFirstChar { it.uppercase() }}: $value")
                        }
                        if ((changes?.get("otherSettingsChanged") as? JsonPrimitive)?.booleanOrNull == true) add("Other settings changed.")
                        (reply["warnings"] as? JsonArray)?.forEach { warning ->
                            val objectValue = warning as? JsonObject
                            add(objectValue?.let { listOfNotNull(it["path"]?.jsonPrimitive?.contentOrNull, it["message"]?.jsonPrimitive?.contentOrNull).joinToString(": ") }
                                ?: (warning as? JsonPrimitive)?.contentOrNull ?: warning.toString())
                        }
                    }.joinToString("\n").ifBlank { "Validation passed. Save applies this document." }
                }
            } catch (failure: Exception) { error = failure.message ?: "Could not validate service setup."; validatedText = null }
            finally { working = false }
        }
    }

    private fun requireReply(reply: JsonObject) {
        if ((reply["ok"] as? JsonPrimitive)?.booleanOrNull == true) return
        val code = (reply["reasonCode"] as? JsonPrimitive)?.contentOrNull
        if (code == "restricted") error("Service setup requires an administrator SSH key.")
        val summary = (reply["message"] as? JsonPrimitive)?.contentOrNull ?: (reply["error"] as? JsonPrimitive)?.contentOrNull ?: "The agent refused this service setup request."
        val fields = (reply["errors"] as? JsonArray)?.mapNotNull { value ->
            (value as? JsonObject)?.let { item -> listOfNotNull((item["path"] as? JsonPrimitive)?.contentOrNull, (item["message"] as? JsonPrimitive)?.contentOrNull).joinToString(": ") }
        }.orEmpty()
        error((listOf(summary) + fields).joinToString("\n"))
    }
}
