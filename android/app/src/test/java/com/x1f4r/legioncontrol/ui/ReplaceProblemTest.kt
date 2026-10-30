package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.data.DocumentProblem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Contract section 9.5: a device whose setup has a section 9.7 error pushes nothing until it is
 * fixed, and Replace least of all, because an agent before 3.1.0 stores whatever it is sent.
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
}
