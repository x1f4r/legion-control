package com.x1f4r.legioncontrol.ui

import android.app.Application
import com.x1f4r.legioncontrol.agent.LegionControl
import com.x1f4r.legioncontrol.net.UpdateSource
import kotlinx.coroutines.flow.map

/**
 * What there is one of in this process: the wiring below the UI and the models above it.
 *
 * Held here rather than by the activity. An activity is recreated by Android for reasons nobody
 * chose, a font size changed or a language, and on older versions it is finished outright by Back;
 * the models used to be closed with it, which cancelled every change in flight and built a second
 * copy of every store beside the first. Nothing here is ever closed. The process ends, and the
 * records of whatever was in flight are what the next one picks up.
 *
 * Built on first use, not at process start, so a process started for the debug receiver alone never
 * builds a screen's worth of state it will not draw.
 */
class LegionApplication : Application() {
    /** Everything below the UI. The one copy of every store, so a write through one is seen by all. */
    val control: LegionControl by lazy { LegionControl(this) }

    private val services: ControlServices by lazy { createControlServices(control) }

    val model: AppModel by lazy {
        AppModel(
            context = this,
            services = services,
            remembered = RememberedSettings(this),
            // The store the wiring writes revisions from, not a second one: a name set here has to
            // be the name the next revision this device writes carries.
            bindings = control.bindings,
            notifications = NotificationSettings(this),
        )
    }

    /**
     * Updating the app itself. Held here for the same reason the model is, plus one of its own: the
     * page that draws it is a page of a pager and is disposed the moment it is swiped away from, so
     * state kept inside it would ask GitHub for the release list again on every swipe back.
     *
     * The repository this device approved, never the one the shared setup merely names: where this
     * app's own updates come from is a launch value (contract section 9.9), and one waiting for
     * approval leaves the approved or the built-in source in place. It is held to GitHub's grammar
     * again on the way into a URL.
     */
    val updates: AppUpdateModel by lazy {
        AppUpdateModel(this) {
            UpdateSource.of(services.launch.value.config?.appUpdates?.githubRepo)
        }.apply {
            observeRepositories(services.launch.map { gated -> UpdateSource.of(gated.config?.appUpdates?.githubRepo) })
        }
    }
}
