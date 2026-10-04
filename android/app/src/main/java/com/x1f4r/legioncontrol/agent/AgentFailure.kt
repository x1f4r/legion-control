package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.Endpoint

/**
 * How far a command got before it failed, which is the only thing that decides whether sending it
 * again is safe.
 *
 * A restart that was refused at the door did not restart anything and can be sent to the next
 * address. A restart that was accepted and then lost its connection may or may not have run, and
 * sending it again is how one press becomes two restarts. Nothing may collapse these.
 *
 * [wire] is the contract's own word for each (section 15.4), which is what the vectors compare.
 */
enum class DispatchStage(val wire: String) {
    /**
     * The far side never received the command: the socket never opened, the handshake never
     * finished, the key was refused, the line could not be written, or a shell said it could not
     * start the interpreter. Retrying cannot duplicate anything.
     */
    NOT_STARTED("never"),

    /**
     * The command was handed over and the outcome is not known. It may have completed, it may have
     * half completed, it may never have started. A mutation in this state is never retried
     * automatically; it is reconciled by looking at the machine.
     */
    AMBIGUOUS("unknown"),

    /**
     * The agent answered, and what it said could not be read. It certainly had the command, so this
     * is reconciled exactly as [AMBIGUOUS] is and never sent again.
     */
    ACKNOWLEDGED("acknowledged"),
    ;

