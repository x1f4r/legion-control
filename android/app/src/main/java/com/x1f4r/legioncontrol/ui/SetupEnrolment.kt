package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.data.CanonicalSetup
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.FetchedCopy
import com.x1f4r.legioncontrol.data.SetupOutcome

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
     * Shows what [host] served, for Confirm or Cancel, and says what to do next.
     *
     * Only bytes that hash to what the machine reported are shown (contract section 9.5). An agent
     * before 3.1.0 sends no `raw`, and a copy rebuilt from what it did send that does not reproduce
     * its hash is a document no machine holds: taken, it was a revision every machine diverged from.
     *
     * Then by ancestry, as a fetch on a status decides it ([FetchedCopy]). The copy this device
     * applies already, or one it was made from, is said so and nothing waits. A branch of this
     * device's copy goes to be merged, as one a machine carries does. Anything else waits for
     * Confirm. A document that would be refused is said so now, rather than after somebody read
     * through it.
     *
     * Confirm is held to the hash in storage, the head every compare-and-swap is made against, and
     * not to the document loaded: a stored copy that cannot be read loads as none, and fetching is
     * how a phone in that state is put right.
     */
    fun offer(fetched: ControllerFetch.Document, host: String): String {
        preview = null
        val served = fetched.replyHash
        if (served == null || CanonicalSetup.hashOf(fetched.text) != served) return unverifiable(host, "used")
        val applied = services.document.value
        val found = EnrolmentPreview.of(
            text = fetched.text,
            host = host,
            servedHash = served,
            head = services.appliedHash(),
            approves = applied == null,
        ).getOrElse { return "The setup from $host was refused. ${it.message}" }
        val document = ControllerDocument.parse(fetched.text).getOrNull()
        when (document?.let { FetchedCopy.of(applied, it) }) {
            FetchedCopy.SAME -> return "This device already has the setup $host holds."
            FetchedCopy.ANCESTOR ->
                return "This device's setup was made from the one $host holds, so there is nothing newer to take."

            FetchedCopy.DIVERGED -> {
                services.recordDivergence(
                    host,
                    host,
                    "This device and $host each have a copy of the setup that was edited separately, and neither was made from the other.",
                    fetched.text,
                )
                return "$host has a copy of the setup that was edited separately. It is waiting to be merged."
            }

            else -> Unit
        }
        preview = found
        return "Check what the setup from $host runs before using it."
    }

    /**
     * Puts the previewed setup in force, if the setup here is still the one it was shown against.
     * On a device with no setup yet this approves every launch value the summary showed; on one that
     * had a setup, the values it did not approve wait. Returns what happened, or null when nothing
     * was waiting.
     */
    fun confirm(): String? {
        val shown = preview ?: return null
        preview = null
        return when (val outcome = services.applyEnrolment(shown.text, shown.servedHash, shown.head, shown.host, services.deviceName())) {
            is SetupOutcome.Applied -> {
                val found = services.config.value?.machines?.size ?: 0
                "The setup came from ${shown.host}: $found machine${if (found == 1) "" else "s"}."
            }

            SetupOutcome.Unchanged -> "This device already has the setup ${shown.host} holds."
            SetupOutcome.HeadMoved ->
                "The setup here changed while this was open, so nothing from ${shown.host} was applied. Fetch it again to compare."

            is SetupOutcome.Invalid -> "The setup from ${shown.host} was refused. ${outcome.reason}"
            is SetupOutcome.NotSaved -> outcome.reason
        }
    }

    /** Drops the previewed setup. Nothing was applied, and nothing is. Null when nothing was waiting. */
    fun cancel(): String? {
        val shown = preview ?: return null
        preview = null
        return "Nothing from ${shown.host} was applied."
    }
}
