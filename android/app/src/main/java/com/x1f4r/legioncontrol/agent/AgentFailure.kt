package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.Endpoint

/**
 * Why a call did not produce an answer.
 *
 * These are kept apart rather than collapsed into one error because the right thing to do about them
 * is completely different: an unauthorised key wants the public key screen, an unreachable machine
 * wants wake-on-LAN, and a missing agent wants the installer run on the far side. A single "could
 * not connect" would send the user looking in the wrong place every time.
 */
sealed class AgentFailure(
    /** One sentence, written for the person holding the phone. */
    val summary: String,
    /** The raw text underneath it: ssh output, exit codes, whatever there was. */
    val detail: String? = null,
    cause: Throwable? = null,
) : Exception(summary, cause) {

    /** Nothing answered on any address we know. Asleep, off, or off the network. */
    class Unreachable(machineName: String?, detail: String?, cause: Throwable? = null) : AgentFailure(
        "${machineName?.takeIf { it.isNotBlank() } ?: "The machine"} did not answer.",
        detail,
        cause,
    )

    /** We reached sshd and it refused the key. On a fresh install this is the normal first result. */
    class NotAuthorised(
        val endpoint: Endpoint,
        machineName: String?,
        detail: String?,
        cause: Throwable? = null,
    ) : AgentFailure(
        "This phone's key is not authorised on " +
            "${machineName?.takeIf { it.isNotBlank() } ?: "that machine"} yet.",
        detail,
        cause,
    )

    /**
     * The host presented a key that none of the ones trusted for this address match.
     *
     * Worth its own case rather than a generic failure. A machine that dual boots is the innocent
     * explanation and it is the likely one, since every system on it answers on the same LAN address
     * with its own host key, but the app must not decide that on the user's behalf.
     */
    class HostKeyChanged(
        val address: String,
        val trustedFingerprints: List<String>,
        val offeredFingerprint: String,
        /** The offered key's base64 wire encoding, so trusting it stores exactly what was seen. */
        val offeredKeyBlob: String,
        cause: Throwable? = null,
    ) : AgentFailure(
        "The host key for $address is not one this app trusts.",
        "Trusted ${trustedFingerprints.joinToString(", ")}, offered $offeredFingerprint. " +
            "Every system on a machine answers on the same LAN address with its own host key, " +
            "so each of them raises this once. Trusting the new key keeps the ones already trusted.",
        cause,
    )

    /**
     * The link stayed up and the command never finished inside its own budget.
     *
     * Kept apart from [Unreachable] because the two want opposite things from the caller. An address
     * that stopped answering is a reason to try the next address; a command that ran too long is not,
     * and trying the rest would only spend the same budget again on each of them.
     */
    class TimedOut(val seconds: Int, detail: String?) : AgentFailure(
        "The command took longer than $seconds seconds and was given up on.",
        detail,
    )

    /** A shell came up, and the interpreter or the agent script was not on the far side. */
    class AgentMissing(val system: MachineSystem?, detail: String?) : AgentFailure(
        if (system == null) {
            "The control agent is not installed on that machine."
        } else {
            "The control agent is not installed on ${system.name}. Run the installer on that system."
        },
        detail,
    )

    /** The agent ran and printed something that is not the one JSON object it promises. */
    class BadOutput(detail: String?, cause: Throwable? = null) : AgentFailure(
        "The agent replied with something unreadable.",
        detail,
        cause,
    )

    /** The agent answered cleanly, and what it said was that it had failed. */
    class Reported(detail: String?) : AgentFailure(
        "The control agent reported a problem.",
        detail,
    )
}
