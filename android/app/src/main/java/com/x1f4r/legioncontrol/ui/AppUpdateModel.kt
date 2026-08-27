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
 * Updating the app itself, and the one credential that makes it possible.
 *
 * This lives beside the activity rather than inside the composable that draws it, and that is not
 * tidiness. The three sections are pages of a pager now, so the one that owns this is thrown away
 * and rebuilt every time the user swipes past it. State held in the composable would mean a fresh
 * request to GitHub on every swipe, and a download losing its progress by being scrolled away from.
 *
 * The token is a plain field on purpose. Everything else here is snapshot state that Compose reads;
 * the token is never read by the UI, never drawn, never put in a status line and never logged. What
 * the screen is allowed to know is whether one exists.
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
    private val secrets = SecretStore(app)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)

    /** Never observed by the UI, and never leaves this object except as an Authorization header. */
    private var token: String? = null

    var check: AppUpdates.Check? by mutableStateOf(null)
        private set
    var checking: Boolean by mutableStateOf(false)
        private set
    var hasToken: Boolean by mutableStateOf(false)
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

    /**
     * What is being typed into the token field.
     *
     * Deliberately here rather than in a `rememberSaveable`, because a saveable would put a personal
     * access token into the activity's saved instance state, which is written to disk by the system
     * and read back by anything that can read it. It survives a swipe, which is all it has to do.
     */
    var draft: String by mutableStateOf("")

    val newerAppAvailable: Boolean
        get() = check is AppUpdates.Check.Available

    /** Read the token once, then ask GitHub once. Releases are cut by hand, so once is enough. */
    fun start() {
        if (check != null || checking) return
        scope.launch {
            token = secrets.read()
            hasToken = token != null
            runCheck()
        }
    }

    /** Pull to refresh reaches this too, so one gesture re-reads the machine and the release list. */
    fun recheck() {
        if (checking) return
        scope.launch { runCheck() }
    }

    fun saveToken() {
        val value = draft.trim()
        if (value.isEmpty()) return
        scope.launch {
            failure = null
            // The message is written by hand rather than taken from the exception, because an
            // exception thrown while handling a secret is exactly the kind of thing that quotes it.
            runCatching { secrets.write(value) }.fold(
                onSuccess = {
                    token = value
                    draft = ""
                    hasToken = true
                    note = "The token is saved on this phone."
                    runCheck()
                },
                onFailure = {
                    note = null
                    failure = "The token could not be saved on this phone."
                },
            )
        }
    }

    fun clearToken() {
        scope.launch {
            runCatching { secrets.clear() }
            token = null
            draft = ""
            hasToken = false
            failure = null
            note = "The token was removed from this phone."
            runCheck()
        }
    }

    fun download() {
        val release = (check as? AppUpdates.Check.Available)?.release ?: return
        if (downloading) return
        downloading = true
        progress = 0
        failure = null
        scope.launch {
            val result = AppUpdates.download(app, release, token) { progress = it }
            downloading = false
            result.fold(
                onSuccess = { downloaded = it },
                onFailure = { failure = it.message ?: "the download failed" },
            )
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
            check = AppUpdates.check(repo(), token)
        } finally {
            checking = false
        }
    }
}
