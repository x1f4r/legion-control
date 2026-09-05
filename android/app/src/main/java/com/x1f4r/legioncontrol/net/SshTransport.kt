package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.data.DeviceIdentity
import com.x1f4r.legioncontrol.data.HostKeyStore
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import net.schmizz.keepalive.KeepAliveProvider
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.common.SecurityUtils
import net.schmizz.sshj.connection.channel.direct.Session
import net.schmizz.sshj.userauth.UserAuthException
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** What one remote command produced. */
data class CommandOutcome(
    /** null when the far side closed the channel without sending one, which a reboot does. */
    val exitStatus: Int?,
    val stdout: String,
    val stderr: String,
)

/**
 * One ssh connection per command: connect, run one thing, read both streams, hang up.
 *
 * No pooling and no persistent session on purpose. The commands are seconds apart at best and
 * minutes apart normally, a phone loses its network constantly, and half of what this app runs
 * (boot, restart) deliberately kills the connection it arrived on. A pool would spend most of its
 * life holding dead sockets and would have to detect that anyway.
 */
class SshTransport(
    private val identity: DeviceIdentity,
    private val hostKeys: HostKeyStore,
    /** Only ever used to write a sentence: which machine it was that did not answer. */
    private val machineName: String,
) {
    /**
     * Runs [command] on [endpoint] and returns what it printed.
     *
     * Throws an [AgentFailure] and nothing else: every ssh level failure is translated here, because
     * this is the only place that knows the difference between a refused key and a refused socket.
     */
    suspend fun run(
        endpoint: Endpoint,
        command: String,
        timeoutMillis: Long,
        /**
         * Bytes to write to the command's standard input, or null.
         *
         * The contract has two commands that take their payload this way rather than as an
         * argument: `policy set`, which reads a JSON patch, and `self-update --stdin`, which reads a
         * whole signed tarball. Both exist so that nothing large or quoted has to travel on a
         * command line, which is also what makes them work through the restricted dispatcher.
         */
        stdin: StdinSource? = null,
    ): CommandOutcome = coroutineScope {
        val config = DefaultConfig().apply {
            // The long commands (update takes minutes) sit silent on a phone's connection, which is
            // exactly the shape of traffic a mobile NAT drops. A heartbeat keeps the mapping alive.
            keepAliveProvider = KeepAliveProvider.KEEP_ALIVE
        }

        val verifier = PinnedHostKeyVerifier(hostKeys, endpoint.address)
        val client = SSHClient(config)

        // Flipped the instant the command is handed to a channel, and read by every failure path
        // below. It is the difference between "the far side never heard this" and "the far side may
        // have run it", and a mutation is only ever repeated on the first of those.
        val dispatched = AtomicBoolean(false)

        // The whole blocking conversation runs as a child, and this coroutine does nothing but wait
        // on it. That split is what makes cancellation work at all. sshj reads from a plain socket,
        // and a blocking socket read notices neither a cancelled coroutine nor an interrupted thread;
        // the only thing that breaks it is closing the socket, and that has to be done by something
        // that is not itself blocked inside the read. Waiting on a Deferred is such a thing, because
        // await is a suspension point and gives up the moment the caller goes away.
        val work = async(Dispatchers.IO) {
            try {
                client.connectTimeout = CONNECT_TIMEOUT_MILLIS
                // sshj copies this onto the socket once, inside connect, and never looks at it again,
                // so it has to be the value the long silent middle of an update needs rather than the
                // one the handshake needs. The handshake gets its own bound from the watchdog below.
                client.timeout = timeoutMillis.coerceIn(1L, Int.MAX_VALUE.toLong()).toInt()
                client.addHostKeyVerifier(verifier)

                // A server that accepts the TCP connection and then says nothing would otherwise hold
                // the whole command timeout, which for an update is seven minutes of a button doing
                // nothing. Same trick as above, for the same reason: a separate coroutine, on a
                // separate thread, closing the socket the blocked one is stuck on.
                val connected = CompletableDeferred<Unit>()
                val watchdog = launch {
                    if (withTimeoutOrNull(HANDSHAKE_TIMEOUT_MILLIS) { connected.await() } == null) {
                        runCatching { client.close() }
                    }
                }
                try {
                    connect(client, endpoint, verifier)
                    authenticate(client, endpoint)
                } finally {
                    connected.complete(Unit)
                    watchdog.cancel()
                }

                client.connection.keepAlive.keepAliveInterval = KEEP_ALIVE_SECONDS

                exec(client, command, timeoutMillis, dispatched, stdin)
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: AgentFailure) {
                throw failure
            } catch (failure: Exception) {
                // The last word on this class's promise to throw AgentFailure and nothing else. sshj
                // throws checked exceptions out of places that are easy to miss, and the caller above
                // this one catches AgentFailure only: anything that got past here would not become a
                // sentence at the bottom of the screen, it would take the app down.
                throw unreachable(endpoint, failure, stageOf(dispatched))
            } finally {
                runCatching { client.close() }
            }
        }

        try {
            work.await()
        } catch (cancelled: CancellationException) {
            // The child is now cancelled but still parked in a read that will not return on its own.
            // Closing the client here is what lets it unwind, which it has to do before this scope is
            // allowed to finish. Without this the thread would be held until the socket timeout, and
            // for an update that is seven minutes after the screen it belonged to has gone.
            runCatching { client.close() }
            throw cancelled
        } catch (failure: AgentFailure) {
            throw failure
        } catch (failure: Exception) {
            // Nothing above this line is allowed to leak a raw ssh exception. The caller catches
            // AgentFailure and only AgentFailure, and every one of these arrives on a coroutine that
            // the screen launched, so anything else would take the whole app down rather than turn
            // into a sentence. sshj throws from more places than the ones translated by hand: opening
            // a channel, closing one, and every write on a link that has already gone.
            throw unreachable(endpoint, failure, stageOf(dispatched))
        }
    }

    private fun stageOf(dispatched: AtomicBoolean): DispatchStage =
        if (dispatched.get()) DispatchStage.AMBIGUOUS else DispatchStage.NOT_STARTED

    private suspend fun connect(client: SSHClient, endpoint: Endpoint, verifier: PinnedHostKeyVerifier) {
        try {
            client.connect(endpoint.host, endpoint.port)
        } catch (failure: CancellationException) {
            throw failure
        } catch (failure: Exception) {
            // Order matters: a host key mismatch surfaces as a transport exception like any other, so
            // the recorded mismatch has to be checked before the generic translation.
            verifier.mismatch?.let {
                throw AgentFailure.HostKeyChanged(
                    address = it.address,
                    trustedFingerprints = it.trustedFingerprints,
                    offeredFingerprint = it.offeredFingerprint,
                    offeredKeyBlob = it.offeredKeyBlob,
                    isFirstContact = it.isFirstContact,
                    canApprove = it.canApprove,
                    cause = failure,
                )
            }
            throw unreachable(endpoint, failure)
        }
    }

    private suspend fun authenticate(client: SSHClient, endpoint: Endpoint) {
        try {
            client.authPublickey(endpoint.user, identity.identity())
        } catch (failure: CancellationException) {
            throw failure
        } catch (failure: UserAuthException) {
            // sshd let us talk to it and turned the key down. On a phone that has just been set up
            // this is not an error so much as the next step, and the UI shows the key to paste.
            throw AgentFailure.NotAuthorised(
                endpoint = endpoint,
                machineName = machineName,
                detail = describe(failure),
                cause = failure,
            )
        } catch (failure: Exception) {
            throw unreachable(endpoint, failure)
        }
    }

    /**
     * Copies a local file to the far side over SFTP.
     *
     * Used for one thing: putting a signed agent tarball on a machine whose agent is too old to have
     * `self-update --stdin`. It is a file transfer, not a shell command, so there is no redirect to
     * quote and no chance of the path turning into something else on the way. sshj opens the SFTP
     * subsystem over the same authenticated connection.
     */
    suspend fun upload(
        endpoint: Endpoint,
        source: File,
        remotePath: String,
        timeoutMillis: Long,
    ): Unit = coroutineScope {
        val verifier = PinnedHostKeyVerifier(hostKeys, endpoint.address)
        val client = SSHClient(DefaultConfig())
        val work = async(Dispatchers.IO) {
            try {
                client.connectTimeout = CONNECT_TIMEOUT_MILLIS
                client.timeout = timeoutMillis.coerceIn(1L, Int.MAX_VALUE.toLong()).toInt()
                client.addHostKeyVerifier(verifier)
                connect(client, endpoint, verifier)
                authenticate(client, endpoint)
                client.newSFTPClient().use { sftp ->
                    // The agent's incoming directory does not exist on a machine that has never
                    // been sent anything, and sshj's put does not create parents.
                    remotePath.substringBeforeLast('/', "").takeIf { it.isNotEmpty() }?.let {
                        runCatching { sftp.mkdirs(it) }
                    }
                    sftp.put(source.absolutePath, remotePath)
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: AgentFailure) {
                throw failure
            } catch (failure: Exception) {
                // An upload that failed halfway leaves a partial file, and the agent refuses an
                // archive whose hash does not match, so a partial upload can only ever be refused
                // rather than installed. Reported as ambiguous all the same: bytes did leave here.
                throw unreachable(endpoint, failure, DispatchStage.AMBIGUOUS)
            } finally {
                runCatching { client.close() }
            }
        }
        try {
            work.await()
        } catch (cancelled: CancellationException) {
            runCatching { client.close() }
            throw cancelled
        } catch (failure: AgentFailure) {
            throw failure
        } catch (failure: Exception) {
            throw unreachable(endpoint, failure, DispatchStage.AMBIGUOUS)
        }
    }

    private suspend fun exec(
        client: SSHClient,
        command: String,
        timeoutMillis: Long,
        dispatched: AtomicBoolean,
        stdin: StdinSource?,
    ): CommandOutcome = coroutineScope {
        val session: Session = try {
            client.startSession()
        } catch (failure: CancellationException) {
            throw failure
        } catch (failure: Exception) {
            // The key was accepted and the channel was not opened, so the command was never handed
            // over. Reported as unreachable rather than as unreadable output, because "unreadable"
            // is the ambiguous case and this one is not: nothing ran.
            throw AgentFailure.Unreachable(
                machineName,
                "$machineName accepted the key and then refused a session. ${describe(failure)}",
                failure,
                DispatchStage.NOT_STARTED,
            )
        }

        // The command's own wall clock, and the only thing that actually bounds it. The socket read
        // timeout cannot: the keep alive above asks for a reply every twenty seconds, so a link that
        // is up but carrying a command that never finishes stays busy forever, and the read the two
        // drains below are parked in never returns. Same shape as the handshake watchdog, for the
        // same reason, because a blocking read only ends when someone else closes the socket.
        val finished = CompletableDeferred<Unit>()
        val timedOut = AtomicBoolean(false)
        val watchdog = launch {
            if (withTimeoutOrNull(timeoutMillis) { finished.await() } == null) {
                timedOut.set(true)
                runCatching { client.close() }
            }
        }

        val outcome = try {
            val process = session.exec(command)
            // From here on the far side has the command. Nothing below may be reported as "never
            // sent", however it ends.
            dispatched.set(true)
            // Both streams get drained at the same time. Reading one to the end first deadlocks as
            // soon as the other fills its window, and node writes progress to stderr while the JSON
            // is still coming out of stdout, so that is not a theoretical case here.
            // Writing a whole tarball to stdin while nothing reads stdout deadlocks the same way
            // that reading one output stream to the end before the other does, so all three run at
            // once.
            val (out, err) = coroutineScope {
                val writing = stdin?.let { source ->
                    async(Dispatchers.IO) { writeStdin(process.outputStream, source) }
                }
                val stdout = async(Dispatchers.IO) { process.inputStream.readTextQuietly() }
                val stderr = async(Dispatchers.IO) { process.errorStream.readTextQuietly() }
                writing?.await()
                stdout.await() to stderr.await()
            }

            runCatching { process.join(timeoutMillis, TimeUnit.MILLISECONDS) }
            // No exit status is normal rather than an error: boot prints its JSON and then takes the
            // machine down while the channel is still open, so the reply is all we are going to get.
            CommandOutcome(exitStatus = process.exitStatus, stdout = out, stderr = err)
        } finally {
            finished.complete(Unit)
            watchdog.cancel()
            // Closing a channel writes to the connection and waits for the far side to answer, so on
            // a link that has just gone it throws. That must not cost us the reply we already have:
            // boot prints its JSON and then takes the machine down, which is precisely the case where
            // the read succeeded and the close cannot.
            runCatching { session.close() }
        }

        if (timedOut.get()) {
            throw AgentFailure.TimedOut(
                seconds = (timeoutMillis / 1000L).toInt(),
                detail = "The connection was open and the command had not finished. " +
                    "Partial output: ${outcome.stdout.take(200).trim()}",
            )
        }
        outcome
    }

    private fun unreachable(
        endpoint: Endpoint,
        failure: Throwable,
        stage: DispatchStage = DispatchStage.NOT_STARTED,
    ): AgentFailure.Unreachable {
        val what = when (failure) {
            is UnknownHostException -> "${endpoint.host} could not be resolved."
            is NoRouteToHostException -> "There is no route to ${endpoint.host}."
            is ConnectException -> "${endpoint.address} refused the connection."
            is SocketTimeoutException -> "${endpoint.address} did not answer in time."
            else -> describe(failure)
        }
        return AgentFailure.Unreachable(machineName, "${endpoint.label}: $what", failure, stage)
    }

    private fun describe(failure: Throwable): String {
        val root = generateSequence(failure) { it.cause }.last()
        val text = failure.message?.takeIf { it.isNotBlank() } ?: failure::class.java.simpleName
        val rootText = root.message?.takeIf { it.isNotBlank() && it != failure.message }
        return if (rootText == null) text else "$text ($rootText)"
    }

    init {
        SshjOnAndroid.configure()
    }

    private companion object {
        const val CONNECT_TIMEOUT_MILLIS = 8_000

        /** Connect, key exchange and public key auth. Generous for a phone, nowhere near a command. */
        const val HANDSHAKE_TIMEOUT_MILLIS = 25_000L

        /**
         * Short enough that the phone's carrier NAT does not drop the mapping while an update runs,
         * and it also puts traffic on a connection that is otherwise silent for minutes at a time.
         */
        const val KEEP_ALIVE_SECONDS = 20
    }
}

/**
 * The one adjustment sshj needs before it will work on Android.
 *
 * Android ships a cut down copy of Bouncy Castle already registered under the name "BC". Left alone,
 * sshj finds a provider by that name, concludes the full library is there, and then asks it for
 * algorithms it does not have, which shows up as a handshake that cannot agree on a cipher. Telling
 * sshj to register nothing and to name no provider sends every JCE lookup to the platform default,
 * which does have them.
 *
 * This is global state on a library class, so it is done exactly once. An object's initialiser is the
 * only way to get "once, before anyone else, and thread safe" without writing the locking by hand.
 */
private object SshjOnAndroid {
    init {
        SecurityUtils.setRegisterBouncyCastle(false)
        SecurityUtils.setSecurityProvider(null)
    }

    fun configure() = Unit
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

/**
 * Writes the input and closes it, because a command reading stdin waits for end of file.
 *
 * Failures are swallowed on purpose. A far side that exited before reading its input closes the
 * channel, and the write then throws; that is not the interesting failure, the reply is, and losing
 * the reply to report a broken pipe would hide what actually happened.
 */
private fun writeStdin(output: OutputStream, source: StdinSource) {
    try {
        output.use { sink ->
            when (source) {
                is StdinSource.Bytes -> sink.write(source.bytes)
                is StdinSource.Streamed -> source.open().use { it.copyTo(sink, 64 * 1024) }
            }
            sink.flush()
        }
    } catch (_: IOException) {
        // The far side stopped listening. Whatever it said is the answer.
    }
}

/**
 * Reads a stream to the end and treats a broken pipe as the end.
 *
 * Buffered incrementally rather than read in one go, so that what arrived before the break survives.
 * That is the whole reason this is not readBytes(): a command that reboots the machine ends its own
 * connection, and the JSON it printed first is the answer. Throwing there would lose a reply we did
 * in fact receive.
 */
private fun InputStream.readTextQuietly(): String {
    val collected = ByteArrayOutputStream()
    val chunk = ByteArray(8 * 1024)
    try {
        while (true) {
            val read = read(chunk)
            if (read < 0) break
            collected.write(chunk, 0, read)
        }
    } catch (_: IOException) {
        // Whatever is in the buffer is the answer.
    }
    return collected.toString(Charsets.UTF_8.name())
}
