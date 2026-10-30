package com.x1f4r.legioncontrol.ui

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The count that keeps the process running while a change is in flight (A1).
 *
 * What matters is when the service is asked for and when it may stop: once for the first hold
 * after none, never again while one is still held, and nothing left held once the work is over,
 * however it ended.
 */
class KeepAliveTest {

    private var asked = 0
    private val keepAlive = KeepAlive { asked++ }

    @Test
    fun `the service is asked for by the first hold after none, and only by that one`() {
        keepAlive.hold()
        keepAlive.hold()
        keepAlive.release()
        assertEquals(1, asked)
        assertEquals(1, keepAlive.holds.value)

        keepAlive.release()
        assertEquals("nothing held: the service stops itself", 0, keepAlive.holds.value)

        keepAlive.hold()
        assertEquals("held again after none: asked again, because it may have stopped", 2, asked)
    }

    @Test
    fun `a release with nothing held leaves nothing held`() {
        keepAlive.release()
        keepAlive.hold()
        assertEquals(1, keepAlive.holds.value)
        assertEquals(1, asked)
    }

    @Test
    fun `work is held while it runs and released however it ends`() = runBlocking {
        val failed = runCatching { keepAlive.whileHeld { error("a defect") } }
        assertTrue(failed.isFailure)
        assertEquals(0, keepAlive.holds.value)

        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined)
        val never = CompletableDeferred<Unit>()
        var cancelled = false
        scope.launch {
            try {
                keepAlive.whileHeld { never.await() }
            } catch (stopped: CancellationException) {
                cancelled = true
                throw stopped
            }
        }
        assertEquals("held while it waits", 1, keepAlive.holds.value)
        scope.cancel()
        assertTrue(cancelled)
        assertEquals("a cancelled wait gives its hold back", 0, keepAlive.holds.value)
    }
}
