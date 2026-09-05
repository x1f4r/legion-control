package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.agent.Contract
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.longOrNull
import java.util.Locale
import java.util.UUID

/**
 * The shared setup document, edited as JSON rather than as a data class.
 *
 * This is the one rule a typed model cannot satisfy: an edit must
 * preserve every key it does not understand. A document written by a newer client, or by a hand
 * that put a comment field in, is round-tripped through [ControllerConfig] as the subset this build
 * knows about, and everything else would be silently dropped on the first edit made from a phone.
 * Once one peer drops a key, the document diverges for everybody.
 *
 * So editing happens on the parsed [JsonObject]. [ControllerConfig] stays the reader, because the
 * validation and the UI want types; this is the writer.
 */
class ControllerDocument private constructor(
    /** The parsed tree, which is the source of truth for an edit. */
    val root: JsonObject,
) {
    /** The exact text a machine would store and hash. */
    val canonicalText: String by lazy { CanonicalSetup.canonicalText(Pretty.encodeToString(JsonObject.serializer(), root)) }

    val hash: String by lazy { CanonicalSetup.hashOf(canonicalText) }

    /** The setup block, or null on a document that has never had one. */
    val identity: DocumentIdentity? get() = DocumentIdentity.from(root["controller"] as? JsonObject)

    val setupId: String? get() = identity?.id
    val revision: Long get() = identity?.revision ?: 0L
    val lineage: List<String> get() = identity?.lineage.orEmpty()

    /** Everything this build understands, for the UI and the validator. */
    fun typed(): Result<ControllerConfig> = readControllerConfig(canonicalText)

    /** The same document with a different tree. Used by every edit below. */
    private fun with(tree: JsonObject) = ControllerDocument(tree)

    /**
     * A new revision of this document, descending from it.
     *
     * The revision counts up, the current hash goes to the front of the ancestry, and the ancestry
     * is truncated. The edit becomes the applied document at once, because an offline edit
     * is a first-class edit; publishing is what happens on the next poll of each machine.
     */
    fun asEditOf(
        base: ControllerDocument,
        deviceName: String?,
        now: String,
    ): ControllerDocument {
        val baseIdentity = base.identity
        val id = baseIdentity?.id?.takeIf { it.isNotBlank() } ?: newSetupId()
        val lineage = (listOf(base.hash) + base.lineage).distinct().take(MAX_LINEAGE)
        return withIdentity(
            DocumentIdentity(
                id = id,
                name = baseIdentity?.name ?: identity?.name,
                revision = maxOf(base.revision, revision) + 1L,
                updatedAt = now,
                source = CLIENT_KIND,
                device = deviceName,
                lineage = lineage,
            ),
        )
    }

    /**
     * The identity a document that has never had one gets.
     *
     * A pasted or hand-written document becomes an ordinary setup with a fresh id, revision 1 and
     * no ancestry. It is deliberately not tied to the device any more: the phone is a peer, and
     * a setup it created is the fleet's setup rather than the phone's.
     */
    fun withFreshIdentity(deviceName: String?, now: String): ControllerDocument = withIdentity(
        DocumentIdentity(
            id = newSetupId(),
            name = identity?.name,
            revision = 1L,
            updatedAt = now,
            source = CLIENT_KIND,
            device = deviceName,
            lineage = emptyList(),
        ),
    )

    fun withIdentity(identity: DocumentIdentity): ControllerDocument {
        val existing = root["controller"] as? JsonObject ?: JsonObject(emptyMap())
        // Every key of the block that this build does not know about is carried through, exactly as
        // the rest of the document is.
        val merged = buildJsonObject {
            existing.forEach { (key, value) -> put(key, value) }
            put("id", JsonPrimitive(identity.id))
            identity.name?.let { put("name", JsonPrimitive(it)) }
            put("revision", JsonPrimitive(identity.revision))
            identity.updatedAt?.let { put("updatedAt", JsonPrimitive(it)) }
            identity.source?.let { put("source", JsonPrimitive(it)) }
            identity.device?.let { put("device", JsonPrimitive(it)) }
            put("lineage", buildJsonArray { identity.lineage.forEach { add(JsonPrimitive(it)) } })
        }
        return with(JsonObject(root + ("controller" to merged)))
    }

    // MARK: - Structured edits

    /** Replaces the whole of one top-level key, or removes it when [value] is null. */
    fun setTop(key: String, value: JsonElement?): ControllerDocument =
        with(if (value == null) JsonObject(root - key) else JsonObject(root + (key to value)))

    private fun machinesArray(): List<JsonObject> =
        (root["machines"] as? JsonArray).orEmpty().filterIsInstance<JsonObject>()

    private fun sitesArray(): List<JsonObject> =
        (root["sites"] as? JsonArray).orEmpty().filterIsInstance<JsonObject>()

    private fun withMachines(machines: List<JsonObject>): ControllerDocument =
        setTop("machines", JsonArray(machines))

    private fun withSites(sites: List<JsonObject>): ControllerDocument =
        if (sites.isEmpty() && root["sites"] == null) this else setTop("sites", JsonArray(sites))

    /** Applies [change] to the machine with [id], keeping every key it does not touch. */
    fun editMachine(id: String, change: (JsonObject) -> JsonObject): ControllerDocument =
        withMachines(machinesArray().map { if (it.text("id") == id) change(it) else it })

    fun addMachine(id: String, name: String): ControllerDocument = withMachines(
        machinesArray() + buildJsonObject {
            put("id", JsonPrimitive(id))
            put("name", JsonPrimitive(name))
            put("endpoints", JsonArray(emptyList()))
            put("systems", JsonArray(emptyList()))
        },
    )

    fun removeMachine(id: String): ControllerDocument =
        withMachines(machinesArray().filterNot { it.text("id") == id })

    fun addSite(id: String, name: String): ControllerDocument = withSites(
        sitesArray() + buildJsonObject {
            put("id", JsonPrimitive(id))
            put("name", JsonPrimitive(name))
            put("lanPrefixes", JsonArray(emptyList()))
            put("broadcast", JsonArray(emptyList()))
        },
    )

    fun editSite(id: String, change: (JsonObject) -> JsonObject): ControllerDocument =
        withSites(sitesArray().map { if (it.text("id") == id) change(it) else it })

    fun removeSite(id: String): ControllerDocument {
        // A machine pointing at a site that has gone would not validate, so the pointer goes with it.
        val cleaned = machinesArray().map { machine ->
            if (machine.text("site") == id) JsonObject(machine - "site") else machine
        }
        return withSites(sitesArray().filterNot { it.text("id") == id }).withMachines(cleaned)
    }

    /**
     * Replaces the wake helpers of one machine, keeping the singular alias in step.
     *
     * An editor that writes `helpers` also writes `helper` as the first of them, because a document
     * is read by whatever build somebody else is still running and the older ones only know the
     * singular form.
     */
    fun setHelpers(machineId: String, helpers: List<WakeHelperConfig>): ControllerDocument =
        editMachine(machineId) { machine ->
            val wake = machine["wake"] as? JsonObject ?: return@editMachine machine
            val list = buildJsonArray {
                helpers.forEach { helper ->
                    add(
                        buildJsonObject {
                            put("machine", JsonPrimitive(helper.machine))
                            put("action", JsonPrimitive(helper.action))
                        },
                    )
                }
            }
            val updated = if (helpers.isEmpty()) {
                JsonObject(wake - "helpers" - "helper")
            } else {
                JsonObject(
                    wake + mapOf(
                        "helpers" to list,
                        "helper" to list.first(),
                    ),
                )
            }
            JsonObject(machine + ("wake" to updated))
        }

    companion object {
        /** This client's kind, on the wire. Fixed by the contract's controllerSource enum. */
        const val CLIENT_KIND = "phone"

        private val Lenient = Json {
            ignoreUnknownKeys = true
            isLenient = true
        }

        private val Pretty = Json {
            prettyPrint = true
            prettyPrintIndent = "  "
        }

        /** Reads a document, or explains in one sentence why it is not one. */
        fun parse(text: String): Result<ControllerDocument> {
            val canonical = try {
                CanonicalSetup.canonicalText(text)
            } catch (failure: CanonicalSetup.NotUtf8) {
                return Result.failure(IllegalArgumentException(failure.message))
            }
            val tree = try {
                Lenient.parseToJsonElement(canonical)
            } catch (failure: Exception) {
                return Result.failure(
                    IllegalArgumentException(
                        "That is not a configuration this app can read: " +
                            (failure.message?.lineSequence()?.firstOrNull()?.trim() ?: "unreadable"),
                    ),
                )
            }
            if (tree !is JsonObject) {
                return Result.failure(IllegalArgumentException("A setup has to be a JSON object."))
            }
            return Result.success(ControllerDocument(tree))
        }

        fun of(root: JsonObject): ControllerDocument = ControllerDocument(root)

        /** A fresh setup id. A uuid, because two peers must never independently pick the same one. */
        fun newSetupId(): String = "setup-" + UUID.randomUUID().toString().lowercase(Locale.ROOT)
    }
}

