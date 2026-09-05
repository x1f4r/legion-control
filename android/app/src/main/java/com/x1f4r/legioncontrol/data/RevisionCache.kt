package com.x1f4r.legioncontrol.data

import android.content.Context
import java.io.File

/**
 * The last few documents this device has held, kept by hash.
 *
 * A three-way merge needs the bytes of the common ancestor, and the only device that has them is one
 * that kept them. The Mac and the desktop already keep a `revisions/` directory; this is the phone's
 * version of it, and it is deliberately small: thirty documents of a few kilobytes each.
 *
 * Without it a divergence still merges, but every differing entry becomes a two-way choice instead of
 * only the ones both sides actually touched. That is a worse afternoon, not a wrong answer.
 */
class RevisionCache(context: Context) {
    private val directory = File(context.applicationContext.filesDir, "revisions")

    /** Writes [text] under its hash. Doing it twice costs nothing. */
    fun remember(hash: String, text: String) {
        if (!hash.matches(HASH)) return
        runCatching {
            directory.mkdirs()
            val file = File(directory, "$hash.json")
            if (file.exists()) {
                // Touched rather than rewritten, so the pruning below drops the least recently
                // useful rather than the least recently written.
                file.setLastModified(System.currentTimeMillis())
                return@runCatching
            }
            val staging = File(directory, "$hash.json.new")
            staging.writeText(text)
            if (!staging.renameTo(file)) {
                staging.copyTo(file, overwrite = true)
                staging.delete()
            }
            prune()
        }
    }

    /** The document with that hash, or null. */
    fun get(hash: String?): String? {
        val key = hash?.takeIf { it.matches(HASH) } ?: return null
        val file = File(directory, "$key.json")
        if (!file.exists()) return null
        return runCatching { file.readText() }.getOrNull()
    }

    fun has(hash: String?): Boolean = get(hash) != null

    /** Every hash held, newest first. */
    fun hashes(): List<String> = runCatching {
        directory.listFiles()
            ?.filter { it.name.endsWith(".json") }
            ?.sortedByDescending { it.lastModified() }
            ?.map { it.name.removeSuffix(".json") }
            .orEmpty()
    }.getOrDefault(emptyList())

    private fun prune() {
        val files = directory.listFiles()?.filter { it.name.endsWith(".json") } ?: return
        if (files.size <= MAX_REVISIONS) return
        files.sortedByDescending { it.lastModified() }
            .drop(MAX_REVISIONS)
            .forEach { it.delete() }
    }

    private companion object {
        val HASH = Regex("[a-f0-9]{64}")
        /** The last thirty distinct applied documents, which is enough to merge against. */
        const val MAX_REVISIONS = 30
    }
}
