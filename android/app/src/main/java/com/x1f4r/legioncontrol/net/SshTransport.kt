package com.x1f4r.legioncontrol.net

import com.x1f4r.legioncontrol.agent.Addresses
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.FailureKind
import com.x1f4r.legioncontrol.data.DeviceIdentity
import com.x1f4r.legioncontrol.data.HostKeyStore
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import net.schmizz.keepalive.KeepAliveProvider
import net.schmizz.sshj.Config
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.common.SecurityUtils
import net.schmizz.sshj.connection.channel.direct.Session
import net.schmizz.sshj.userauth.UserAuthException
import net.schmizz.sshj.userauth.keyprovider.KeyProvider
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

/**
 * One ssh connection per command: connect, run one thing, read both streams, hang up.
 *
 * No pooling and no persistent session on purpose. The commands are seconds apart at best and
 * minutes apart normally, a phone loses its network constantly, and half of what this app runs
 * (boot, restart) deliberately kills the connection it arrived on. A pool would spend most of its
 * life holding dead sockets and would have to detect that anyway.
 *
 * Contract section 15.4 for a client with an ssh library in its own process: before the exec request
 * is written, every failure is one where nothing was sent, and it is named by what failed; from the
 * moment the exec request is written, any failure is a link that may have run the command.
 */
