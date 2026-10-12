package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.contract.ContractCorpus
import com.x1f4r.legioncontrol.data.DEFAULT_GITHUB_REPO
import com.x1f4r.legioncontrol.data.sha256Hex
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.ByteArrayInputStream
import java.io.File
import java.io.InputStream

/** What the releases endpoint's answer means, and how much of anything it sends is read. */
class AppUpdatesTest {

    @get:Rule
    val folder = TemporaryFolder()

    @Test
    fun `a repository with no releases is up to date rather than broken`() {
        // GitHub answers 404 both for a repository that has published nothing and for one it will
        // not show to an anonymous request. Neither is a fault, and neither leaves anything to
        // install, so both are the same quiet sentence rather than a failure at the bottom of a page.
        assertEquals(AppUpdates.Check.UpToDate(noReleaseYet = true), AppUpdates.outcomeOf(404))
    }

    @Test
    fun `200 is the one status the body decides`() {
        assertEquals(null, AppUpdates.outcomeOf(200))
    }

    @Test
    fun `anything else is reported with the number GitHub gave`() {
        val outcome = AppUpdates.outcomeOf(503)
        assertTrue(outcome is AppUpdates.Check.Failed)
        assertEquals("GitHub answered 503", (outcome as AppUpdates.Check.Failed).reason)
    }

    @Test
    fun `versions are compared field by field rather than as text`() {
        assertTrue(AppUpdates.isNewer("1.0.10", "1.0.9"))
        assertTrue(!AppUpdates.isNewer("1.0.9", "1.0.10"))
        assertTrue(!AppUpdates.isNewer("1.1.0", "1.1.0"))
    }

    // MARK: - The update source

    @Test
    fun `the repository grammar is the schema's, string for string`() {
        val defs = ContractCorpus.json("schemas/common.schema.json").jsonObject["\$defs"]!!.jsonObject
        assertEquals(defs["githubRepo"]!!.jsonObject["pattern"]!!.jsonPrimitive.content, UpdateSource.REPO_PATTERN)
    }

    @Test
    fun `a repository GitHub could not have is passed over for the built-in one, and said so`() {
        // The setup names the source, and any peer can edit the setup. A name outside the grammar
        // would put a path, a query or a second host into the request.
        for (bad in listOf(
            "owner/..", "owner/.", "owner/name/releases", "owner/name?per_page=1", "-owner/name",
            "owner-/name", "owner", "/name", "owner/", "${"a".repeat(40)}/name", "owner/${"n".repeat(101)}",
            "owner/name\n", "owner /name", "evil.example/name", "owner/name#x",
        )) {
            assertFalse("\"$bad\" is not a repository", UpdateSource.isRepository(bad))
            val source = UpdateSource.of(bad)
            assertEquals(DEFAULT_GITHUB_REPO, source.repository)
            assertEquals(bad, source.refused)
            assertTrue(source.note!!.contains(DEFAULT_GITHUB_REPO))
        }
        for (good in listOf("x1f4r/legion-control", "owner/name.with_dots-1", "${"a".repeat(39)}/${"n".repeat(100)}", "o/..x")) {
            assertTrue("\"$good\" is a repository", UpdateSource.isRepository(good))
            assertEquals(UpdateSource(good), UpdateSource.of(good))
        }
        assertEquals(UpdateSource(DEFAULT_GITHUB_REPO), UpdateSource.of(null))
        assertEquals(UpdateSource(DEFAULT_GITHUB_REPO), UpdateSource.of(" "))
        assertNull(UpdateSource.of(null).note)
    }

    // MARK: - Small files

    /** A stream that counts what was read from it, and fails the test when it should not be read. */
    private class Counted(private val bytes: ByteArray, private val readable: Boolean = true) : InputStream() {
        private val inner = ByteArrayInputStream(bytes)
        var read = 0L

        override fun read(): Int = throw UnsupportedOperationException()

        override fun read(b: ByteArray, off: Int, len: Int): Int {
            if (!readable) fail("a refused response is never read")
            return inner.read(b, off, len).also { if (it > 0) read += it }
        }
    }

    @Test
    fun `a manifest of 257 KiB is refused, at 256 KiB it is read`() {
        val limit = ReleaseLimits.MANIFEST_BYTES
        assertEquals(256 * 1024, limit)
        val atLimit = ByteArray(limit) { 'x'.code.toByte() }
        assertArrayEquals(atLimit, AppUpdates.readCapped(Counted(atLimit), limit, -1, "the manifest"))

        try {
            AppUpdates.readCapped(Counted(ByteArray(257 * 1024)), limit, -1, "That release's manifest")
            fail("257 KiB is more than any manifest")
        } catch (refused: ResponseTooLarge) {
            assertTrue(refused.message!!.startsWith("That release's manifest is over $limit bytes"))
        }

        // A server that keeps sending is not read to its end: reading stops within a chunk of the cap.
        val endless = Counted(ByteArray(16 * 1024 * 1024))
        try {
            AppUpdates.readCapped(endless, limit, -1, "That release's manifest")
            fail("16 MiB is more than any manifest")
        } catch (_: ResponseTooLarge) {
        }
        assertTrue("read ${endless.read} bytes", endless.read <= limit + 8 * 1024)

        // And the verifier holds a manifest to the same, whoever read it.
        val outcome = ReleaseVerification.verify(ByteArray(limit + 1), "AAAA", null)
        assertEquals(
            ManifestProblem.TooLarge(limit),
            (outcome.exceptionOrNull() as ManifestVerificationFailed).problem,
        )
    }

