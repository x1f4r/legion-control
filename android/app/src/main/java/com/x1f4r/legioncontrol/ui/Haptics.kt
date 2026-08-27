package com.x1f4r.legioncontrol.ui

import android.view.HapticFeedbackConstants
import android.view.View
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalView

/**
 * Two weights of touch, and nothing else.
 *
 * Driven from the platform constants rather than from the Compose wrapper on purpose: the two
 * feelings wanted here are already named by the system, they have been stable since API 30, and the
 * system's own tuning is what the rest of the phone feels like. Inventing a vibration pattern would
 * mean shipping something that feels foreign next to every other app.
 *
 * [tick] is for a change you caused but did not commit to: moving between sections. [confirm] is for
 * the moment something irreversible has been agreed to, and it is deliberately not fired when the
 * button is pressed, only when the question has been answered yes.
 */
@Immutable
class Haptics internal constructor(private val view: View) {

    /** Light. A section changed, a value was copied, a button took the press. */
    fun tick() {
        view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
    }

    /** Firmer. Something that interrupts the machine has just been confirmed. */
    fun confirm() {
        view.performHapticFeedback(HapticFeedbackConstants.CONFIRM)
    }
}

@Composable
fun rememberHaptics(): Haptics {
    val view = LocalView.current
    return remember(view) { Haptics(view) }
}
