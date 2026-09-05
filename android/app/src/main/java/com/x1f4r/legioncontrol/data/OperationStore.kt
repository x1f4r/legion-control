package com.x1f4r.legioncontrol.data

import android.content.Context
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.agent.OperationRecord
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json
import java.io.File

/**
 * Every change this app has asked for, kept across launches.
 *
 * On a file rather than in preferences, because these are records rather than settings and because
 * the write has to be atomic: an app that is killed halfway through writing the one thing that says
 * what happened to an update is worse than one that never wrote it. Written beside the target and
 * moved into place, which is the same trick the key vault uses and for the same reason.
 *
 * The history is capped. It exists to answer "what happened to the thing I pressed", not to be an
 * audit log, and a phone that has been used for a year should not be carrying a year of it.
 */
class OperationStore(context: Context) {
    private val file = File(File(context.applicationContext.filesDir, "operations"), "operations.json")
    private val lock = Any()

    private val _records = MutableStateFlow(read())

    /** Newest first, which is the order every screen wants them in. */
    val records: StateFlow<List<OperationRecord>> = _records.asStateFlow()

    /** The ones that have not finished, which is what a phone coming back has to deal with. */
    val pending: List<OperationRecord> get() = _records.value.filter { it.isPending }

    fun get(id: String): OperationRecord? = _records.value.firstOrNull { it.id == id }

    fun forMachine(machineId: String): List<OperationRecord> =
        _records.value.filter { it.machineId == machineId }

    /**
     * Writes [record], replacing any earlier version of it.
     *
     * Returns what was stored, so a caller can chain a reduce onto the value that is actually on
     * disk rather than onto the one it happened to be holding.
     */
    fun put(record: OperationRecord): OperationRecord = synchronized(lock) {
        val current = _records.value
        val without = current.filterNot { it.id == record.id }
        val updated = (listOf(record) + without)
            .sortedByDescending { it.startedAt }
            .let { trim(it) }
        _records.value = updated
        write(updated)
        record
    }

    /** Applies [change] to the stored copy of [id], if it is still there. */
    fun update(id: String, change: (OperationRecord) -> OperationRecord): OperationRecord? {
        val existing = get(id) ?: return null
        return put(change(existing))
    }

    fun clearHistory() = synchronized(lock) {
        val kept = _records.value.filter { it.isPending }
        _records.value = kept
        write(kept)
    }

    /**
     * Drops records belonging to machines the configuration no longer names.
     *
     * Called at the same moment host keys and route hints are pruned. A machine that has left the
     * document cannot be looked at, so its history is unreachable rather than merely old.
     */
    fun retainOnly(machineIds: Set<String>) = synchronized(lock) {
        val kept = _records.value.filter { it.machineId in machineIds }
        if (kept.size == _records.value.size) return@synchronized
        _records.value = kept
        write(kept)
    }

    /**
     * Everything, as text, for the export action.
     *
     * Pretty printed and complete, including the log lines, because the reason to export is that
     * something went wrong in a way the screen could not explain on its own.
     */
    fun exportText(): String = Pretty.encodeToString(ListSerializer(OperationRecord.serializer()), _records.value)

    private fun trim(records: List<OperationRecord>): List<OperationRecord> {
        if (records.size <= MAX_RECORDS) return records
        // Pending work is never dropped to make room. It is the only kind that still has to be
        // resolved, and a phone that has been busy must not lose the record that stops a duplicate.
        val pending = records.filter { it.isPending }
        val finished = records.filter { it.isOver }
        return (pending + finished.take((MAX_RECORDS - pending.size).coerceAtLeast(0)))
            .sortedByDescending { it.startedAt }
    }

    private fun read(): List<OperationRecord> {
        if (!file.exists()) return emptyList()
        return runCatching {
            Lenient.decodeFromString(ListSerializer(OperationRecord.serializer()), file.readText())
        }.getOrDefault(emptyList())
            .map { record ->
                // A change that was in flight when the process died cannot still be in flight. It is
                // moved to "outcome unknown" so that the machine is asked rather than the command
                // being sent again, which is the whole point of writing these down. Disconnection
                // never proves a transition, and neither does the app being killed.
                if (record.state == TrackedState.DISPATCHED || record.state == TrackedState.REQUESTED) {
                    record.copy(state = TrackedState.OUTCOME_UNKNOWN, retryable = record.trackable)
                } else {
                    record
                }
            }
    }

    private fun write(records: List<OperationRecord>) {
        runCatching {
            file.parentFile?.mkdirs()
            val staging = File(file.parentFile, "${file.name}.new")
            staging.writeText(Compact.encodeToString(ListSerializer(OperationRecord.serializer()), records))
            if (!staging.renameTo(file)) {
                staging.copyTo(file, overwrite = true)
                staging.delete()
            }
        }
    }

    private companion object {
        const val MAX_RECORDS = 80

        val Compact = Json { encodeDefaults = true }

        val Lenient = Json {
            ignoreUnknownKeys = true
            isLenient = true
            // A record written by an older build is missing whatever was added since, and losing the
            // whole history because one field is new would defeat the purpose of keeping it.
            coerceInputValues = true
        }

        val Pretty = Json {
            prettyPrint = true
            prettyPrintIndent = "  "
            encodeDefaults = true
        }
    }
}
