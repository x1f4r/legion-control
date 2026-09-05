package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.net.AppUpdates

/** Monotonic timing prevents repeated requests on quick activity switches or wall-clock changes. */
internal class UpdateCheckSchedule {
    private var repository: String? = null
    private var lastStarted: Long? = null

    fun due(repo: String, now: Long, interval: Long): Boolean =
        repository != repo || lastStarted?.let { now < it || now - it >= interval } != false

    fun started(repo: String, now: Long) {
        repository = repo
        lastStarted = now
    }

    companion object {
        const val FOREGROUND_INTERVAL = 15 * 60_000L
        const val PERIODIC_INTERVAL = 6 * 60 * 60_000L
    }
}

/** A transient failed check does not dismiss an update whose manifest was already verified. */
internal fun retainVerifiedUpdate(previous: AppUpdates.Check?, next: AppUpdates.Check, sameRepo: Boolean): AppUpdates.Check =
    if (sameRepo && previous is AppUpdates.Check.Available && next is AppUpdates.Check.Failed) previous else next

/** A source changed away and back is a new check, not permission to reuse work from its old epoch. */
internal class UpdateRepositoryEpoch(initialRepository: String) {
    data class Ticket(val repository: String, val generation: Long)
    @Volatile private var current = Ticket(initialRepository, 0)

    fun change(repository: String): Boolean {
        if (repository == current.repository) return false
        current = Ticket(repository, current.generation + 1)
        return true
    }

    fun ticket(): Ticket = current
    fun matches(ticket: Ticket, liveRepository: String): Boolean = ticket == current && liveRepository == ticket.repository
}
