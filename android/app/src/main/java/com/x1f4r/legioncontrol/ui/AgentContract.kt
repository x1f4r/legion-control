package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.SetupSource
import com.x1f4r.legioncontrol.net.RouteKind
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow

/**
 * The shape the screen expects from the transport.
 *
 * The payload types are the transport's own: one description of what the agent says, shared by the
 * layer that parses it and the layer that draws it. A second copy over here would only be a second
 * thing to keep in step, and the first field that drifted would be invisible until it was wrong on
 * screen. What this file does add is the handful of ideas that only exist for the sake of the
 * screen: what the link looks like, which address answered, and whether waking is possible at all.
 */

/** Which address answered. The user is told, because it changes what the app can do. */
data class Route(val label: String, val kind: RouteKind) {
    val displayName: String get() = "over $label"
}

/**
 * What the link to a machine looks like right now.
 *
 * These are kept apart because what the user should do about them is completely different, and a
 * single "could not connect" would send them looking in the wrong place every time.
 */
sealed interface LinkState {
    /** Nothing has been read yet. Not the same as offline, and it must not be drawn as if it were. */
    data object Unknown : LinkState

    /** The agent answered and said which system it is. */
    data class Online(val system: MachineSystem) : LinkState

    /** Nothing answered on any address. Asleep, off, or off the network. */
    data class Offline(val reason: String) : LinkState

    /** sshd refused the key. On a fresh install this is the normal first result. */
    data object NeedsKeyAuthorisation : LinkState

    /** A shell opened and the control agent was not on the far side. */
    data class AgentMissing(val system: MachineSystem) : LinkState

    /**
     * The host offered a key that none of the ones trusted for that address match.
     *
     * Expected rather than alarming on a LAN address of a machine that dual boots, because every
     * system on it answers there with its own host key, so each of them trips this once. It still
     * has to be the user's decision, so it gets its own state and its own action rather than being
     * trusted quietly. The offered key rides along so that the trust action stores exactly what was
     * seen, not a rescan.
     */
    data class HostKeyChanged(val address: String, val offeredKeyBlob: String) : LinkState
}

/** Whether a wake packet can reach a machine right now, and in one sentence why not. */
data class WakeReadiness(
    val possible: Boolean,
    val explanation: String,
)

/**
 * One machine's transport, as the screen sees it.
 *
 * Every call comes back as a [Result] rather than throwing, because on this screen a failure is not
 * an exceptional case: it is the machine being asleep, which is most of the time.
 */
interface MachineClient {
    val machine: Machine

    /** Follows the network, so the wake action can be enabled and disabled while the app is open. */
    fun wakeReadiness(): Flow<WakeReadiness>

    /** Which address answered last. Null until one has. */
    val route: Route?

    suspend fun status(): Result<AgentStatus>

    /**
     * [serviceId] is null for an agent that predates services, and the caller decides that from the
     * status it is working from. The first version of the agent refuses a flag it does not know by
     * name, so sending `--service` to it fails the command rather than being ignored.
     */
    suspend fun update(serviceId: String?, force: Boolean): Result<AgentActionResult>

    suspend fun restart(serviceId: String?, force: Boolean): Result<AgentActionResult>

    suspend fun setAutoUpdate(on: Boolean): Result<AgentActionResult>

    suspend fun boot(targetId: String, force: Boolean): Result<AgentActionResult>

    /** Suspends the machine. Busy gated by the agent exactly as restart and boot are. */
    suspend fun sleep(force: Boolean): Result<AgentActionResult>

    /** Runs one of the actions the agent offered. */
    suspend fun run(actionId: String, force: Boolean): Result<AgentActionResult>

    /**
     * The setup this machine carries, read over the route that just answered.
     *
     * Asked for only when the machine's status said it is holding a document the phone has not got,
     * because every reply carries the hash and only a difference is worth a second round trip.
     */
    suspend fun fetchSetup(): Result<ControllerFetch>

