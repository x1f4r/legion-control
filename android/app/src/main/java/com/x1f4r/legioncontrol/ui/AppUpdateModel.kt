package com.x1f4r.legioncontrol.ui

import android.content.Context
import android.os.SystemClock
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.data.SecretStore
import com.x1f4r.legioncontrol.net.AppUpdates
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import java.io.File

/**
 * Updating the app itself.
 *
 * This lives beside the activity rather than inside the composable that draws it, and that is not
 * tidiness. The three sections are pages of a pager now, so the one that owns this is thrown away
 * and rebuilt every time the user swipes past it. State held in the composable would mean a fresh
 * request to GitHub on every swipe, and a download losing its progress by being scrolled away from.
 *
 * There is no credential here and nothing to ask the user for. The releases this reads are public,
 * a repository that will not show them anonymously has nothing to offer this app, and a token kept
 * on a phone for that one case was a secret with nobody looking after it.
 */
@Stable
class AppUpdateModel(
    context: Context,
    /**
     * Where to look. Read on every check rather than once, because the configuration that names it
     * can be applied while the app is open, and the next check has to go to the new address.
     */
    private val repo: () -> String,
) {
    private val app = context.applicationContext
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)

    private val schedule = UpdateCheckSchedule()
    private var foregroundJob: Job? = null
    private var checkedRepo: String? = null
    private val repositoryEpoch = UpdateRepositoryEpoch(repo())
    private var downloadOrigin: UpdateRepositoryEpoch.Ticket? = null

    var check: AppUpdates.Check? by mutableStateOf(null)
        private set
    var checking: Boolean by mutableStateOf(false)
        private set
    var downloading: Boolean by mutableStateOf(false)
        private set
    var progress: Int by mutableStateOf(-1)
        private set
    var downloaded: File? by mutableStateOf(null)
        private set
    var downloadedVersion: String? by mutableStateOf(null)
        private set
    var failure: String? by mutableStateOf(null)
        private set
    var note: String? by mutableStateOf(null)
        private set

    val newerAppAvailable: Boolean
        get() = check is AppUpdates.Check.Available

    /** Setup changes invalidate offers immediately, including while the activity is paused. */
    fun observeRepositories(changes: Flow<String>) {
        scope.launch {
            changes.distinctUntilChanged().collect {
                if (syncRepository(it) && foregroundJob != null) recheck()
            }
        }
    }

    private fun syncRepository(repository: String = repo()): Boolean {
        if (!repositoryEpoch.change(repository)) return false
        checkedRepo = null
        check = null
        discardDownload()
        failure = null
        note = null
        return true
    }

    private fun isCurrent(ticket: UpdateRepositoryEpoch.Ticket): Boolean = repositoryEpoch.matches(ticket, repo())

    /** Check on launch and foreground return, and periodically while the app stays visible. */
    fun start() {
        syncRepository()
        if (foregroundJob != null) return
        foregroundJob = scope.launch {
            runCatching { SecretStore(app).clear() }
            if (check == null || schedule.due(repo(), SystemClock.elapsedRealtime(), UpdateCheckSchedule.FOREGROUND_INTERVAL)) runCheck()
            while (isActive) {
                delay(60_000)
                val interval = if (check is AppUpdates.Check.Failed || note != null) UpdateCheckSchedule.FOREGROUND_INTERVAL else UpdateCheckSchedule.PERIODIC_INTERVAL
                if (schedule.due(repo(), SystemClock.elapsedRealtime(), interval)) runCheck()
            }
        }
    }

    fun stop() {
        val previous = foregroundJob
        foregroundJob = null
        previous?.cancel()
    }

    /** Pull to refresh reaches this too, so one gesture re-reads the machine and the release list. */
    fun recheck() {
        syncRepository()
        if (checking || downloading) return
        scope.launch { runCheck() }
    }

    fun download() {
        syncRepository()
        val release = (check as? AppUpdates.Check.Available)?.release ?: return
        if (checkedRepo != repo() || downloading || checking) return
        val origin = repositoryEpoch.ticket()
        downloading = true
        progress = 0
        failure = null
        scope.launch {
            try {
                val result = AppUpdates.download(app, release) { if (isCurrent(origin)) progress = it }
                if (!isCurrent(origin)) {
                    if (result is AppUpdates.Download.Ready) result.file.delete()
                    syncRepository()
                    return@launch
                }
                when (result) {
                    is AppUpdates.Download.Ready -> {
                        downloaded = result.file
                        downloadedVersion = release.version
                        downloadOrigin = origin
                    }
                    is AppUpdates.Download.Refused -> failure = result.problem.sentence
                    is AppUpdates.Download.Failed -> failure = result.reason
                }
            } finally {
                downloading = false
                if (!isCurrent(origin) && foregroundJob != null) recheck()
            }
        }
    }

    /** Recheck the captured download origin in the same event as opening the Android installer. */
    fun install(onReady: (File) -> Unit) {
        syncRepository()
        val origin = downloadOrigin ?: return
        val file = downloaded ?: return
        if (!isCurrent(origin) || checkedRepo != repo()) {
            discardDownload()
            return
        }
        onReady(file)
    }

    fun discardDownload() {
        downloadOrigin = null
        downloaded = null
        downloadedVersion = null
        progress = -1
    }

    fun reportInstallProblem(message: String) {
        failure = message
    }

    fun close() {
        stop()
        scope.cancel()
    }

    private suspend fun runCheck() {
        syncRepository()
        if (checking || downloading) return
        val origin = repositoryEpoch.ticket()
        val repository = origin.repository
        val sameRepo = repository == checkedRepo
        if (!sameRepo) {
            check = null
            discardDownload()
        }
        checking = true
        try {
            val result = AppUpdates.check(repository)
            if (!isCurrent(origin)) {
                syncRepository()
                return
            }
            checkedRepo = repository
            schedule.started(repository, SystemClock.elapsedRealtime())
            note = if (result is AppUpdates.Check.Failed && check is AppUpdates.Check.Available) {
                "The latest check failed. The previously verified update is still available: ${result.reason}"
            } else null
            check = retainVerifiedUpdate(check, result, sameRepo)
            if (downloadedVersion != null && downloadedVersion != (check as? AppUpdates.Check.Available)?.release?.version) {
                discardDownload()
            }
        } finally {
            checking = false
            if (!isCurrent(origin) && foregroundJob != null) recheck()
        }
    }
}
