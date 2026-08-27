package com.x1f4r.legioncontrol.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** What the releases endpoint's answer means before its body has been read. */
class AppUpdatesTest {

    @Test
    fun `a repository with no releases is up to date rather than broken`() {
        // GitHub answers 404 both for a repository that has published nothing and for one it will
        // not show to an anonymous request. Neither is a fault, and neither leaves anything to
        // install, so both are the same quiet sentence rather than a failure at the bottom of a page.
        assertEquals(AppUpdates.Check.UpToDate(noReleaseYet = true), AppUpdates.outcomeOf(404))
    }

    @Test
    fun `200 is the one status the body decides`() {
        assertEquals(null, AppUpdates.outcomeOf(200))
    }

    @Test
    fun `anything else is reported with the number GitHub gave`() {
        val outcome = AppUpdates.outcomeOf(503)
        assertTrue(outcome is AppUpdates.Check.Failed)
        assertEquals("GitHub answered 503", (outcome as AppUpdates.Check.Failed).reason)
    }

    @Test
    fun `versions are compared field by field rather than as text`() {
        assertTrue(AppUpdates.isNewer("1.0.10", "1.0.9"))
        assertTrue(!AppUpdates.isNewer("1.0.9", "1.0.10"))
        assertTrue(!AppUpdates.isNewer("1.1.0", "1.1.0"))
    }
}
