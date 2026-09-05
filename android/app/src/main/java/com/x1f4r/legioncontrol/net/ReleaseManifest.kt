package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.data.Trust
import com.x1f4r.legioncontrol.data.sha256Hex
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/**
 * The signed list of what a release contains.
 *
 * The shape and the signature are fixed exactly, and nothing here is allowed to be generous about
 * either. The schema is `{schema, version, agentVersion, artifacts[]}` and the
 * detached `.sig` is base64 of the raw 64 signature bytes followed by one newline, computed over the
 * exact JSON bytes as they were downloaded.
 *
 * "As they were downloaded" is the part worth being careful about: the signature covers bytes, not a
 * parsed object, so the bytes are kept and verified before anything is decoded from them. Parsing
 * first and re-serialising would produce a different byte string and a signature that never matches,
 * or worse, one that matches something other than what was signed.
 */
@Serializable
data class ReleaseManifest(
    val schema: Int = 0,
    val version: String = "",
    val agentVersion: String = "",
    val artifacts: List<ReleaseArtifact> = emptyList(),
) {
    fun artifact(name: String): ReleaseArtifact? = artifacts.firstOrNull { it.name == name }
}

@Serializable
data class ReleaseArtifact(
    /** The exact basename of the asset. Matched by equality; never by suffix, never "the first". */
    val name: String = "",
    /** Lowercase hex. */
    val sha256: String = "",
    val size: Long = 0,
)

/** The exact asset basenames. Selecting by suffix is how a release ends up installing the wrong OS. */
object ReleaseAssets {
    const val MANIFEST = "Legion-Control-manifest.json"
    const val MANIFEST_SIGNATURE = "Legion-Control-manifest.json.sig"

    /** This app's own artefact. arm64 only, which is what the build produces. */
    const val ANDROID_APK = "Legion-Control-android-arm64.apk"

    const val AGENT_MANIFEST = "Legion-Control-agent-manifest.json"
    const val AGENT_MANIFEST_SIGNATURE = "Legion-Control-agent-manifest.json.sig"

    fun agentTarball(version: String) = "legionctl-agent-$version.tgz"
}

/** Why a signed manifest was not accepted. Every one of these refuses the artefact. */
sealed interface ManifestProblem {
    val sentence: String

    data object MissingManifest : ManifestProblem {
        override val sentence =
            "That release has no signed manifest, so there is no way to tell what its files should be."
    }

    data object MissingSignature : ManifestProblem {
        override val sentence = "That release has a manifest and no signature over it."
    }

    data object BadSignature : ManifestProblem {
        override val sentence =
            "The signature on that release was not made by the key this app trusts. Nothing was installed."
    }

    data class Unreadable(val detail: String) : ManifestProblem {
        override val sentence = "That release's manifest could not be read: $detail"
    }

    data class WrongSchema(val schema: Int) : ManifestProblem {
        override val sentence =
            "That release's manifest is schema $schema and this app understands schema 1."
    }

    data class VersionMismatch(val manifest: String, val tag: String) : ManifestProblem {
        override val sentence =
            "That release is tagged $tag and its manifest says $manifest. They have to agree."
    }

    data class MissingArtifact(val name: String) : ManifestProblem {
        override val sentence = "That release's manifest does not list $name."
    }

    data class ArtifactMissingFromRelease(val name: String) : ManifestProblem {
        override val sentence = "That release's manifest lists $name and the release does not have it."
    }

    data class WrongSize(val name: String, val expected: Long, val actual: Long) : ManifestProblem {
        override val sentence =
            "$name is $actual bytes and the signed manifest says $expected. Nothing was installed."
    }

    data class WrongHash(val name: String) : ManifestProblem {
        override val sentence =
            "$name does not match the checksum in the signed manifest. Nothing was installed."
    }
}

/** A manifest that has been checked against the pinned key. The only way to get one. */
class VerifiedManifest internal constructor(
    val manifest: ReleaseManifest,
    /** The exact bytes the signature was checked over. */
    val bytes: ByteArray,
) {
    val version: String get() = manifest.version
    val agentVersion: String get() = manifest.agentVersion

    fun artifact(name: String): ReleaseArtifact? = manifest.artifact(name)
}

object ReleaseVerification {

    private val json = Json { ignoreUnknownKeys = true }

    /** Only schema 1 exists. A later one is refused rather than guessed at. */
    private const val SUPPORTED_SCHEMA = 1

    /**
     * Checks the signature first and reads the manifest second.
     *
     * That order is the whole point. A manifest that has not been verified is attacker-controlled
     * text, and every field read out of it before the signature check is a decision made on the
     * attacker's say-so.
     */
    fun verify(
        manifestBytes: ByteArray?,
        signature: String?,
        expectedVersion: String?,
    ): Result<VerifiedManifest> {
        if (manifestBytes == null) return failure(ManifestProblem.MissingManifest)
        if (signature.isNullOrBlank()) return failure(ManifestProblem.MissingSignature)
        if (!Trust.verifyDetachedSignature(manifestBytes, signature)) {
            return failure(ManifestProblem.BadSignature)
        }

        val parsed = try {
            json.decodeFromString(ReleaseManifest.serializer(), String(manifestBytes, Charsets.UTF_8))
        } catch (failure: Exception) {
            return failure(
                ManifestProblem.Unreadable(failure.message ?: failure::class.java.simpleName),
            )
        }

        if (parsed.schema != SUPPORTED_SCHEMA) return failure(ManifestProblem.WrongSchema(parsed.schema))
        if (expectedVersion != null && parsed.version != expectedVersion) {
            return failure(ManifestProblem.VersionMismatch(parsed.version, expectedVersion))
        }
        return Result.success(VerifiedManifest(parsed, manifestBytes))
    }

    /**
     * Whether these bytes are the artefact the signed manifest describes.
     *
     * Size before hash, because a size that is wrong is cheap to notice and says something more
     * specific on screen than "the checksum did not match".
     */
    fun checkArtifact(
        manifest: VerifiedManifest,
        name: String,
        bytes: ByteArray,
    ): ManifestProblem? {
        val artifact = manifest.artifact(name) ?: return ManifestProblem.MissingArtifact(name)
        if (artifact.size > 0 && bytes.size.toLong() != artifact.size) {
            return ManifestProblem.WrongSize(name, artifact.size, bytes.size.toLong())
        }
        if (!sha256Hex(bytes).equals(artifact.sha256, ignoreCase = true)) {
            return ManifestProblem.WrongHash(name)
        }
        return null
    }

    private fun failure(problem: ManifestProblem): Result<VerifiedManifest> =
        Result.failure(ManifestVerificationFailed(problem))
}

/** Carried as an exception so a Result can hold it, with the sentence already written. */
class ManifestVerificationFailed(val problem: ManifestProblem) : Exception(problem.sentence)
