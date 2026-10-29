package com.x1f4r.legioncontrol.ui

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.x1f4r.legioncontrol.ui.theme.LegionTheme

/**
 * The whole app.
 *
 * Polling is driven from resume and pause here rather than from inside a composable, because the
 * rule is not "poll while something is drawn" but "poll while the screen is in front of you". No
 * worker and no alarm: when this activity stops, every question this app asks a machine stops with
 * it.
 *
 * What does not stop is a change somebody asked for. An update takes minutes and locking the phone
 * is not withdrawing the request, so those run on the model's own scope and are reconciled by their
 * operation id when the app comes back.
 *
 * The models belong to the process, in [LegionApplication], so this activity can be recreated for
 * any reason at all without a change in flight noticing. The manifest still keeps it alive across
 * rotation, a light or dark switch, a new font size, density or language, for the composition's
 * sake: what is half typed into a field is kept by the screen, not the model.
 */
class MainActivity : ComponentActivity() {
    private val legion: LegionApplication get() = application as LegionApplication

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val model = legion.model
        val updates = legion.updates
        setContent {
            LegionTheme {
                LegionApp(model, updates)
            }
        }
    }

    override fun onResume() {
        super.onResume()
        legion.model.startPolling()
        legion.updates.start()
    }

    override fun onPause() {
        super.onPause()
        legion.model.stopPolling()
        legion.updates.stop()
    }
}
