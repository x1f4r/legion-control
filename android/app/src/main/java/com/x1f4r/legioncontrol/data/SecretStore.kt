package com.x1f4r.legioncontrol.data

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/**
 * The GitHub token this app used to keep, and now only knows how to forget.
 *
 * Earlier builds asked for a personal access token so that a private repository would show its
 * releases, and kept it beside the ssh key: app private storage, wrapped by a key in the Android
 * keystore. Nothing asks for one any more, and a secret that is no longer used is a secret nobody
 * is looking after, so the file is deleted the first time a build without the token runs.
 *
 * Suspending because a file delete is file IO, and this happens on the way into a screen.
 */
class SecretStore(context: Context) {
    private val file = File(File(context.applicationContext.filesDir, "secrets"), "github-token.bin")

    /** Deletes the token, if there is one left. Doing it twice costs nothing. */
    suspend fun clear() = withContext(Dispatchers.IO) {
        file.delete()
        // The staging file KeyVault writes beside the target, in case a write died halfway through
        // and left one behind. Clearing the token has to clear every copy of it.
        File(file.parentFile, "${file.name}.new").delete()
        Unit
    }
}
