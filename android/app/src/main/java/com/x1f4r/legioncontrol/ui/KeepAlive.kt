package com.x1f4r.legioncontrol.ui

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.getAndUpdate
import kotlinx.coroutines.flow.update

/**
 * How many changes somebody asked for are being sent or followed in this process right now.
 *
 * Counted because that is what decides whether Android may freeze the process. A phone that is
 * locked with nothing in front of the system soon stops running this app's coroutines at all, and an
 * update whose reply was on its way, or whose `op --wait` watch had twenty minutes left, simply
 * stopped where it was until the screen came back. Locking the phone is not withdrawing the request,
 * so while anything is held here [ChangeService] keeps the process in the foreground, and once
 * nothing is it lets it go.
 *
 * The count is the whole interface. Work holds it for as long as it runs and releases it however it
 * ends; the first hold after none is the moment to ask for the service, and the service stops itself
 * when it reads nothing held. Nothing here knows what the work is, which is why a send, a watch and
 * a whole wake can share it.
 */
class KeepAlive(
    /**
     * Called when the first hold is taken after none. It asks Android to keep the process running,
     * and may be refused: the work still runs, exactly as it did before there was a service at all.
     */
    private val wanted: () -> Unit = {},
) {
    private val count = MutableStateFlow(0)

    /** How many holds there are now. */
    val holds: StateFlow<Int> = count.asStateFlow()

    fun hold() {
        if (count.getAndUpdate { it + 1 } == 0) wanted()
    }

    /** Never below nothing: a release with no hold left to release is a defect, not a debt. */
    fun release() {
        count.update { (it - 1).coerceAtLeast(0) }
    }

    /** Runs [work] held, and releases the hold however it ends, a cancellation included. */
    suspend fun <T> whileHeld(work: suspend () -> T): T {
        hold()
        try {
            return work()
        } finally {
            release()
        }
    }
}