    @Test
    fun `a response announced over its cap is refused before a byte is read`() {
        try {
            AppUpdates.readCapped(Counted(ByteArray(10), readable = false), ReleaseLimits.SIGNATURE_BYTES, 5_000, "That release's signature")
            fail("an announced 5000 byte signature is refused")
        } catch (_: ResponseTooLarge) {
        }
        assertEquals(1024 * 1024, ReleaseLimits.RELEASE_JSON_BYTES)
        assertEquals(4 * 1024, ReleaseLimits.SIGNATURE_BYTES)
    }

    // MARK: - The artefact

    private val apk = "Legion-Control-android-arm64.apk"

    private fun manifest(vararg artifacts: ReleaseArtifact) =
        VerifiedManifest(ReleaseManifest(schema = 1, version = "1.4.0", artifacts = artifacts.toList()), ByteArray(0))

    private fun artifact(bytes: ByteArray) = ReleaseArtifact(apk, sha256Hex(bytes), bytes.size.toLong())

    private fun staging(): File = File(folder.root, "$apk.part")

    @Test
    fun `an artefact is resolved, with a signed size, before anything is fetched`() {
        val missing = ReleaseVerification.artifactToFetch(manifest(), apk)
        assertTrue((missing.exceptionOrNull() as ManifestVerificationFailed).problem is ManifestProblem.MissingArtifact)

        // No size is no point at which a download would stop. The release's own number and the
        // server's Content-Length are not signed by anybody, so neither stands in for it.
        val unsized = ReleaseVerification.artifactToFetch(manifest(ReleaseArtifact(apk, "00", 0)), apk)
        assertEquals(ManifestProblem.NoSize(apk), (unsized.exceptionOrNull() as ManifestVerificationFailed).problem)
        assertTrue(ReleaseVerification.checkArtifact(manifest(ReleaseArtifact(apk, sha256Hex(ByteArray(0)), 0)), apk, ByteArray(0)) is ManifestProblem.NoSize)

        val sized = ReleaseArtifact(apk, "00", 3)
        assertEquals(sized, ReleaseVerification.artifactToFetch(manifest(sized), apk).getOrThrow())
    }

    @Test
    fun `the signed artefact is received whole and checked`() = runBlocking {
        val bytes = ByteArray(200_000) { (it % 251).toByte() }
        val progress = mutableListOf<Int>()
        val problem = AppUpdates.receive(Counted(bytes), bytes.size.toLong(), staging(), apk, artifact(bytes)) { progress += it }
        assertNull(problem)
        assertArrayEquals(bytes, staging().readBytes())
        assertEquals(100, progress.last())
    }

    @Test
    fun `one byte past the signed size stops the download and deletes it`() = runBlocking {
        val signed = ByteArray(200_000) { 1 }
        val sent = Counted(signed + byteArrayOf(1))
        val problem = AppUpdates.receive(sent, -1, staging(), apk, artifact(signed)) {}
        assertEquals(ManifestProblem.Oversized(apk, signed.size.toLong()), problem)
        assertFalse(staging().exists())
        assertEquals("the file is never written past the signed size", true, sent.read <= signed.size + 64 * 1024)
    }

    @Test
    fun `a server announcing more than the signed size is refused before reading`() = runBlocking {
        val signed = ByteArray(10)
        val problem = AppUpdates.receive(Counted(signed, readable = false), 11, staging(), apk, artifact(signed)) {}
        assertEquals(ManifestProblem.Oversized(apk, 10), problem)
        assertFalse(staging().exists())
    }

    @Test
    fun `a short or altered artefact is refused and deleted`() = runBlocking {
        val signed = ByteArray(1_000) { it.toByte() }
        val short = AppUpdates.receive(Counted(signed.copyOf(999)), -1, staging(), apk, artifact(signed)) {}
        assertEquals(ManifestProblem.WrongSize(apk, 1_000, 999), short)
        assertFalse(staging().exists())

        val altered = signed.copyOf().also { it[500] = 0x7f }
        assertEquals(ManifestProblem.WrongHash(apk), AppUpdates.receive(Counted(altered), -1, staging(), apk, artifact(signed)) {})
        assertFalse(staging().exists())
    }

    @Test
    fun `a cancelled download stops between chunks and leaves nothing behind`() = runBlocking {
        val signed = ByteArray(1_000_000) { 3 }
        lateinit var job: Job
        val stream = object : InputStream() {
            var reads = 0
            override fun read(): Int = throw UnsupportedOperationException()
            override fun read(b: ByteArray, off: Int, len: Int): Int {
                reads += 1
                // Cancelled while the first chunk is in flight, as leaving the screen would.
                if (reads == 1) job.cancel()
                java.util.Arrays.fill(b, off, off + len, 3)
                return len
            }
        }
        job = launch { AppUpdates.receive(stream, -1, staging(), apk, artifact(signed)) {} }
        job.join()

        assertTrue(job.isCancelled)
        assertEquals("not one read after the cancellation", 1, stream.reads)
        assertFalse(staging().exists())
    }

    // MARK: - The archive

    @Test
    fun `an archive has to be this app, the release's version and a newer build`() {
        fun judge(pkg: String = "com.x1f4r.legioncontrol", version: String? = "1.4.0", build: Long = 15) =
            ApkInspection.judge("com.x1f4r.legioncontrol", 14, pkg, version, build, "1.4.0")

        assertNull(judge())
        assertTrue(judge(pkg = "com.example.other")!!.contains("com.example.other"))
        assertTrue(judge(version = "1.3.9")!!.contains("1.3.9"))
        // Android compares build numbers, not names: the same build reinstalls, a lower one is a
        // downgrade it refuses, and either would have been offered as an update.
        assertTrue(judge(build = 14)!!.contains("build 14"))
        assertTrue(judge(build = 9)!!.contains("build 9"))
    }
}
