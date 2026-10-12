package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.AgentFailure
import java.io.File
import java.io.InputStream

/**
 * What one remote command produced, once it had been handed to the far side.
 *
 * Evidence, not a verdict: [com.x1f4r.legioncontrol.agent.AttemptClassifier] decides what it means.
 */
data class CommandOutcome(
    /** null when the far side closed the channel without sending one, which a reboot does. */
    val exitStatus: Int?,
    val stdout: String,
    val stderr: String,
    /**
     * True when this app's own watchdog ended the command rather than the far side. Whatever was
     * printed before that is still here, because a reply that arrived in time is still a reply.
     */
    val timedOut: Boolean = false,
    /** True when stdout went past the transport's cap and the rest was discarded. */
    val stdoutTruncated: Boolean = false,
)

/**
 * The one thing the agent client needs from ssh: run a line on an address and say what happened.
 *
 * An interface so the client's walk over routes, systems and failures can be checked against a fake
 * that records every line it was handed, which is the only way to be sure a change is never sent
 * twice short of running it against a machine.
 */
interface CommandTransport {
    /**
     * Runs [command] on [endpoint] and returns what the channel produced.
     *
     * Throws an [AgentFailure] and nothing else for every way it can fail to get that far, with its
     * kind and dispatch stage: before the exec request is written that is never sent, and from that
     * moment on it is a link that may have run the command.
     *
     * @param timeoutMillis the whole of this attempt's budget, handshake included.
     * @param stdin bytes for the command's standard input, or null. The contract has commands that
     *   take their payload this way rather than as an argument, so that nothing large or quoted has
     *   to travel on a command line, which is also what makes them work through the restricted
     *   dispatcher.
     */
    suspend fun run(
        endpoint: Endpoint,
        command: String,
        timeoutMillis: Long,
        stdin: StdinSource? = null,
    ): CommandOutcome

    /** Copies a local file to [remotePath] on [endpoint]. Throws an [AgentFailure] on failure. */
    suspend fun upload(endpoint: Endpoint, source: File, remotePath: String, timeoutMillis: Long)
}

/**
 * Where a command's standard input comes from.
 *
 * Two shapes because the two callers are genuinely different: a policy patch is a few hundred bytes
 * that are already in memory, and an agent tarball is megabytes that should not be.
 */
sealed interface StdinSource {
    class Bytes(val bytes: ByteArray) : StdinSource

    /** Opened when the write starts and closed when it finishes. */
    class Streamed(val open: () -> InputStream) : StdinSource
}
