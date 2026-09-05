package com.x1f4r.legioncontrol.ui

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.x1f4r.legioncontrol.data.BindingsStore
import com.x1f4r.legioncontrol.data.DEFAULT_GITHUB_REPO
import com.x1f4r.legioncontrol.ui.theme.LegionTheme

/**
 * The whole app.
 *
 * Polling is driven from resume and pause here rather than from inside a composable, because the
 * rule is not "poll while something is drawn" but "poll while the screen is in front of you". No
 * service, no worker, no alarm: when this activity stops, every question this app asks a machine
 * stops with it.
 *
 * What does not stop is a change somebody asked for. An update takes minutes and locking the phone
 * is not withdrawing the request, so those run on the model's own scope and are reconciled by their
 * operation id when the app comes back.
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
            context = applicationContext,
            services = services,
            remembered = RememberedSettings(applicationContext),
            bindings = BindingsStore(applicationContext),
            notifications = NotificationSettings(applicationContext),
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
