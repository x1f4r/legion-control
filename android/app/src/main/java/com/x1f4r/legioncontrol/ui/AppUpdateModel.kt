package com.x1f4r.legioncontrol.ui

import android.content.Context
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.data.SecretStore
import com.x1f4r.legioncontrol.net.AppUpdates
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
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
    var failure: String? by mutableStateOf(null)
        private set
    var note: String? by mutableStateOf(null)
        private set

    val newerAppAvailable: Boolean
        get() = check is AppUpdates.Check.Available

    /** Ask GitHub once. Releases are cut by hand, so once per run of the app is the whole budget. */
    fun start() {
        if (check != null || checking) return
        scope.launch {
            // Earlier builds kept a personal access token on the phone for private repositories.
            // Nothing reads one any more, and a secret nothing uses is a secret nobody is looking
            // after, so the file goes on the first launch of a build that has stopped asking.
            runCatching { SecretStore(app).clear() }
            runCheck()
        }
    }

    /** Pull to refresh reaches this too, so one gesture re-reads the machine and the release list. */
    fun recheck() {
        if (checking) return
        scope.launch { runCheck() }
    }

    fun download() {
        val release = (check as? AppUpdates.Check.Available)?.release ?: return
        if (downloading) return
        downloading = true
        progress = 0
        failure = null
        scope.launch {
            when (val result = AppUpdates.download(app, release) { progress = it }) {
                is AppUpdates.Download.Ready -> downloaded = result.file
                // A refusal is not a download that went wrong. It is a file that is not the one the
                // signed manifest describes, and saying which check it failed is the whole point of
                // having done the check.
                is AppUpdates.Download.Refused -> failure = result.problem.sentence
                is AppUpdates.Download.Failed -> failure = result.reason
            }
            downloading = false
        }
    }

    fun discardDownload() {
        downloaded = null
        progress = -1
    }

    fun reportInstallProblem(message: String) {
        failure = message
    }

    fun close() {
        scope.cancel()
    }

    private suspend fun runCheck() {
        checking = true
        try {
            check = AppUpdates.check(repo())
        } finally {
            checking = false
        }
    }
}
