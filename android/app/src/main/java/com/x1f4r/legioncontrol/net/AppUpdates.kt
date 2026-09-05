package com.x1f4r.legioncontrol.net

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
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
import java.security.MessageDigest

/**
 * Keeping the app itself up to date, from the GitHub releases of whichever repository the
 * configuration names.
 *
 * A size and a version string detect a packaging mistake and prove nothing about who built the file. Android's own
 * installer will refuse an APK signed by a different key, which is a real backstop, but by then the
 * app has already downloaded an attacker's file, told the user it was an update, and handed it to
 * the system installer with a name it made up. Everything below happens before that point.
 *
 * The order is fixed and each step depends on the one before:
 *
 * 1. Read the release. Take the manifest and its detached signature by exact name.
 * 2. Verify the signature against the key pinned in [com.x1f4r.legioncontrol.data.Trust]. Nothing is
 *    read out of the manifest before this succeeds.
 * 3. Take the artefact whose name is exactly `Legion-Control-android-arm64.apk`, never the first
 *    thing ending in `.apk`.
 * 4. Download it, and check its size and sha256 against the signed manifest.
 * 5. Look inside the archive: the package name, the version name and the signing certificate all
 *    have to match the app that is running.
 * 6. Only then offer it to the system installer.
 *
 * Deliberately built on HttpURLConnection rather than pulling in an http client: three requests, no
 * streaming API worth the dependency, and the rest of this project has stayed free of them.
 */
object AppUpdates {

    private fun releasesUrl(repo: String) = "https://api.github.com/repos/$repo/releases/latest"

    private val json = Json { ignoreUnknownKeys = true }

    data class Release(
        val version: String,
        val notes: String,
        val assetUrl: String,
        val assetName: String,
        val sizeBytes: Long,
        /** The signed manifest this release was verified against. */
        val manifest: VerifiedManifest,
    ) {
        /** What the release says the agent version is, for the agent install action. */
        val agentVersion: String get() = manifest.agentVersion
    }

    sealed interface Check {
        /**
         * Nothing to install. [noReleaseYet] when the repository has published none at all, which
         * is a different sentence on screen and the same outcome: there is nothing to press.
         */
        data class UpToDate(val noReleaseYet: Boolean = false) : Check
        data class Available(val release: Release) : Check
        data class Failed(val reason: String) : Check

        /**
         * A release exists and this build cannot verify it against itself.
         *
         * A debug build is signed with the debug key and carries a different package name, so a
         * release APK is legitimately not installable over it. Saying so is better than a signature
         * error from the system installer that reads like the release is broken.
         */
        data class NotForThisBuild(val reason: String) : Check
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