    /** Whether the far side may have the command, which is the one question a retry asks. */
    val mayHaveRun: Boolean get() = this != NOT_STARTED
}

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

    /** Whether the command had been handed over when this failed. See [DispatchStage]. */
    open val dispatch: DispatchStage get() = DispatchStage.NOT_STARTED

    /**
     * What section 15.4 calls this failure, or null for one that is not about an attempt at all,
     * such as an agent that answered and refused.
     */
    open val kind: FailureKind? get() = null

    /**
     * Nothing answered on this address. Asleep, off, off the network, or a link that went away.
     *
     * [stage] is what tells the two apart. A connection that was refused or never completed its
     * handshake carries [DispatchStage.NOT_STARTED]; a link that dropped while a command was running
     * on it carries [DispatchStage.AMBIGUOUS], and no mutation is repeated on the strength of it.
     * [reason] says which of the contract's route-level kinds it was.
     */
    class Unreachable(
        machineName: String?,
        detail: String?,
        cause: Throwable? = null,
        private val stage: DispatchStage = DispatchStage.NOT_STARTED,
        private val reason: FailureKind =
            if (stage == DispatchStage.NOT_STARTED) FailureKind.HOST_UNREACHABLE else FailureKind.LINK_LOST,
    ) : AgentFailure(
        "${machineName?.takeIf { it.isNotBlank() } ?: "The machine"} did not answer.",
        detail,
        cause,
    ) {
        override val dispatch: DispatchStage get() = stage
        override val kind: FailureKind get() = reason
    }

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
    ) {
        override val kind: FailureKind get() = FailureKind.AUTHENTICATION_FAILED
    }

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
        /** True when nothing has ever been trusted for this address, so nothing has changed. */
        val isFirstContact: Boolean = false,
        /** Whether another configured system still has an unused trust slot at this address. */
        val canApprove: Boolean = true,
        cause: Throwable? = null,
    ) : AgentFailure(
        if (isFirstContact) {
            "This phone has not been told which host key $address should have."
        } else if (canApprove) {
            "The host key for $address is not one this app trusts."
        } else {
            "The host key for $address is unknown, and every locally approved operating system already has keys."
        },
        if (isFirstContact) {
            "It offered $offeredFingerprint. Nothing is trusted for this address yet, and this app " +
                "does not accept the first key it is shown: compare that fingerprint against " +
                "`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the machine itself before " +
                "trusting it."
        } else if (canApprove) {
            "Trusted ${trustedFingerprints.joinToString(", ")}, offered $offeredFingerprint. " +
                "Every system on a machine answers on the same LAN address with its own host key, " +
                "so each of them raises this once. Trusting the new key keeps the ones already " +
                "trusted, and nothing already trusted is ever removed."
        } else {
            "Trusted ${trustedFingerprints.joinToString(", ")}, offered $offeredFingerprint. " +
                "No existing key was removed. If a system was reinstalled or its host key was " +
                "rotated, review host trust separately and compare both the old and new fingerprints. " +
                "Shared setup edits cannot authorize another identity."
        },
        cause,
    ) {
        override val kind: FailureKind
            get() = if (isFirstContact) FailureKind.HOST_KEY_UNKNOWN else FailureKind.HOST_KEY_CHANGED
    }

    /**
     * The link stayed up and the command never finished inside its own budget.
     *
     * Kept apart from [Unreachable] because the two want opposite things from the caller. An address
     * that stopped answering is a reason to try the next address; a command that ran too long is not,
     * and trying the rest would only spend the same budget again on each of them.
     *
     * Always ambiguous. The command was running when we stopped listening, and the far side has no
     * idea we left.
     */
    class TimedOut(val seconds: Int, detail: String?) : AgentFailure(
        "The command took longer than $seconds seconds and was given up on.",
        detail,
    ) {
        override val dispatch: DispatchStage get() = DispatchStage.AMBIGUOUS
        override val kind: FailureKind get() = FailureKind.TIMED_OUT
    }

    /** Bootstrap layouts were exhausted; installation state is still unknown. */
    class DiscoveryFailed(detail: String?) : AgentFailure(
        "Could not locate or start the control agent on that machine.",
        detail,
    )

    /**
     * A shell came up, and the interpreter or the agent script was not on the far side.
     *
     * Only a shell's own exit status (126, 127 or 9009 with nothing printed) proves the command never
     * started, and only then is [stage] [DispatchStage.NOT_STARTED]. A "Cannot find module" on stderr
     * is a sentence worth showing and nothing more: a program that printed it may have done half of
     * its work first, so it is [DispatchStage.AMBIGUOUS] and a change is never sent again on the
     * strength of it.
     */
    class AgentMissing(
        val system: MachineSystem?,
        detail: String?,
        private val stage: DispatchStage = DispatchStage.NOT_STARTED,
        private val reason: FailureKind = FailureKind.INTERPRETER_MISSING,
    ) : AgentFailure(
        if (system == null) {
            "The control agent is not installed on that machine."
        } else {
            "The control agent is not installed on ${system.name}. Run the installer on that system."
        },
        detail,
    ) {
        override val dispatch: DispatchStage get() = stage
        override val kind: FailureKind get() = reason
    }

    /**
     * The request could not be made as configured: the command cannot be written for the system's
     * shell, an argument is not one the contract allows, or the address is not one this app dials.
     *
     * A fault in the configuration rather than a fact about the machine, and nothing was sent, so it
     * is safe by construction. It gets its own case because the fix is a field in the document and
     * saying "the machine did not answer" would send the user to the wrong place entirely.
     */
    class BadRequest(summary: String, detail: String?) : AgentFailure(summary, detail) {
        override val kind: FailureKind get() = FailureKind.BAD_REQUEST
    }

    /**
     * The agent ran and printed something that is not the one JSON object it promises.
     *
     * Ambiguous by definition: something ran. A mutation that ends here is reconciled against the
     * machine rather than sent again. [stage] is [DispatchStage.ACKNOWLEDGED] when the output was a
     * reply that could not be decoded, which is the agent having said something.
     */
    class BadOutput(
        detail: String?,
        cause: Throwable? = null,
        private val stage: DispatchStage = DispatchStage.AMBIGUOUS,
    ) : AgentFailure(
        "The agent replied with something unreadable.",
        detail,
        cause,
    ) {
        override val dispatch: DispatchStage get() = stage
        override val kind: FailureKind get() = FailureKind.UNREADABLE_OUTPUT
    }

    /** The agent answered cleanly, and what it said was that it had failed. */
    class Reported(detail: String?) : AgentFailure(
        "The control agent reported a problem.",
        detail,
    )
}
