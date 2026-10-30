package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.data.DocumentProblem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Contract section 9.5: a device whose setup has a section 9.7 error pushes nothing until it is
 * fixed, and Replace least of all, because an agent before 3.1.0 stores whatever it is sent. Replace
 * stays off with the error to fix beside it.
 */
class ReplaceProblemTest {
    @Test
    fun `a setup without errors may replace another copy`() {
        assertNull(whyNotReplace(emptyList()))
    }

    @Test
    fun `a setup with an error replaces nothing, and says which error to fix`() {
        val errors = listOf(
            DocumentProblem("helper-unknown", "machines[0].wake.helpers[0]", "the helper names no machine"),
            DocumentProblem("unknown-site", "machines[1].site", "the site is not in sites"),
        )
        assertEquals(
            "This device's setup has an error to fix in the editor before it can replace another copy: the helper names no machine",
            whyNotReplace(errors),
        )
    }

    @Test
    fun `once the setup here has an error, the question says so rather than what an earlier Replace came to`() {
        // Replace went off and the line still said to try it again once the lock was let go.
        val held = SetupPeerSync.Replaced.NotDone(
            "The setup on Tower was not replaced yet: an update holds the lock. Try again once that has finished.",
            isError = false,
        )
        assertEquals(held.line, replaceLine(held, configError = null, problem = null))

        val problem = whyNotReplace(listOf(DocumentProblem("helper-unknown", "machines[0].wake.helpers[0]", "the helper names no machine")))
        assertEquals(problem, replaceLine(held, configError = null, problem = problem))
        // What another answer on the page came to is still said, as it was before.
        val note = "This device could not save the setup just now, so nothing changed."
        assertEquals(note, replaceLine(held, configError = note, problem = problem))
        assertEquals(problem, replaceLine(null, configError = null, problem = problem))
        assertNull(replaceLine(null, configError = null, problem = null))
    }
}