    /** Ask GitHub what the newest release is, verify its manifest, and compare it to what is here. */
    suspend fun check(repo: String): Check = withContext(Dispatchers.IO) {
        if (BuildConfig.DEBUG) {
            return@withContext Check.NotForThisBuild(
                "This is a debug build. It is signed with the debug key and installs under a " +
                    "different package name, so a published release cannot replace it.",
            )
        }
        try {
            val connection = open(releasesUrl(repo), "application/vnd.github+json")
            outcomeOf(connection.responseCode)?.let { return@withContext it }

            val body = connection.inputStream.bufferedReader().use { it.readText() }
            val root = json.parseToJsonElement(body).jsonObject
            val tag = root["tag_name"]?.jsonPrimitive?.content.orEmpty()
            val notes = root["body"]?.jsonPrimitive?.content.orEmpty()
            val assets = root["assets"]?.jsonArray?.map { it.jsonObject }.orEmpty()

            fun assetUrl(name: String): String? = assets
                .firstOrNull { it["name"]?.jsonPrimitive?.content == name }
                ?.get("url")?.jsonPrimitive?.content

            fun assetSize(name: String): Long = assets
                .firstOrNull { it["name"]?.jsonPrimitive?.content == name }
                ?.get("size")?.jsonPrimitive?.content?.toLongOrNull() ?: 0L

            val latest = tag.removePrefix("v")
            if (!isNewer(latest, BuildConfig.VERSION_NAME)) return@withContext Check.UpToDate()

            val manifestBytes = assetUrl(ReleaseAssets.MANIFEST)?.let { fetch(it) }
            val signature = assetUrl(ReleaseAssets.MANIFEST_SIGNATURE)
                ?.let { fetch(it) }
                ?.toString(Charsets.UTF_8)

            val verified = ReleaseVerification.verify(manifestBytes, signature, latest)
                .getOrElse { failure ->
                    return@withContext Check.Failed(
                        (failure as? ManifestVerificationFailed)?.problem?.sentence
                            ?: failure.message
                            ?: "the release manifest could not be verified",
                    )
                }

            if (verified.artifact(ReleaseAssets.ANDROID_APK) == null) {
                return@withContext Check.Failed(
                    ManifestProblem.MissingArtifact(ReleaseAssets.ANDROID_APK).sentence,
                )
            }
            val url = assetUrl(ReleaseAssets.ANDROID_APK) ?: return@withContext Check.Failed(
                ManifestProblem.ArtifactMissingFromRelease(ReleaseAssets.ANDROID_APK).sentence,
            )

            Check.Available(
                Release(
                    version = latest,
                    notes = notes.lineSequence().take(6).joinToString("\n").trim(),
                    assetUrl = url,
                    assetName = ReleaseAssets.ANDROID_APK,
                    sizeBytes = assetSize(ReleaseAssets.ANDROID_APK),
                    manifest = verified,
                ),
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

    /** What a download ended in. A file only ever appears on success. */
    sealed interface Download {
        data class Ready(val file: File) : Download
        data class Refused(val problem: ManifestProblem) : Download
        data class Failed(val reason: String) : Download
    }

    /**
     * Download into app private storage, then check it against the signed manifest and against this
     * app's own identity. onProgress gets 0..100, or -1 when the size is unknown.
     *
     * The file is written to a staging name and only renamed into place once every check has
     * passed, so there is never a moment where a half-checked APK sits at the path the install
     * action reads. A refused download leaves nothing behind at all.
     */
    suspend fun download(
        context: Context,
        release: Release,
        onProgress: (Int) -> Unit,
    ): Download = withContext(Dispatchers.IO) {
        try {
            val dir = File(context.cacheDir, "updates").apply { mkdirs() }
            // One file, reused. A cache full of old apks is not worth keeping around.
            dir.listFiles()?.forEach { it.delete() }
            val staging = File(dir, "${release.assetName}.part")
            val target = File(dir, release.assetName)

            val connection = open(release.assetUrl, "application/octet-stream").apply {
                instanceFollowRedirects = true
                readTimeout = 60_000
            }
            if (connection.responseCode != 200) {
                return@withContext Download.Failed("download failed: HTTP ${connection.responseCode}")
            }

            val expected = release.manifest.artifact(release.assetName)?.size
                ?.takeIf { it > 0 }
                ?: release.sizeBytes.takeIf { it > 0 }
                ?: connection.contentLengthLong

            // Hashed while it is written rather than by reading the file again afterwards. It is one
            // pass over the same bytes and it means the hash is of what was received, not of what is
            // on disk a moment later.
            val digest = MessageDigest.getInstance("SHA-256")
            var written = 0L
            connection.inputStream.use { input ->
                staging.outputStream().use { output ->
                    val buffer = ByteArray(64 * 1024)
                    while (true) {
                        val read = input.read(buffer)
                        if (read < 0) break
                        output.write(buffer, 0, read)
                        digest.update(buffer, 0, read)
                        written += read
                        onProgress(if (expected > 0) ((written * 100) / expected).toInt() else -1)
                    }
                }
            }

            val artifact = release.manifest.artifact(release.assetName)
            if (artifact == null) {
                staging.delete()
                return@withContext Download.Refused(
                    ManifestProblem.MissingArtifact(release.assetName),
                )
            }
            if (artifact.size > 0 && written != artifact.size) {
                staging.delete()
                return@withContext Download.Refused(
                    ManifestProblem.WrongSize(release.assetName, artifact.size, written),
                )
            }
            val actual = digest.digest().joinToString("") { "%02x".format(it) }
            if (!actual.equals(artifact.sha256, ignoreCase = true)) {
                staging.delete()
                return@withContext Download.Refused(ManifestProblem.WrongHash(release.assetName))
            }

            // The archive itself, before it is ever named as something to install. Package, version
            // and signing certificate, all against the app that is running.
            ApkInspection.check(context, staging, expectedVersion = release.version)?.let { problem ->
                staging.delete()
                return@withContext Download.Failed(problem)
            }

            if (!staging.renameTo(target)) {
                staging.copyTo(target, overwrite = true)
                staging.delete()
            }
            Download.Ready(target)
        } catch (e: Exception) {
            Download.Failed(e.message ?: e.javaClass.simpleName)
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
     *
     * By the time anything reaches here the file has been through the signed manifest and the
     * archive inspection. The installer's own signature check is the last of four, not the first.
     */
    fun install(context: Context, apk: File) {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.updates", apk)
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        context.startActivity(intent)
    }

    private fun open(url: String, accept: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            setRequestProperty("Accept", accept)
            setRequestProperty("User-Agent", "legion-control")
            connectTimeout = 10_000
            readTimeout = 15_000
        }

    /** A small asset, whole. Used for the manifest and its signature, which are both tiny. */
    private fun fetch(url: String): ByteArray? {
        val connection = open(url, "application/octet-stream").apply {
            instanceFollowRedirects = true
        }
        if (connection.responseCode != 200) return null
        return connection.inputStream.use { it.readBytes() }
    }
}

/**
 * What is actually inside a downloaded APK.
 *
 * The signed manifest proves the bytes are the ones the release was built from. This proves the
 * bytes are an update to *this* app: the same package, a version that matches what the release
 * claims, and the same signing certificate. Android would refuse a mismatched certificate at install
 * time anyway, and finding out here means the user is told why instead of being handed an installer
 * that fails with a message about certificates.
 */
object ApkInspection {

    /** Null when the archive is a genuine update to this app, or the sentence saying why not. */
    fun check(context: Context, apk: File, expectedVersion: String): String? {
        val flags = PackageManager.GET_SIGNING_CERTIFICATES
        val archive = context.packageManager.getPackageArchiveInfo(apk.absolutePath, flags)
            ?: return "That file is not an Android package this phone can read."

        if (archive.packageName != context.packageName) {
            return "That package is ${archive.packageName} and this app is ${context.packageName}."
        }

        val version = archive.versionName
        if (version != null && version != expectedVersion) {
            return "That package says it is version $version and the release says $expectedVersion."
        }

        val installed = signingDigests(context, context.packageName)
        val offered = signingDigests(archive)
        if (installed.isEmpty() || offered.isEmpty()) {
            return "The signing certificate of that package could not be read, so it was not offered."
        }
        if (installed.intersect(offered).isEmpty()) {
            return "That package is signed by a different key than the app that is running. " +
                "Nothing was installed."
        }
        return null
    }

    private fun signingDigests(context: Context, packageName: String): Set<String> = runCatching {
        val info = context.packageManager.getPackageInfo(
            packageName,
            PackageManager.GET_SIGNING_CERTIFICATES,
        )
        signingDigests(info)
    }.getOrDefault(emptySet())

    @Suppress("DEPRECATION")
    private fun signingDigests(info: android.content.pm.PackageInfo): Set<String> {
        val signatures = info.signingInfo?.let { signing ->
            if (signing.hasMultipleSigners()) signing.apkContentsSigners else signing.signingCertificateHistory
        } ?: return emptySet()
        return signatures.orEmpty().map { signature ->
            MessageDigest.getInstance("SHA-256").digest(signature.toByteArray())
                .joinToString("") { "%02x".format(it) }
        }.toSet()
    }
}