    /** Sends the magic packet and waits for the probe address. Fails with a sentence if it does not. */
    suspend fun wake(): Result<Unit>

    /** Adds an offered host key to the ones trusted for an address. The keys already there stay. */
    suspend fun trustHostKey(address: String, keyBlobBase64: String)
}

/**
 * The phone's own business, and the door to the machines.
 *
 * The configuration is here rather than beside the machines because it is what decides that there
 * are any: with nothing pasted there is no machine, no route and nothing to talk to, and this is
 * still the object the screen has.
 */
interface ControlServices {
    /** This phone's key in OpenSSH format. Generated on first use, so this may have to wait for it. */
    suspend fun publicKey(): String

    /** The configuration in force, or null when there is none. */
    val config: StateFlow<ControllerConfig?>

    /** Exactly the text that was applied, so the field on screen starts where the user left it. */
    val configText: StateFlow<String>

    /** The hash of the document in force, which is what a machine's own hash is compared against. */
    val configHash: StateFlow<String?>

    /** The machine the setup was last fetched from, so the fields start where they were left. */
    val setupSource: StateFlow<SetupSource?>

    /** Checks, stores and puts a document in force. Returns null, or the reason nothing changed. */
    fun applyConfig(text: String): String?

    /** The same, for a document a machine served, keeping the hash that machine gave it. */
    fun applyFetchedConfig(text: String, hash: String?): String?

    /**
     * Runs `config` on an address the app has been told nothing else about.
     *
     * The bootstrap, and the one call that goes somewhere the configuration does not name: the
     * phone has nothing and any one machine has all of it.
     */
    suspend fun fetchSetupFrom(source: SetupSource): Result<ControllerFetch>

    /** Adds an offered host key to the ones trusted for an address. The keys already there stay. */
    suspend fun trustHostKey(address: String, keyBlobBase64: String)

    /** One client per machine the configuration describes, in the order it lists them. */
    fun clients(configuration: ControllerConfig?): List<MachineClient>
}

/**
 * The phone's copy of the setup, from the point of view of one machine's poll.
 *
 * A machine that reports a document the phone has not got hands it over on the spot, and applying
 * it rebuilds every machine model there is, including the one that is in the middle of doing it.
 * That is why the note lives out here rather than on the model: the model that earned the line is
 * thrown away by the very change it made, and the line still has to be on the page afterwards.
 */
interface SetupSync {
    /** The hash of the document in force, or null when there is none. */
    val appliedHash: String?

    /** Applies a document a machine served. Returns null, or the sentence saying why it was not. */
    fun applyFromMachine(document: String, hash: String?): String?

    /** Remembers one line about what a machine's handover did. */
    fun remember(machineId: String, line: String)

    /** That line, while it is still news. Null once it has stopped being true. */
    fun noteFor(machineId: String, now: Long): String?
}

// MARK: - Reading a failure

/** The sentence to put on screen. */
val Throwable.sentence: String
    get() = (this as? AgentFailure)?.summary ?: message ?: "That did not work."

/** The raw text to put under it: ssh output, exit codes, whatever there was. */
val Throwable.rawDetail: String?
    get() = (this as? AgentFailure)?.detail

/** What a failure means for the link, which is what decides how the screen changes. */
fun Throwable.toLinkState(): LinkState = when (this) {
    is AgentFailure.NotAuthorised -> LinkState.NeedsKeyAuthorisation
    is AgentFailure.HostKeyChanged -> LinkState.HostKeyChanged(address, offeredKeyBlob)
    // A command that ran too long says nothing about the link, because the link is what carried it
    // right up to the moment we gave up. Drawing that as offline would claim the machine is asleep on
    // the strength of it having answered, and the boot action reads an offline link as "the reboot
    // took the connection with it", which would report a machine that never moved as restarting.
    is AgentFailure.TimedOut -> LinkState.Unknown
    is AgentFailure.AgentMissing -> system?.let { LinkState.AgentMissing(it) } ?: LinkState.Offline(summary)
    else -> LinkState.Offline(sentence)
}
