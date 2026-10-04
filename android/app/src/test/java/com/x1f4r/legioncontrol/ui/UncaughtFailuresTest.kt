package com.x1f4r.legioncontrol.ui

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The safety net under the app's long-lived scopes (A5).
 *
 * A failure nothing caught used to reach the thread from a scope with no handler, which on Android
 * ends the process. With the handler it is reported, and the scope goes on running the rest.
 */
class UncaughtFailuresTest {

    @Test
    fun `an uncaught failure is reported and the scope carries on`() = runBlocking {
        val reported = mutableListOf<Throwable>()
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined + uncaughtFailureHandler("test") { reported += it })

        scope.launch { throw IllegalArgumentException("an id that is not a token") }.join()
        var after = false
        scope.launch { after = true }.join()

        assertEquals(1, reported.size)
        assertTrue(reported.single() is IllegalArgumentException)
        assertTrue("later work still runs", after)
        assertTrue(scope.isActive)
    }
}