/** The `controller` block, as this build reads it. */
data class DocumentIdentity(
    val id: String,
    val name: String? = null,
    val revision: Long = 0L,
    val updatedAt: String? = null,
    val source: String? = null,
    val device: String? = null,
    val lineage: List<String> = emptyList(),
) {
    /** Who to blame for this revision, in words, for the divergence screen. */
    fun describeAuthor(): String {
        val who = device?.takeIf { it.isNotBlank() }
        val kind = com.x1f4r.legioncontrol.agent.ControllerSource.describe(source)
        return listOfNotNull(who, kind?.let { "on $it" }).joinToString(" ").ifBlank { "somebody" }
    }

    companion object {
        fun from(block: JsonObject?): DocumentIdentity? {
            val id = block?.text("id")?.takeIf { it.isNotBlank() && Contract.isToken(it) } ?: return null
            return DocumentIdentity(
                id = id,
                name = block.text("name"),
                revision = block.long("revision") ?: 0L,
                updatedAt = block.text("updatedAt"),
                source = block.text("source"),
                device = block.text("device"),
                lineage = (block["lineage"] as? JsonArray)
                    .orEmpty()
                    .mapNotNull { (it as? JsonPrimitive)?.contentOrNull }
                    .filter { it.isNotBlank() },
            )
        }
    }
}

