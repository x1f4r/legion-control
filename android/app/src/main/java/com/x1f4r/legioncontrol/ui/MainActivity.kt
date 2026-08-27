package com.x1f4r.legioncontrol.ui

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.x1f4r.legioncontrol.data.DEFAULT_GITHUB_REPO
import com.x1f4r.legioncontrol.ui.theme.LegionTheme

/**
 * The whole app.
 *
 * Polling is driven from resume and pause here rather than from inside a composable, because the
 * rule is not "poll while something is drawn" but "poll while the screen is in front of you". No
 * service, no worker, no alarm: when this activity stops, every ssh round trip this app makes stops
 * with it, and the next reading happens when it is looked at again.
 *
 * The model is held by the activity, which the manifest keeps alive across rotation and a light or
 * dark switch, so turning the phone sideways does not throw away a reading and start another ssh
 * connection to get the same answer back.
 */
class MainActivity : ComponentActivity() {
    private lateinit var model: AppModel
    private lateinit var updates: AppUpdateModel

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val services = createControlServices(applicationContext)
        model = AppModel(
            services = services,
            remembered = RememberedSettings(applicationContext),
        )
        // Held here for the same reason the model is, plus one of its own: the page that draws it
        // is a page of a pager and is disposed the moment it is swiped away from. State kept inside
        // it would ask GitHub for the release list again on every swipe back.
        updates = AppUpdateModel(applicationContext) {
            services.config.value?.appUpdates?.githubRepo?.takeIf { it.isNotBlank() }
                ?: DEFAULT_GITHUB_REPO
        }
        setContent {
            LegionTheme {
                LegionApp(model, updates)
            }
        }
    }

    override fun onResume() {
        super.onResume()
        model.startPolling()
    }

    override fun onPause() {
        super.onPause()
        model.stopPolling()
    }

    override fun onDestroy() {
        super.onDestroy()
        model.close()
        updates.close()
    }
}
