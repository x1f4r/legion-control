package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.net.Endpoint

/**
 * What one address did when it was asked a real question.
 *
 * The review's complaint about health was that it stopped short: "the machine did not answer" was
 * one sentence covering a machine that is asleep, a key that was refused, a host key that changed,
 * a shell with no agent behind it and a tunnel that is down. Those are five different afternoons.
 * Each one is its own status here, and each one has something different to do about it.
 */
enum class DiagnosisStatus(val label: String) {
    /** sshd answered, the key was accepted, and the control agent replied. */
    ANSWERED("answered"),

    /** sshd answered and turned the key down. The key screen is what this wants. */
    REFUSED_KEY("refused this phone's key"),

    /** The host offered a key none of the trusted ones match. A question, not a fault. */
    HOST_KEY_CHANGED("offered an untrusted host key"),

    /** A shell came up and the agent was not behind it. The installer is what this wants. */
    NO_AGENT("has no control agent"),

    /** The agent ran and printed something that is not its one JSON object. */
    UNREADABLE("answered with something unreadable"),

    /** Nothing on the other end inside the budget, with the link still up. */
    TIMED_OUT("did not answer in time"),

    /** Nothing at all: asleep, off, or a route that does not carry. */
    UNREACHABLE("did not answer"),
    ;

    val isGood: Boolean get() = this == ANSWERED

    /** Whether this is a question for the user rather than a report about the machine. */
    val needsDecision: Boolean get() = this == HOST_KEY_CHANGED || this == REFUSED_KEY
}

/** One address, one authenticated request, and how long it took. */
data class EndpointDiagnosis(
    val endpoint: Endpoint,
    val status: DiagnosisStatus,
    val detail: String?,
    val millis: Long,
) {
    val label: String get() = endpoint.label
    val address: String get() = endpoint.address
}