internal fun JsonObject.text(key: String): String? =
    (this[key] as? JsonPrimitive)?.takeIf { it !is JsonNull }?.contentOrNull?.takeIf { it.isNotBlank() }

internal fun JsonObject.long(key: String): Long? = (this[key] as? JsonPrimitive)?.let {
    it.longOrNull ?: it.contentOrNull?.toLongOrNull()
}

internal fun JsonArray?.orEmpty(): List<JsonElement> = this ?: emptyList()

/** Sets or removes a string field, leaving every other key exactly where it was. */
internal fun JsonObject.withText(key: String, value: String?): JsonObject {
    val trimmed = value?.trim()
    return if (trimmed.isNullOrEmpty()) JsonObject(this - key) else JsonObject(this + (key to JsonPrimitive(trimmed)))
}

/** Replaces one array of objects, keeping the rest of the object untouched. */
internal fun JsonObject.withList(
    key: String,
    transform: (List<JsonObject>) -> List<JsonObject>,
): JsonObject {
    val existing = (this[key] as? JsonArray).orEmpty().filterIsInstance<JsonObject>()
    return JsonObject(this + (key to JsonArray(transform(existing))))
}

/** Edits the wake block, creating it if a machine did not have one. */
internal fun JsonObject.withWake(transform: (JsonObject) -> JsonObject): JsonObject {
    val wake = this["wake"] as? JsonObject ?: JsonObject(emptyMap())
    val updated = transform(wake)
    return if (updated.isEmpty()) JsonObject(this - "wake") else JsonObject(this + ("wake" to updated))
}
