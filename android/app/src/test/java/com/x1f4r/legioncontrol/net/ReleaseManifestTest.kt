package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.Trust
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * What has to be true before an app update is even offered.
 *
 * A size and a version string detect a packaging mistake and prove nothing about who built the file.
 * Android's installer would refuse a foreign signature eventually, but by then the app has downloaded
 * an attacker's file and told the user it was an update.
 *
 * The order is what matters, and it is checked here: signature, then manifest, then exact asset name,
 * then size, then hash. Nothing is read out of an unverified manifest.
 */
class ReleaseManifestTest {

    private fun fixture(name: String): File? = listOf(
        File("../../tests/fixtures/trust/$name"),
        File("../tests/fixtures/trust/$name"),
    ).firstOrNull { it.isFile }

    private fun verified(): VerifiedManifest? {
        val manifest = fixture("manifest.json") ?: return null
        val signature = fixture("manifest.json.sig") ?: return null
        return ReleaseVerification.verify(manifest.readBytes(), signature.readText(), null).getOrNull()
    }

    @Test
    fun `a missing manifest is a refusal, not a reason to carry on`() {
        val outcome = ReleaseVerification.verify(null, "sig", null)
        assertTrue(outcome.isFailure)
        assertEquals(
            ManifestProblem.MissingManifest,
            (outcome.exceptionOrNull() as ManifestVerificationFailed).problem,
        )
    }

    @Test
    fun `a manifest with no signature is refused`() {
        val outcome = ReleaseVerification.verify("{}".toByteArray(), null, null)
        assertEquals(
            ManifestProblem.MissingSignature,
            (outcome.exceptionOrNull() as ManifestVerificationFailed).problem,
        )
    }

    @Test
    fun `a version that does not match the tag is refused`() {
        val manifest = fixture("manifest.json")
        val signature = fixture("manifest.json.sig")
        assumeTrue("the shared trust fixture is not in this checkout", manifest != null)

        val outcome = ReleaseVerification.verify(
            manifest!!.readBytes(),
            signature!!.readText(),
            expectedVersion = "2.0.0",
        )
        val problem = (outcome.exceptionOrNull() as ManifestVerificationFailed).problem
        assertTrue(problem is ManifestProblem.VersionMismatch)
        assertTrue(problem.sentence.contains("1.3.0"))
        assertTrue(problem.sentence.contains("2.0.0"))
    }

    @Test
    fun `an artifact the manifest does not list is refused`() {
        val manifest = verified()
        assumeTrue("the shared trust fixture is not in this checkout", manifest != null)
        val problem = ReleaseVerification.checkArtifact(manifest!!, "not-listed.apk", ByteArray(3))
        assertTrue(problem is ManifestProblem.MissingArtifact)
    }

    @Test
    fun `the wrong size and the wrong hash are told apart`() {
        val manifest = verified()
        assumeTrue("the shared trust fixture is not in this checkout", manifest != null)

        val wrongSize = ReleaseVerification.checkArtifact(manifest!!, "fixture.bin", "abcd".toByteArray())
        assertTrue(wrongSize is ManifestProblem.WrongSize)
        assertTrue(wrongSize!!.sentence.contains("4 bytes"))

        val wrongHash = ReleaseVerification.checkArtifact(manifest, "fixture.bin", "abd".toByteArray())
        assertTrue(wrongHash is ManifestProblem.WrongHash)

        assertNull(ReleaseVerification.checkArtifact(manifest, "fixture.bin", "abc".toByteArray()))
    }

    @Test
    fun `the asset names are the exact basenames, never a suffix match`() {
        // "the first thing ending in .apk" is how a release ends up installing the wrong platform's
        // artefact, and it is what the previous version did.
        assertEquals("Legion-Control-android-arm64.apk", ReleaseAssets.ANDROID_APK)
        assertEquals("Legion-Control-manifest.json", ReleaseAssets.MANIFEST)
        assertEquals("Legion-Control-manifest.json.sig", ReleaseAssets.MANIFEST_SIGNATURE)
        assertEquals("legionctl-agent-3.0.0.tgz", ReleaseAssets.agentTarball("3.0.0"))
    }

    @Test
    fun `the pinned key has a fingerprint that can be checked by eye`() {
        assertEquals(64, Trust.keyFingerprint.length)
        assertTrue(Trust.keyFingerprint.all { it.isDigit() || it in 'a'..'f' })
        assertEquals(16, Trust.shortFingerprint.length)
    }
}
