package com.x1f4r.legioncontrol.data

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/**
 * One small secret, kept the same way the ssh private key is kept.
 *
 * The GitHub token that lets this app read the releases of a private repository is exactly as
 * sensitive as the key next door, so it goes through the same door: app private storage, wrapped by
 * an AES key that lives in the Android keystore and never leaves it. A copy of the file lifted off
 * the phone, or carried to another device by a backup, is not usable.
 *
 * Everything here is suspending because both halves are real work: file IO, and a keystore round
 * trip that on a cold start has to unlock a hardware backed key. Neither belongs on the main thread.
 */
class SecretStore(context: Context) {
    private val file = File(File(context.applicationContext.filesDir, "secrets"), "github-token.bin")

    /**
     * The stored token, or null when there is none.
     *
     * A file that will not decrypt reads as null rather than throwing, for the same reason the ssh
     * key does: the wrapping key is gone after a restore onto another phone, the secret is
     * unrecoverable either way, and the only useful answer is the one the user can act on, which is
     * "there is no token, paste one".
     */
    suspend fun read(): String? = withContext(Dispatchers.IO) {
        runCatching { KeyVault.read(file)?.toString(Charsets.UTF_8) }
            .getOrNull()
            ?.takeIf { it.isNotBlank() }
    }

    suspend fun write(secret: String) = withContext(Dispatchers.IO) {
        KeyVault.write(file, secret.toByteArray(Charsets.UTF_8))
    }

    suspend fun clear() = withContext(Dispatchers.IO) {
        file.delete()
        // The staging file KeyVault writes beside the target, in case a write died halfway through
        // and left one behind. Clearing the token has to clear every copy of it.
        File(file.parentFile, "${file.name}.new").delete()
        Unit
    }
}