class SshTransport internal constructor(
    private val keys: suspend () -> KeyProvider,
    private val verifierFor: (address: String) -> PinnedHostKeyVerifier,
    /** Only ever used to write a sentence: which machine it was that did not answer. */
    private val machineName: String,
    /** The sshj client for one command. Replaced in tests, and only there. */
    private val newClient: (Config) -> SSHClient = ::SSHClient,
    /** How much of each output stream is kept. Smaller in tests, and only there. */
    private val outputCapBytes: Int = OUTPUT_CAP_BYTES,
    /**
     * Milliseconds, for what the handshake left of an attempt. Replaced in tests, and only there.
     * Monotonic for the reason the client's deadline is: a clock set by the network mid-attempt
     * would hand the command time the attempt does not have.
     */
    private val clock: () -> Long = { System.nanoTime() / 1_000_000L },
) : CommandTransport {

    constructor(identity: DeviceIdentity, hostKeys: HostKeyStore, machineName: String) : this(
        keys = { identity.identity() },
        verifierFor = { address -> PinnedHostKeyVerifier(hostKeys, address) },
        machineName = machineName,
    )

    /**
     * Runs [command] on [endpoint] and returns what it printed.
     *
     * Throws an [AgentFailure] and nothing else: every ssh level failure is translated here, because
     * this is the only place that knows the difference between a refused key and a refused socket.
     *
     * All of it runs on the IO dispatcher, closing the connection included. Closing writes a
     * disconnect to the socket and can block on a link that has gone, and the caller of this is
     * usually a screen on the main thread.
     */
    override suspend fun run(
        endpoint: Endpoint,
        command: String,
        timeoutMillis: Long,
        stdin: StdinSource?,
    ): CommandOutcome {
        val started = clock()
        refuseUndialable(endpoint)
        return withContext(Dispatchers.IO) {
            val config = DefaultConfig().apply {
                // The long commands (update takes minutes) sit silent on a phone's connection, which
                // is exactly the shape of traffic a mobile NAT drops. A heartbeat keeps the mapping
                // alive.
                keepAliveProvider = KeepAliveProvider.KEEP_ALIVE
            }

            val verifier = verifierFor(endpoint.address)
            val client = newClient(config)

            // Flipped the instant before the command is handed to a channel, and read by every
            // failure path below. It is the difference between "the far side never heard this" and
            // "the far side may have run it", and a mutation is only ever repeated on the first of
            // those.
            val dispatched = AtomicBoolean(false)

            // The whole blocking conversation runs as a child, and this coroutine does nothing but
            // wait on it. That split is what makes cancellation work at all. sshj reads from a plain
            // socket, and a blocking socket read notices neither a cancelled coroutine nor an
            // interrupted thread; the only thing that breaks it is closing the socket, and that has
            // to be done by something that is not itself blocked inside the read. Waiting on a
            // Deferred is such a thing, because await is a suspension point and gives up the moment
            // the caller goes away.
            val work = async {
                try {
                    client.connectTimeout = CONNECT_TIMEOUT_MILLIS
                    // sshj copies this onto the socket once, inside connect, and never looks at it
                    // again, so it has to be the value the long silent middle of an update needs
                    // rather than the one the handshake needs. The handshake gets its own bound from
                    // the watchdog below.
                    client.timeout = timeoutMillis.coerceIn(1L, Int.MAX_VALUE.toLong()).toInt()
                    client.addHostKeyVerifier(verifier)

                    handshake(client, endpoint, verifier, timeoutMillis)

                    client.connection.keepAlive.keepAliveInterval = KEEP_ALIVE_SECONDS

                    // One attempt has one budget, and the handshake has already spent some of it.
                    // Given the whole amount again, a status whose handshake was slow ran for nearly
                    // twice what the caller had left, past the one deadline the walk keeps.
                    val remaining = (timeoutMillis - (clock() - started)).coerceAtLeast(1L)
                    exec(client, endpoint, command, remaining, dispatched, stdin)
                } catch (cancelled: CancellationException) {
                    throw cancelled
                } catch (failure: AgentFailure) {
                    throw failure
                } catch (failure: Exception) {
                    // The last word on this class's promise to throw AgentFailure and nothing else.
                    // sshj throws checked exceptions out of places that are easy to miss, and the
                    // caller above this one catches AgentFailure only: anything that got past here
                    // would not become a sentence at the bottom of the screen, it would take the app
                    // down.
                    throw failureAfter(endpoint, failure, dispatched)
                } finally {
                    closeQuietly(client)
                }
            }

            try {
                work.await()
            } catch (cancelled: CancellationException) {
                // The child is now cancelled but still parked in a read that will not return on its
                // own. Closing the client here is what lets it unwind, which it has to do before
                // this scope is allowed to finish. Without this the thread would be held until the
                // socket timeout, and for an update that is minutes after the screen it belonged to
                // has gone. NonCancellable, because this coroutine is already cancelled and the close
                // still has to happen.
                withContext(NonCancellable) { closeQuietly(client) }
                throw cancelled
            } catch (failure: AgentFailure) {
                throw failure
            } catch (failure: Exception) {
                // Nothing above this line is allowed to leak a raw ssh exception. The caller catches
                // AgentFailure and only AgentFailure, and every one of these arrives on a coroutine
                // that the screen launched, so anything else would take the whole app down rather
                // than turn into a sentence. sshj throws from more places than the ones translated
                // by hand: opening a channel, closing one, and every write on a link that has
                // already gone.
                throw failureAfter(endpoint, failure, dispatched)
            }
        }
    }

    /**
     * Connect, key exchange and public key authentication, inside a bound of their own.
     *
     * A server that accepts the TCP connection and then says nothing would otherwise hold the whole
     * command timeout, which for an update is minutes of a button doing nothing. Same trick as the
     * command's own watchdog, for the same reason: a separate coroutine, on a separate thread,
     * closing the socket the blocked one is stuck on. Never longer than the attempt itself has.
     */
    private suspend fun handshake(
        client: SSHClient,
        endpoint: Endpoint,
        verifier: PinnedHostKeyVerifier,
        timeoutMillis: Long,
    ) = coroutineScope {
        val connected = CompletableDeferred<Unit>()
        val expired = AtomicBoolean(false)
        val watchdog = launch {
            if (withTimeoutOrNull(minOf(HANDSHAKE_TIMEOUT_MILLIS, timeoutMillis)) { connected.await() } == null) {
                expired.set(true)
                closeQuietly(client)
            }
        }
        try {
            connect(client, endpoint, verifier, expired)
            authenticate(client, endpoint, expired)
        } finally {
            connected.complete(Unit)
            watchdog.cancel()
        }
    }

    private fun connect(
        client: SSHClient,
        endpoint: Endpoint,
        verifier: PinnedHostKeyVerifier,
        handshakeExpired: AtomicBoolean,
    ) {
        try {
            client.connect(endpoint.host, endpoint.port)
        } catch (failure: Exception) {
            // Order matters: a host key mismatch surfaces as a transport exception like any other,
            // so the recorded mismatch has to be checked before the generic translation.
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
            throw unreachable(endpoint, failure, DispatchStage.NOT_STARTED, kindBeforeExec(failure, handshakeExpired))
        }
    }

    private suspend fun authenticate(client: SSHClient, endpoint: Endpoint, handshakeExpired: AtomicBoolean) {
        // Read apart from the authentication itself, because a key this phone could not read is a
        // fact about the phone. Folded into the generic translation below it read as the machine not
        // answering, and sent the user looking at the network.
        val key = try {
            keys()
        } catch (failure: CancellationException) {
            throw failure
        } catch (failure: Exception) {
            throw AgentFailure.KeyUnavailable(failure)
        }
        try {
            client.authPublickey(endpoint.user, key)
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
            throw unreachable(endpoint, failure, DispatchStage.NOT_STARTED, kindBeforeExec(failure, handshakeExpired))
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
    override suspend fun upload(
        endpoint: Endpoint,
        source: File,
        remotePath: String,
        timeoutMillis: Long,
    ) {
        refuseUndialable(endpoint)
        withContext(Dispatchers.IO) {
            val verifier = verifierFor(endpoint.address)
            val client = newClient(DefaultConfig())
            val work = async {
                try {
                    client.connectTimeout = CONNECT_TIMEOUT_MILLIS
                    client.timeout = timeoutMillis.coerceIn(1L, Int.MAX_VALUE.toLong()).toInt()
                    client.addHostKeyVerifier(verifier)
                    handshake(client, endpoint, verifier, timeoutMillis)
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
                    throw unreachable(endpoint, failure, DispatchStage.AMBIGUOUS, FailureKind.LINK_LOST)
                } finally {
                    closeQuietly(client)
                }
            }
            try {
                work.await()
            } catch (cancelled: CancellationException) {
                withContext(NonCancellable) { closeQuietly(client) }
                throw cancelled
            } catch (failure: AgentFailure) {
                throw failure
            } catch (failure: Exception) {
                throw unreachable(endpoint, failure, DispatchStage.AMBIGUOUS, FailureKind.LINK_LOST)
            }
        }
    }

    private suspend fun exec(
        client: SSHClient,
        endpoint: Endpoint,
        command: String,
        remainingMillis: Long,
        dispatched: AtomicBoolean,
        stdin: StdinSource?,
    ): CommandOutcome = coroutineScope {
        val session: Session = try {
            client.startSession()
        } catch (failure: CancellationException) {
            throw failure
        } catch (failure: Exception) {
            // The key was accepted and the channel was not opened, so the command was never handed
            // over. Reported as a refusal rather than as unreadable output, because "unreadable" is
            // the ambiguous case and this one is not: nothing ran.
            throw AgentFailure.Unreachable(
                machineName,
                "$machineName accepted the key and then refused a session. ${describe(failure)}",
                failure,
                DispatchStage.NOT_STARTED,
                FailureKind.CONNECTION_REFUSED,
            )
        }

        // The command's own wall clock, set to what the handshake left of the attempt, and the only
        // thing that actually bounds it. The socket read timeout cannot: a link that is up but
        // carrying a command that never finishes stays busy forever, and the read the two drains
        // below are parked in never returns. Same shape as the handshake watchdog, for the same
        // reason, because a blocking read only ends when someone else closes the socket.
        val finished = CompletableDeferred<Unit>()
        val timedOut = AtomicBoolean(false)
        val watchdog = launch {
            if (withTimeoutOrNull(remainingMillis) { finished.await() } == null) {
                timedOut.set(true)
                closeQuietly(client)
            }
        }

        try {
            // Set before the exec request is written, not after it returns. The request is on the
            // wire the moment exec starts writing it, and an exec that then throws, because the link
            // dropped or the far side closed the channel, may well have started the command. Read
            // the other way round, that failure was reported as never sent and a change was free to
            // go down the next address.
            dispatched.set(true)
            val process = session.exec(command)
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
                val stdout = async(Dispatchers.IO) { process.inputStream.readCapped(outputCapBytes) }
                val stderr = async(Dispatchers.IO) { process.errorStream.readCapped(outputCapBytes) }
                writing?.await()
                stdout.await() to stderr.await()
            }

            runCatching { process.join(remainingMillis, TimeUnit.MILLISECONDS) }
            // No exit status is normal rather than an error: boot prints its JSON and then takes the
            // machine down while the channel is still open, so the reply is all we are going to get.
            CommandOutcome(
                exitStatus = process.exitStatus,
                stdout = out.text,
                stderr = err.text,
                timedOut = timedOut.get(),
                stdoutTruncated = out.truncated,
            )
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (failure: Exception) {
            // The watchdog closing the connection under an exec that had not returned yet is a
            // command that ran out of time, and is read as one. Anything else is the link.
            if (!timedOut.get()) throw failureAfter(endpoint, failure, dispatched)
            CommandOutcome(exitStatus = null, stdout = "", stderr = "", timedOut = true)
        } finally {
            finished.complete(Unit)
            watchdog.cancel()
            // Closing a channel writes to the connection and waits for the far side to answer, so on
            // a link that has just gone it throws. That must not cost us the reply we already have:
            // boot prints its JSON and then takes the machine down, which is precisely the case where
            // the read succeeded and the close cannot.
            runCatching { session.close() }
        }
    }

    /**
     * Refuses an address outside the dial grammar of contract section 15.6 before a socket is opened.
     *
     * Nothing is dialled, scanned or pinned for a host, user or port that fails it, and the sentence
     * names the entry so it can be corrected.
     */
    private fun refuseUndialable(endpoint: Endpoint) {
        val part = when {
            Addresses.hostForm(endpoint.host) == null -> "host \"${endpoint.host}\""
            !Addresses.isUser(endpoint.user) -> "user \"${endpoint.user}\""
            !Addresses.isPort(endpoint.port) -> "port ${endpoint.port}"
            else -> return
        }
        throw AgentFailure.BadRequest(
            "This app does not dial ${endpoint.label} as it is written.",
            "The $part of ${endpoint.label} on $machineName is not one this app dials, scans or " +
                "pins a host key for, so that address was skipped. Correct it in the setup.",
        )
    }

    /** A failure once the handshake has started, named by how far the command got. */
    private fun failureAfter(endpoint: Endpoint, failure: Throwable, dispatched: AtomicBoolean): AgentFailure =
        if (dispatched.get()) {
            unreachable(endpoint, failure, DispatchStage.AMBIGUOUS, FailureKind.LINK_LOST)
        } else {
            unreachable(endpoint, failure, DispatchStage.NOT_STARTED, FailureKind.CONNECTION_REFUSED)
        }

    /**
     * What a failure before the exec request means, as section 15.4 names it for an in-process
     * library: name resolution, no route and a socket that never answered are an unreachable host,
     * and a refused socket or a key exchange that went wrong is a refusal.
     */
    private fun kindBeforeExec(failure: Throwable, handshakeExpired: AtomicBoolean): FailureKind {
        val chain = generateSequence(failure) { it.cause }.toList()
        return when {
            chain.any { it is UnknownHostException || it is NoRouteToHostException || it is SocketTimeoutException } ->
                FailureKind.HOST_UNREACHABLE

            chain.any { it is ConnectException } -> FailureKind.CONNECTION_REFUSED
            // The watchdog closed a socket that connected and then said nothing in time, which
            // OpenSSH reports as a banner exchange that timed out.
            handshakeExpired.get() -> FailureKind.HOST_UNREACHABLE
            else -> FailureKind.CONNECTION_REFUSED
        }
    }

    private fun unreachable(
        endpoint: Endpoint,
        failure: Throwable,
        stage: DispatchStage,
        kind: FailureKind,
    ): AgentFailure.Unreachable {
        val what = when (failure) {
            is UnknownHostException -> "${endpoint.host} could not be resolved."
            is NoRouteToHostException -> "There is no route to ${endpoint.host}."
            is ConnectException -> "${endpoint.address} refused the connection."
            is SocketTimeoutException -> "${endpoint.address} did not answer in time."
            else -> describe(failure)
        }
        return AgentFailure.Unreachable(machineName, "${endpoint.label}: $what", failure, stage, kind)
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

    internal companion object {
        const val CONNECT_TIMEOUT_MILLIS = 8_000

        /** Connect, key exchange and public key auth. Generous for a phone, nowhere near a command. */
        const val HANDSHAKE_TIMEOUT_MILLIS = 25_000L

        /**
         * Short enough that the phone's carrier NAT does not drop the mapping while an update runs,
         * and it also puts traffic on a connection that is otherwise silent for minutes at a time.
         */
        const val KEEP_ALIVE_SECONDS = 20

        /**
         * How much of stdout and of stderr is kept: 16 MiB each, the rest read and discarded. Every
         * reply the agent writes is a small fraction of this, and a far side that prints without end
         * must not be able to take the phone's memory with it.
         */
        const val OUTPUT_CAP_BYTES = 16 * 1024 * 1024
    }
}

/** Closes without letting a failure to close hide the failure that led here. */
private fun closeQuietly(client: SSHClient) {
    runCatching { client.close() }
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

/** What was kept of one output stream, and whether anything had to be left out. */
private class Captured(val text: String, val truncated: Boolean)

/**
 * Reads a stream to the end and treats a broken pipe as the end.
 *
 * Buffered incrementally rather than read in one go, so that what arrived before the break survives.
 * That is the whole reason this is not readBytes(): a command that reboots the machine ends its own
 * connection, and the JSON it printed first is the answer. Throwing there would lose a reply we did
 * in fact receive.
 *
 * At most [cap] bytes are kept. Past that the stream is still read to its end and the rest is thrown
 * away, because a far side whose output nobody reads stops at a full window and never exits.
 */
private fun InputStream.readCapped(cap: Int): Captured {
    val collected = ByteArrayOutputStream()
    val chunk = ByteArray(8 * 1024)
    var truncated = false
    try {
        while (true) {
            val read = read(chunk)
            if (read < 0) break
            val room = cap - collected.size()
            if (room > 0) collected.write(chunk, 0, minOf(read, room))
            if (read > room) truncated = true
        }
    } catch (_: IOException) {
        // Whatever is in the buffer is the answer.
    }
    return Captured(collected.toString(Charsets.UTF_8.name()), truncated)
}
