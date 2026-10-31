package com.x1f4r.legioncontrol.agent

import android.content.Context
import com.x1f4r.legioncontrol.data.sha256Hex
import com.x1f4r.legioncontrol.net.ManifestVerificationFailed
import com.x1f4r.legioncontrol.net.ReleaseAssets
import com.x1f4r.legioncontrol.net.ReleaseVerification
import com.x1f4r.legioncontrol.net.VerifiedManifest
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/**
 * The signed control agent this build carries, ready to be put on a machine.
 *
 * The app bundles the agent tarball and its signed manifest from the release it came from, so that
 * putting contract 3 on a machine that has contract 2 is one action rather than a terminal session.
 * Nothing about it is trusted because it shipped inside the APK: the manifest is checked against the
 * pinned release key and the tarball against the manifest, here, before a byte of it is sent
 * anywhere. An APK can be repacked; a signature cannot be forged by repacking one.
 *
 * A build without the bundle is normal and expected: every build and every test works before a signed
 * bundle exists, and the install action is then explicitly unavailable rather than silently missing
 * or, far worse, quietly installing something unverified.
 */
class AgentBundle private constructor(
    /** The tarball, copied out of assets into cache so it has a path and a stream. */
    val tarball: SignedTarball,
    val manifest: VerifiedManifest,
    val version: String,
) {
    val fileName: String get() = tarball.file.name

    /** What to call it on screen. */
    fun describe(): String = "control agent $version, signed"

    companion object {
        /** Where the release build puts the three files. */
        private const val ASSET_DIR = "agent"

        private const val STAGING_SUFFIX = ".new"

        /**
         * Reads and verifies the bundle, or explains why there is not one.
         *
         * Every failure is a refusal. There is deliberately no path that returns a tarball which has
         * not been checked, because a caller holding one would eventually send it.
         */
        suspend fun load(context: Context): Result<AgentBundle> = withContext(Dispatchers.IO) {
            val assets = context.assets
            val names = runCatching { assets.list(ASSET_DIR)?.toList().orEmpty() }
                .getOrDefault(emptyList())
            if (names.isEmpty()) {
                return@withContext Result.failure(
                    AgentBundleMissing(
                        "This build carries no signed control agent bundle, so it cannot install " +
                            "one. A release build does; a development build does not.",
                    ),
                )
            }

            fun read(name: String): ByteArray? = runCatching {
                assets.open("$ASSET_DIR/$name").use { it.readBytes() }
            }.getOrNull()

            val manifestBytes = read(ReleaseAssets.AGENT_MANIFEST)
            val signature = read(ReleaseAssets.AGENT_MANIFEST_SIGNATURE)?.toString(Charsets.UTF_8)

            val verified = ReleaseVerification.verify(manifestBytes, signature, expectedVersion = null)
                .getOrElse { failure ->
                    return@withContext Result.failure(
                        AgentBundleMissing(
                            (failure as? ManifestVerificationFailed)?.problem?.sentence
                                ?: "the bundled agent manifest could not be verified",
                        ),
                    )
                }

            val agentVersion = verified.agentVersion.takeIf { it.isNotBlank() }
                ?: return@withContext Result.failure(
                    AgentBundleMissing("The bundled agent manifest does not say which agent version it is."),
                )
            val tarballName = ReleaseAssets.agentTarball(agentVersion)
            val artifact = verified.artifact(tarballName)
                ?: return@withContext Result.failure(
                    AgentBundleMissing("The bundled manifest does not list $tarballName."),
                )
            val bytes = read(tarballName)
                ?: return@withContext Result.failure(
                    AgentBundleMissing("This build's manifest lists $tarballName and the file is not there."),
                )

            if (artifact.size > 0 && bytes.size.toLong() != artifact.size) {
                return@withContext Result.failure(
                    AgentBundleMissing(
                        "The bundled $tarballName is ${bytes.size} bytes and its signed manifest " +
                            "says ${artifact.size}.",
                    ),
                )
            }
            if (!sha256Hex(bytes).equals(artifact.sha256, ignoreCase = true)) {
                return@withContext Result.failure(
                    AgentBundleMissing(
                        "The bundled $tarballName does not match the checksum in its signed manifest.",
                    ),
                )
            }

            // Written out only after every check has passed. A file on disk is a file something can
            // send, and one that failed verification should never exist as a path at all. Written
            // beside the name and renamed onto it, because every screen loads the bundle, and one
            // loading it while another uploads it must never leave a half-written file to read.
            val directory = File(context.cacheDir, "agent-bundle").apply { mkdirs() }
            val file = File(directory, tarballName)
            val staging = File.createTempFile(tarballName, STAGING_SUFFIX, directory)
            staging.writeBytes(bytes)
            if (!staging.renameTo(file)) {
                staging.delete()
                return@withContext Result.failure(
                    AgentBundleMissing("The bundled $tarballName could not be written out to be sent."),
                )
            }
            // Another version's file goes. One that another load is still writing does not.
            directory.listFiles()
                ?.filter { it.name != tarballName && !it.name.endsWith(STAGING_SUFFIX) }
                ?.forEach { it.delete() }

            // What the signature vouches for, which is what the install is checked against: the
            // signed size, or when the manifest gives none, that of the bytes the signed hash matched.
            val signed = SignedTarball(
                file = file,
                sha256 = artifact.sha256.lowercase(),
                size = artifact.size.takeIf { it > 0 } ?: bytes.size.toLong(),
                version = agentVersion,
            )
            Result.success(AgentBundle(signed, verified, agentVersion))
        }
    }
}

