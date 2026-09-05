package com.x1f4r.legioncontrol.data

import com.x1f4r.legioncontrol.net.ManifestVerificationFailed
import com.x1f4r.legioncontrol.net.ReleaseVerification
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File

/**
 * The release signature, against the shared golden fixture.
 *
 * This test exists to catch one specific family of mistakes that nothing else would: an SPKI blob
 * read as a raw key, base64 decoded with the wrong alphabet, a signature verified over the wrong
 * bytes. Each of those produces code that looks right, passes every test written against itself, and
 * rejects every real release. The fixture is signed by the actual pinned key, so agreeing with it is
 * agreeing with the other three clients and with the release script.
 */
class TrustTest {

    private fun fixture(name: String): File? = listOf(
        File("../../tests/fixtures/trust/$name"),
        File("../tests/fixtures/trust/$name"),
        File("tests/fixtures/trust/$name"),
    ).firstOrNull { it.isFile }

    @Test
    fun `the golden manifest verifies against the pinned release key`() {
        val manifest = fixture("manifest.json")
        val signature = fixture("manifest.json.sig")
        assumeTrue("the shared trust fixture is not in this checkout", manifest != null && signature != null)

        assertTrue(
            "the signed fixture has to verify with the production verifier",
            Trust.verifyDetachedSignature(manifest!!.readBytes(), signature!!.readText()),
        )
    }

    @Test
    fun `one changed byte in the manifest is refused`() {
        val manifest = fixture("manifest.json")
        val signature = fixture("manifest.json.sig")
        assumeTrue("the shared trust fixture is not in this checkout", manifest != null && signature != null)

        val bytes = manifest!!.readBytes()
        // The size field, moved by one. A tampered manifest that still parses is the interesting
        // case: a corrupt one would be caught by the JSON reader instead of by the signature.
        val tampered = String(bytes, Charsets.UTF_8).replace("\"size\": 3", "\"size\": 4")
        assertTrue("the tamper has to actually change the bytes", tampered != String(bytes, Charsets.UTF_8))

        assertFalse(
            "a manifest that does not match its signature is refused",
            Trust.verifyDetachedSignature(tampered.toByteArray(Charsets.UTF_8), signature!!.readText()),
        )
    }

    @Test
    fun `a signature that is not base64, is empty, or is the wrong length is refused`() {
        val manifest = fixture("manifest.json") ?: return
        val bytes = manifest.readBytes()
        assertFalse(Trust.verifyDetachedSignature(bytes, ""))
        assertFalse(Trust.verifyDetachedSignature(bytes, "not base64 at all !!!"))
        assertFalse(Trust.verifyDetachedSignature(bytes, "AAAA"))
    }

    @Test
    fun `verification reads the manifest only after the signature holds`() {
        // An unsigned manifest is attacker-controlled text. Nothing may be read out of it, and in
        // particular the version inside it must never be believed before the signature is checked.
        val outcome = ReleaseVerification.verify(
            manifestBytes = """{"schema":1,"version":"9.9.9","agentVersion":"9.9.9","artifacts":[]}"""
                .toByteArray(Charsets.UTF_8),
            signature = "AAAA",
            expectedVersion = "9.9.9",
        )
        assertTrue(outcome.isFailure)
        val problem = (outcome.exceptionOrNull() as ManifestVerificationFailed).problem
        assertTrue(problem.sentence.contains("signature", ignoreCase = true))
    }

    @Test
    fun `the golden manifest describes its one artifact`() {
        val manifest = fixture("manifest.json")
        val signature = fixture("manifest.json.sig")
        assumeTrue("the shared trust fixture is not in this checkout", manifest != null && signature != null)

        val verified = ReleaseVerification.verify(
            manifestBytes = manifest!!.readBytes(),
            signature = signature!!.readText(),
            expectedVersion = null,
        ).getOrThrow()

        val artifact = verified.artifact("fixture.bin")
        assertEquals(3L, artifact?.size)

        // "abc", which is the fixture's own content, and its well known sha256.
        val problem = ReleaseVerification.checkArtifact(verified, "fixture.bin", "abc".toByteArray())
        assertEquals(null, problem)

        // One byte different is a different artifact, whatever the manifest says.
        assertTrue(
            ReleaseVerification.checkArtifact(verified, "fixture.bin", "abd".toByteArray()) != null,
        )
    }
}
