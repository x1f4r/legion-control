package com.x1f4r.legioncontrol.net

import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.content.FileProvider
import com.x1f4r.legioncontrol.BuildConfig
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

/**
 * Keeping the app itself up to date, from the GitHub releases of whichever repository the
 * configuration names.
 *
 * Deliberately built on HttpURLConnection rather than pulling in an http client: two requests, no
 * streaming API worth the dependency, and the rest of this project has stayed free of them.
 *
 * Nothing here authenticates. The releases of a public repository are readable by anyone, which is
 * what this app is built on and what a fork that wants self updating has to provide; a repository
 * that will not show its releases to an anonymous request simply has none to show, and that is the
 * whole of the failure. The download uses the asset API url with `Accept: application/octet-stream`
 * rather than browser_download_url, because that url is the one the API documents.
 */
object AppUpdates {

    private const val ASSET_SUFFIX = ".apk"

    private fun releasesUrl(repo: String) = "https://api.github.com/repos/$repo/releases/latest"

    private val json = Json { ignoreUnknownKeys = true }

    data class Release(
        val version: String,
        val notes: String,
        val assetUrl: String,
        val assetName: String,
        val sizeBytes: Long,
    )

    sealed interface Check {
        /**
         * Nothing to install. [noReleaseYet] when the repository has published none at all, which
         * is a different sentence on screen and the same outcome: there is nothing to press.
         */
        data class UpToDate(val noReleaseYet: Boolean = false) : Check
        data class Available(val release: Release) : Check
        data class Failed(val reason: String) : Check
    }

    /**
     * What an HTTP status from the releases endpoint means on its own. Null when the body decides.
     *
     * A 404 is not a failure worth raising. GitHub answers it both for a repository with no
     * releases and for one it will not show to an anonymous request, and in both cases the honest
     * thing to say is that there is no release to install, not that something went wrong.
     */
    internal fun outcomeOf(responseCode: Int): Check? = when (responseCode) {
        200 -> null
        404 -> Check.UpToDate(noReleaseYet = true)
        else -> Check.Failed("GitHub answered $responseCode")
    }

    /** Ask GitHub what the newest release is, and whether it is newer than what is installed. */
    suspend fun check(repo: String): Check = withContext(Dispatchers.IO) {
        try {
            val connection = (URL(releasesUrl(repo)).openConnection() as HttpURLConnection).apply {
                setRequestProperty("Accept", "application/vnd.github+json")
                setRequestProperty("User-Agent", "legion-control")
                connectTimeout = 10_000
                readTimeout = 15_000
            }
            outcomeOf(connection.responseCode)?.let { return@withContext it }

            val body = connection.inputStream.bufferedReader().use { it.readText() }
            val root = json.parseToJsonElement(body).jsonObject
            val tag = root["tag_name"]?.jsonPrimitive?.content.orEmpty()
            val notes = root["body"]?.jsonPrimitive?.content.orEmpty()
            val asset = root["assets"]?.jsonArray
                ?.map { it.jsonObject }
                ?.firstOrNull { it["name"]?.jsonPrimitive?.content?.endsWith(ASSET_SUFFIX) == true }
                ?: return@withContext Check.Failed("that release has no apk attached")

            val latest = tag.removePrefix("v")
            if (!isNewer(latest, BuildConfig.VERSION_NAME)) return@withContext Check.UpToDate()

            Check.Available(
                Release(
                    version = latest,
                    notes = notes.lineSequence().take(6).joinToString("\n").trim(),
                    assetUrl = asset["url"]?.jsonPrimitive?.content.orEmpty(),
                    assetName = asset["name"]?.jsonPrimitive?.content.orEmpty(),
                    sizeBytes = asset["size"]?.jsonPrimitive?.content?.toLongOrNull() ?: 0L,
                )
            )
        } catch (e: Exception) {
            Check.Failed(e.message ?: e.javaClass.simpleName)
        }
    }

    /**
     * Numeric field by field, so 1.0.10 is correctly newer than 1.0.9. A string comparison gets that
     * backwards, and it is exactly the version where it would start to matter.
     */
    internal fun isNewer(candidate: String, installed: String): Boolean {
        fun parts(v: String) = v.substringBefore('-').split('.').map { it.toIntOrNull() ?: 0 }
        val a = parts(candidate)
        val b = parts(installed)
        for (i in 0 until maxOf(a.size, b.size)) {
            val x = a.getOrElse(i) { 0 }
            val y = b.getOrElse(i) { 0 }
            if (x != y) return x > y
        }
        return false
    }

    /** Download into app private storage. onProgress gets 0..100, or -1 when the size is unknown. */
    suspend fun download(
        context: Context,
        release: Release,
        onProgress: (Int) -> Unit,
    ): Result<File> = withContext(Dispatchers.IO) {
        try {
            val dir = File(context.cacheDir, "updates").apply { mkdirs() }
            // One file, reused. A cache full of old apks is not worth keeping around.
            dir.listFiles()?.forEach { it.delete() }
            val target = File(dir, release.assetName.ifBlank { "legion-control.apk" })

            val connection = (URL(release.assetUrl).openConnection() as HttpURLConnection).apply {
                setRequestProperty("Accept", "application/octet-stream")
                setRequestProperty("User-Agent", "legion-control")
                instanceFollowRedirects = true
                connectTimeout = 15_000
                readTimeout = 60_000
            }
            if (connection.responseCode != 200) {
                return@withContext Result.failure(IllegalStateException("download failed: HTTP ${connection.responseCode}"))
            }

            val total = if (release.sizeBytes > 0) release.sizeBytes else connection.contentLengthLong
            var written = 0L
            connection.inputStream.use { input ->
                target.outputStream().use { output ->
                    val buffer = ByteArray(64 * 1024)
                    while (true) {
                        val read = input.read(buffer)
                        if (read < 0) break
                        output.write(buffer, 0, read)
                        written += read
                        onProgress(if (total > 0) ((written * 100) / total).toInt() else -1)
                    }
                }
            }
            if (total > 0 && written != total) {
                target.delete()
                return@withContext Result.failure(IllegalStateException("download truncated at $written of $total bytes"))
            }
            Result.success(target)
        } catch (e: Exception) {
            Result.failure(e)
        }
    }

    /** True when Android will let us hand it an apk at all. */
    fun canInstall(context: Context): Boolean = context.packageManager.canRequestPackageInstalls()

    /** Send the user to the one setting that has to be on before an install can be offered. */
    fun installPermissionIntent(context: Context): Intent =
        Intent(android.provider.Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES)
            .setData(Uri.parse("package:${context.packageName}"))

    /**
     * Hand the apk to the system installer. It shows its own confirmation, which is the right place
     * for that decision to be made, so this does not ask again first.
     */
    fun install(context: Context, apk: File) {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.updates", apk)
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        context.startActivity(intent)
    }
}