/** There is no usable bundle, and this is the sentence to show instead of an install action. */
class AgentBundleMissing(override val message: String) : Exception(message)

/**
 * The agent tarball on this phone, with the size and SHA-256 its signed manifest vouches for.
 *
 * [sha256] and [size] are what was verified, not whatever [file] holds when it is next read. The
 * bootstrap's install step checks the upload against them on the far side, and that check is worth
 * something only while it is tied to the signature: hashed again from the file at install time, a
 * file being rewritten underneath would be sent with its own hash and pass.
 */
data class SignedTarball(
    val file: File,
    /** Lowercase hex, as the install script compares it. */
    val sha256: String,
    val size: Long,
    /** The agent version its signed manifest names, which decides whether it would be a downgrade. */
    val version: String? = null,
)

/**
 * How to get a signed agent onto one machine, or why it cannot be done from here.
 *
 * Two paths, chosen by what an authenticated status said is already there. A machine that already
 * speaks contract 3 takes the bytes on standard input, which needs no writable temporary path and
 * works through the restricted dispatcher. A machine on 2.x has no `self-update` at all, so the
 * tarball goes over SFTP and the staged tree installs itself, which is the bootstrap the contract
 * describes and [AgentBootstrap] carries out.
 *
 * SFTP rather than `cat > path`: a redirect is shell syntax, and this app's whole quoting discipline
 * exists so that it never has to write any.
 */
sealed interface AgentDeploymentPath {
    /** `self-update --stdin`, streaming the tarball. Contract 3 and later. */
    data object Stdin : AgentDeploymentPath

    /** Prepare [AgentBootstrap.Layout.base], upload into its `incoming/`, and install from there. */
    data class UploadAndInstall(
        /** The system that answered the status, which is the one whose interpreter runs the steps. */
        val system: MachineSystem,
        val layout: AgentBootstrap.Layout,
    ) : AgentDeploymentPath

    /** Nothing is sent, and [reason] says why in a sentence for the machine page. */
    data class Unavailable(val reason: String) : AgentDeploymentPath

