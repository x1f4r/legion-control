package com.x1f4r.legioncontrol.contract

import java.io.File

/**
 * The shared release-signing fixtures: `tests/fixtures/trust/` at the root of the repository, signed
 * by the real pinned key.
 *
 * Found through the `legion.trust` system property that the Gradle test task sets, for the reason
 * [ContractCorpus] is found through its own. These tests used to guess at the working directory and
 * skip when the guess was wrong, which is a pass in exactly the checkout where the signature check
 * was meant to catch something.
 */
object TrustFixtures {
    private val root: File by lazy {
        val path = System.getProperty(PROPERTY)
            ?: throw AssertionError(
                "$PROPERTY is not set, so the shared trust fixtures cannot be found. Run the tests " +
                    "through Gradle, which points it at the repository's tests/fixtures/trust.",
            )
        File(path).also {
            if (!it.isDirectory) throw AssertionError("$PROPERTY points at $it, which is not a directory.")
        }
    }

    /** A fixture by its name. Missing is a failure. */
    fun file(name: String): File = root.resolve(name).also {
        if (!it.isFile) throw AssertionError("tests/fixtures/trust/$name is missing; the Android tests need it.")
    }

    private const val PROPERTY = "legion.trust"
}
