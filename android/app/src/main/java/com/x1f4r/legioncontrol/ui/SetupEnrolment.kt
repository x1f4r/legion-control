package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * A setup fetched from a machine, from the fetch until Confirm or Cancel: enrolment (contract
 * section 9.9).
 *
 * The document is shown first, every command, key file and update source in it, and nothing of it
 * is applied by looking. Confirm is the only way it is put in force, and on a device that applies no
 * setup yet it approves everything that was shown; Cancel leaves this device exactly as it was. It
 * is given the services and nothing else, so that is what it can be held to without a phone.
 */
@Stable
class SetupEnrolment(private val services: ControlServices) {

    /** The fetched setup waiting for Confirm or Cancel, or null. */
    var preview: EnrolmentPreview? by mutableStateOf(null)
        private set

    /**
     * Shows [text], which [host] served, for Confirm or Cancel, and says what to do next. A document
     * that would be refused is said so now, rather than after somebody read through it.
     */
    fun offer(text: String, host: String): String =
        EnrolmentPreview.of(text = text, host = host, approves = services.document.value == null).fold(
            onSuccess = { found ->
                preview = found
                "Check what the setup from $host runs before using it."
            },
            onFailure = { "The setup from $host was refused. ${it.message}" },
        )

    /**
     * Puts the previewed setup in force. On a device with no setup yet this approves every launch
     * value the summary showed; on one that had a setup, the values it did not approve wait.
     * Returns what happened, or null when nothing was waiting.
     */
    fun confirm(): String? {
        val shown = preview ?: return null
        preview = null
        val failure = services.applyEnrolment(shown.text, services.deviceName())
        val found = services.config.value?.machines?.size ?: 0
        return failure?.let { "The setup from ${shown.host} was refused. $it" }
            ?: "The setup came from ${shown.host}: $found machine${if (found == 1) "" else "s"}."
    }

    /** Drops the previewed setup. Nothing was applied, and nothing is. Null when nothing was waiting. */
    fun cancel(): String? {
        val shown = preview ?: return null
        preview = null
        return "Nothing from ${shown.host} was applied."
    }
}
