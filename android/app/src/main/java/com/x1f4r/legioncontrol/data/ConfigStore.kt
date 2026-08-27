package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.security.MessageDigest

/** Where a setup was fetched from, so the three fields start where they were left. */
data class SetupSource(
    val host: String,
    val port: Int,
    val user: String,
)

/**
 * The configuration, kept as the text that was applied.
 *
 * The text and not the parsed object, because the text is what the user edits: a field that showed
 * a re-serialized document would reformat what they typed, lose the order they wrote things in, and
 * turn every visit into a diff they did not ask for.
 *
 * Ordinary app private preferences rather than the encrypted store next door. This document is
 * addresses, usernames and interpreter paths; none of it is a secret, and none of it is worth the
 * keystore round trip on every launch. The one thing in this app that is secret, the ssh key, is the
 * one that goes through [KeyVault].
 */
class ConfigStore(context: Context) {
    private val prefs: SharedPreferences =
        context.applicationContext.getSharedPreferences("legion-config", Context.MODE_PRIVATE)

    private val _text = MutableStateFlow(prefs.getString(KEY_TEXT, null).orEmpty())

    /** Exactly what is stored, so the field on screen starts as what was last applied. */
    val text: StateFlow<String> = _text.asStateFlow()

    private val _config = MutableStateFlow(readControllerConfig(_text.value).getOrNull())

    /** The configuration in force. Null means there is none, which is the first-run state. */
    val config: StateFlow<ControllerConfig?> = _config.asStateFlow()

    private val _hash = MutableStateFlow(prefs.getString(KEY_HASH, null))

    /**
     * The hash of the document in force, in the machines' own terms.
     *
     * It is what a machine's `controller.hash` is compared against, so it has to be the hash that
     * machine would report for the same bytes: the one it served the document with, or, for a
     * document typed in here, the hash of the form an agent stores it in. Being wrong about the
     * second costs one fetch that finds the phone already has the document, and nothing else.
     */
    val hash: StateFlow<String?> = _hash.asStateFlow()

    private val _source = MutableStateFlow(storedSource())

    /** The machine the setup was last fetched from. Null until one has served it. */
    val source: StateFlow<SetupSource?> = _source.asStateFlow()

    /**
     * Checks [text] and, if it holds up, stores it and puts it in force. Returns null on success or
     * the one sentence explaining why nothing changed.
     *
     * Nothing is stored on a failure, on purpose: an app that is working against a good
     * configuration must not lose it because a half-finished edit was applied by accident.
     */
    fun apply(text: String): String? = store(text, setupHashOf(text))

    /**
     * The same, for a document a machine served, remembering the hash it served it with.
     *
     * The machine's own hash rather than one computed here, because the point of the hash is to
     * agree with the machine about which document this is, and the document is written back out in
     * a shape the user can read, which is not the shape it was hashed in.
     */
    fun applyFetched(text: String, hash: String?): String? = store(text, hash)

    fun rememberSource(source: SetupSource) {
        prefs.edit()
            .putString(KEY_SOURCE_HOST, source.host)
            .putInt(KEY_SOURCE_PORT, source.port)
            .putString(KEY_SOURCE_USER, source.user)
            .apply()
        _source.value = source
    }

    private fun store(text: String, hash: String?): String? {
        val result = readControllerConfig(text)
        val failure = result.exceptionOrNull()
        if (failure != null) return failure.message ?: "That configuration could not be read."

        prefs.edit().putString(KEY_TEXT, text).putString(KEY_HASH, hash).apply()
        _hash.value = hash
        _text.value = text
        _config.value = result.getOrNull()
        return null
    }

    private fun storedSource(): SetupSource? {
        val host = prefs.getString(KEY_SOURCE_HOST, null)?.takeIf { it.isNotBlank() } ?: return null
        return SetupSource(
            host = host,
            port = prefs.getInt(KEY_SOURCE_PORT, 22),
            user = prefs.getString(KEY_SOURCE_USER, null).orEmpty(),
        )
    }

    private companion object {
        const val KEY_TEXT = "controller-config"
        const val KEY_HASH = "controller-hash"
        const val KEY_SOURCE_HOST = "setup-source-host"
        const val KEY_SOURCE_PORT = "setup-source-port"
        const val KEY_SOURCE_USER = "setup-source-user"
    }
}

/**
 * The hash a machine would report for this document.
 *
 * An agent stores what it is given trimmed, with one trailing newline, and hashes exactly the bytes
 * it wrote. Matching that here is what lets a document typed into the field on the phone be
 * recognised as the one the machines are already carrying, instead of being replaced by a copy of
 * itself on the next poll.
 */
internal fun setupHashOf(text: String): String {
    val stored = text.trim() + "\n"
    val digest = MessageDigest.getInstance("SHA-256").digest(stored.toByteArray(Charsets.UTF_8))
    return digest.joinToString("") { "%02x".format(it) }
}
