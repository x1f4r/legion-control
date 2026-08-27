package com.x1f4r.legioncontrol.data

import android.content.Context
import android.content.SharedPreferences
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The pasted configuration, kept as the text that was pasted.
 *
 * The text and not the parsed object, because the text is what the user edits: a field that showed
 * a re-serialized document would reformat what they typed, lose the order they wrote things in, and
 * turn every visit into a diff they did not ask for.
 *
 * Ordinary app private preferences rather than the encrypted store next door. This document is
 * addresses, usernames and interpreter paths; none of it is a secret, and none of it is worth the
 * keystore round trip on every launch. The two things in this app that are secret, the ssh key and
 * the GitHub token, are the ones that go through [KeyVault].
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

    /**
     * Checks [text] and, if it holds up, stores it and puts it in force. Returns null on success or
     * the one sentence explaining why nothing changed.
     *
     * Nothing is stored on a failure, on purpose: an app that is working against a good
     * configuration must not lose it because a half-finished edit was applied by accident.
     */
    fun apply(text: String): String? {
        val result = readControllerConfig(text)
        val failure = result.exceptionOrNull()
        if (failure != null) return failure.message ?: "That configuration could not be read."

        prefs.edit().putString(KEY_TEXT, text).apply()
        _text.value = text
        _config.value = result.getOrNull()
        return null
    }

    private companion object {
        const val KEY_TEXT = "controller-config"
    }
}