    companion object {
        /** Why nothing is offered until a machine has answered with an authenticated status. */
        const val READ_STATUS_FIRST = "Read an authenticated agent status first."

        /**
         * The path for a machine, from its last authenticated status and the system that gave it.
         *
         * The Mac's gate in `AgentBootstrap.swift`, row for row. The order is the rule: nothing is
         * decided before an authenticated status, a contract 3 agent installs on standard input
         * whatever key it was reached with, and the legacy bootstrap needs every one of the rows after
         * that to pass. The previous version took a contract it had not read as zero, so a machine
         * nobody had heard from yet was sent the legacy upload.
         */
        fun decide(
            abilities: AgentAbilities,
            lastStatus: AgentStatus?,
            system: MachineSystem?,
            /** The version of the signed bundle that would be sent, when it is known. */
            bundled: String? = null,
        ): AgentDeploymentPath {
            if (lastStatus?.ok != true) return Unavailable(READ_STATUS_FIRST)
            downgradeFrom(bundled, lastStatus)?.let { return Unavailable(it) }
            if ((abilities.contract ?: 0) >= Contract.REQUIRED) return Stdin
            if (!AgentBootstrap.isAuthenticatedLegacy(lastStatus)) {
                return Unavailable(
                    "This agent's status does not say it is a 1.x or 2.x agent, so nothing is " +
                        "installed over it from the phone.",
                )
            }
            if (system == null) return Unavailable("The agent did not say which of this machine's systems it is.")
            if (system.launchState == LaunchState.NOT_DRIVEN) return Unavailable("Not approved on this device.")
            if (system.grammar == RemoteShell.DISPATCH || lastStatus.agent?.restrictedSession == true) {
                return Unavailable("Needs an administrator connection.")
            }
            if (system.grammar != RemoteShell.POSIX) {
                return Unavailable("Only POSIX systems are bootstrapped from the phone.")
            }
            val layout = AgentBootstrap.layout(system.agent)
                ?: return Unavailable("The installation base cannot be derived from this command.")
            return UploadAndInstall(system, layout)
        }

        /**
         * Why installing [bundled] would take the machine backwards, or null (contract section 13,
         * "Version floor").
         *
         * Never offered below the version the machine runs, and never below the floor its agent
         * reports, which the agent would refuse anyway. A downgrade is only ever a deliberate
         * `--allow-downgrade` from an explicit confirmation on an agent that reports a floor, and
         * this app offers none: it installs the agent it carries forwards or not at all. A version
         * that does not read as three numbers decides nothing either way.
         */
        internal fun downgradeFrom(bundled: String?, status: AgentStatus): String? {
            val carried = parseVersion(bundled) ?: return null
            val running = status.version
            if (parseVersion(running)?.let { compareVersions(carried, it) < 0 } == true) {
                return "This machine runs control agent $running, newer than the $bundled this app " +
                    "carries, so nothing older is installed over it."
            }
            val floor = status.agent?.floor
            if (parseVersion(floor)?.let { compareVersions(carried, it) < 0 } == true) {
                return "The agent on this machine will not go below version $floor, and this app " +
                    "carries $bundled."
            }
            return null
        }

        /**
         * That the machine runs an older control agent than [bundled], the one this build carries,
         * as a line for beside the install action, or null. A version that does not read as three
         * numbers says nothing either way, as it does for a downgrade.
         */
        fun behind(bundled: String?, running: String?): String? {
            val carried = parseVersion(bundled) ?: return null
            val installed = parseVersion(running) ?: return null
            if (compareVersions(installed, carried) >= 0) return null
            return "Older than control agent $bundled, which this build carries."
        }

        /** `X.Y.Z` as three numbers, compared numerically as the agent compares them, or null. */
        private fun parseVersion(text: String?): List<Long>? {
            val match = text?.let { VERSION.find(it) } ?: return null
            return match.groupValues.drop(1).map { it.toLongOrNull() ?: return null }
        }

        private fun compareVersions(a: List<Long>, b: List<Long>): Int =
            a.zip(b).firstOrNull { (x, y) -> x != y }?.let { (x, y) -> x.compareTo(y) } ?: 0

        private val VERSION = Regex("^([0-9]+)\\.([0-9]+)\\.([0-9]+)")
    }
}
