package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ConfigMetaReply
import com.x1f4r.legioncontrol.agent.ConfigSetReply
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.DispatchStage
import com.x1f4r.legioncontrol.agent.DoctorReport
import com.x1f4r.legioncontrol.agent.EndpointDiagnosis
import com.x1f4r.legioncontrol.agent.HistoryReply
import com.x1f4r.legioncontrol.agent.InterruptRequest
import com.x1f4r.legioncontrol.agent.LogsReply
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.PolicyPatch
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.agent.UpdateRequest
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.OperationStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.SetupDivergence
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.SetupSource
import com.x1f4r.legioncontrol.net.RouteKind
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow
import java.io.File

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
 * single "could not connect" would send them looking in the wrong place every time. Unreachable, a
 * refused key, a changed host key, a missing agent, a command that overran and a link that dropped
 * after dispatch: the last two are one state here, [Unsettled], because the app does the same thing
 * about both, which is to refuse to draw a conclusion.
 */
sealed interface LinkState {
    /** Nothing has been read yet. Not the same as offline, and it must not be drawn as if it were. */
    data object Unknown : LinkState

    /** The agent answered and said which system it is. */
    data class Online(val system: MachineSystem) : LinkState

    /** Nothing answered on any address. Asleep, off, or off the network. */
    data class Offline(val reason: String) : LinkState

    /**
     * A command was carried and did not finish inside its budget, or a reply was lost after dispatch.
     *
     * Deliberately not offline. The link was up: it carried the command right to the moment we gave
     * up on it. Drawing that as "asleep" is what let a reboot that never happened be reported as one
     * that had, on the strength of the machine having answered.
     */
    data class Unsettled(val reason: String) : LinkState

    /** sshd refused the key. On a fresh install this is the normal first result. */
    data object NeedsKeyAuthorisation : LinkState

    /** A shell opened and the control agent was not on the far side. */
    data class AgentMissing(val system: MachineSystem) : LinkState

    /**
     * The host offered a key that none of the ones trusted for that address match, or none is
     * trusted yet.
     *
     * Both are a question rather than a fault, and neither is ever answered silently: this app does
     * not accept the first key it is shown. [isFirstContact] only changes the words.
     */
    data class HostKeyChanged(
        val address: String,
        val offeredKeyBlob: String,
        val offeredFingerprint: String,
        val isFirstContact: Boolean,
        val canApprove: Boolean,
    ) : LinkState

    /** The command could not be written for the shell the far side uses. A configuration fault. */
    data class Misconfigured(val reason: String) : LinkState
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

    /** What the last reply said the far side is: which contract, which version, which verbs. */
    val abilities: AgentAbilities

    /** Follows the network, so the wake action can be enabled and disabled while the app is open. */
    fun wakeReadiness(): Flow<WakeReadiness>

    /** Which address answered last. Null until one has. */
    val route: Route?

    suspend fun status(): Result<AgentStatus>

    suspend fun update(request: UpdateRequest, operationId: String?): Result<AgentActionResult>

    suspend fun restart(
        serviceId: String?,
        request: InterruptRequest,
        operationId: String?,
    ): Result<AgentActionResult>

    suspend fun setAutoUpdate(
        on: Boolean,
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult>

    suspend fun pauseUpdates(
        duration: String,
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult>

    suspend fun resumeUpdates(serviceId: String?, operationId: String?): Result<AgentActionResult>

    suspend fun writePolicy(
        patch: PolicyPatch,
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult>

    suspend fun readPolicy(serviceId: String?): Result<AgentActionResult>

    suspend fun boot(
        targetId: String,
        request: InterruptRequest,
        noReboot: Boolean,
        operationId: String?,
    ): Result<AgentActionResult>

    /** Suspends the machine. Busy gated by the agent exactly as restart and boot are. */
    suspend fun sleep(request: InterruptRequest, operationId: String?): Result<AgentActionResult>

    /** Runs one of the actions the agent offered. */
    suspend fun run(
        actionId: String,
        request: InterruptRequest,
        operationId: String?,
    ): Result<AgentActionResult>

    /** What became of an operation, by the id this app gave it. The heart of recovery. */
    suspend fun operation(operationId: String, waitSeconds: Int?): Result<OpRecord>

    /** Which setup this machine holds, its revision and its ancestry, without the document. */
    suspend fun configMeta(): Result<ConfigMetaReply>

    /**
     * Publishes a document to this machine.
     *
     * [replace] is the explicit human decision to overwrite a different setup and is never passed
     * without one. It is not a way past a divergence: divergence is merged, not overwritten.
     */
    suspend fun configSet(
        canonicalDocument: String,
        setupId: String,
        revision: Long,
        replace: Boolean,
    ): Result<ConfigSetReply>

    /** Withdraws a queued operation that has not started. */
    suspend fun cancel(operationId: String): Result<AgentActionResult>

    /** The scheduled maintenance cycle, run by hand or previewed. */
    suspend fun cycle(dryRun: Boolean, operationId: String?): Result<AgentActionResult>

    suspend fun history(limit: Int): Result<HistoryReply>

    suspend fun logs(lines: Int, operationId: String?): Result<LogsReply>

    /** The agent's own preflight. Only worth calling on an agent that speaks contract 3. */
    suspend fun doctor(deep: Boolean): Result<DoctorReport>
    suspend fun serviceConfigGet(): Result<kotlinx.serialization.json.JsonObject>
    suspend fun serviceConfigWrite(action: String, payload: kotlinx.serialization.json.JsonObject): Result<kotlinx.serialization.json.JsonObject>

    /** Doctor, status, history, logs and a redacted config, as text, for the export action. */
    suspend fun bundle(): Result<String>

    /** One authenticated request against every address, for the diagnostics page. On demand only. */
    suspend fun diagnose(): List<EndpointDiagnosis>

    /**
     * The setup this machine carries, read over the route that just answered.
     *
     * Asked for only when the machine's status said it is holding a document the phone has not got,
     * because every reply carries the mark and only a difference is worth a second round trip.
     */
    suspend fun fetchSetup(): Result<ControllerFetch>

    /** Sends the magic packet from this phone through the selected site interface. */
    suspend fun sendWakePacket(broadcasts: List<String>, prefixes: List<String>): String?

    /** Waits for an authenticated agent status. False when the machine never provided one. */
    suspend fun waitForWake(): Boolean

    /** Where the wait is watching, so a failure can name the address that never answered. */
    val wakeProbeAddress: String?

    /** Asks this machine to send a magic packet at another one on its own network. */
    suspend fun wakeProxyFor(machineId: String, operationId: String?): Result<AgentActionResult>

    /** Whether this machine's agent actually offers the wake verb. Reads `help` once. */
    suspend fun hasWakeProxy(): Boolean

    /** Puts a signed agent bundle on the machine and installs it. */
    suspend fun installAgent(tarball: File, operationId: String?): Result<AgentActionResult>

    /** Adds an offered host key to the ones trusted for an address. The keys already there stay. */
    fun configureHostSystems(address: String, systems: List<com.x1f4r.legioncontrol.data.HostIdentitySystem>, revision: Long)
    fun hostTrust(address: String): com.x1f4r.legioncontrol.data.HostTrustSnapshot
    suspend fun trustHostKey(address: String, keyBlobBase64: String, approval: com.x1f4r.legioncontrol.data.HostTrustApproval)
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

    /** This phone's key fingerprint, for comparing by eye. */
    suspend fun keyFingerprint(): String

    /** The configuration in force, or null when there is none. */
    val config: StateFlow<ControllerConfig?>

    /** Exactly the text that was applied, so the field on screen starts where the user left it. */
    val configText: StateFlow<String>

    /** The applied document as a tree, which is what an edit works on and what gets published. */
    val document: StateFlow<ControllerDocument?>

    /** Which setup the document in force is, which revision, and what it descends from. */
    val provenance: StateFlow<SetupProvenance?>

    /** A copy of the same setup that neither descends from nor is descended from this one. */
    val divergence: StateFlow<SetupDivergence?>

    /** A machine carrying an entirely different setup. */
    val identityClash: StateFlow<SetupIdentityClash?>

    /** Every IPv4 address this device holds, so the site can be worked out. */
    fun networkAddresses(): Flow<List<String>>

    /** What to call this device in a revision it writes. */
    fun deviceName(): String

    /** Drops the remembered route for a machine, after a boot or an observed system change. */
    fun forgetRoute(machineId: String)

    /** Makes the requested system's endpoints lead the next call after a boot handover. */
    fun prepareRoute(machineId: String, systemId: String, onSite: Boolean)

    /** The machine the setup was last fetched from, so the fields start where they were left. */
    val setupSource: StateFlow<SetupSource?>

    /** Every change this app has asked for, kept across launches. */
    val operations: OperationStore

    /** Checks, stores and puts a pasted or typed document in force. Returns null, or the reason. */
    fun applyPasted(text: String, deviceName: String?): String?

    /** Puts an edit of the applied document in force, as a revision descending from it. */
    fun applyEdit(document: ControllerDocument, deviceName: String?): String?

    /** Takes a copy off a machine. Only ever called when it descends from this device's copy. */
    fun adoptFromMachine(text: String, expectedHash: String?, machineName: String): SetupOutcome

    fun recordDivergence(
        machineName: String,
        reason: String,
        theirText: String,
        theirProvenance: SetupProvenance,
    )

    fun recordIdentityClash(
        machineName: String,
        reason: String,
        theirText: String,
        theirProvenance: SetupProvenance,
    )

    fun clearDivergence()

    fun clearIdentityClash()

    /** Takes the other side of a divergence wholesale. */
    fun adoptDivergent(divergence: SetupDivergence): String?

    /**
     * Keeps this device's copy, as a revision that descends from both sides.
     *
     * Not `--replace`: a revision whose ancestry contains the other machine's hash is one that every
     * machine on either branch accepts as an ordinary fast-forward, so nothing has to be overridden.
     */
    fun keepMineOver(divergence: SetupDivergence, deviceName: String?): String?

    /** Merges the two sides with a decision per differing entry. */
    fun applyMerge(
        merged: ControllerDocument,
        theirProvenance: SetupProvenance,
        deviceName: String?,
    ): String?

    /** Drops this device's setup and follows the other one. */
    fun adoptOtherSetup(clash: SetupIdentityClash): String?

    /** The document with that hash, when this device kept a copy. The base for a three-way merge. */
    fun revision(hash: String?): String?

    /**
     * Runs `config` on an address the app has been told nothing else about.
     *
     * The bootstrap, and the one call that goes somewhere the configuration does not name: the
     * phone has nothing and any one machine has all of it.
     */
    suspend fun fetchSetupFrom(source: SetupSource): Result<ControllerFetch>

    /** Adds an offered host key to the ones trusted for an address. The keys already there stay. */
    fun configureHostSystems(address: String, systems: List<com.x1f4r.legioncontrol.data.HostIdentitySystem>, revision: Long)
    fun hostTrust(address: String): com.x1f4r.legioncontrol.data.HostTrustSnapshot
    suspend fun trustHostKey(address: String, keyBlobBase64: String, approval: com.x1f4r.legioncontrol.data.HostTrustApproval)

    /** Every trusted host key, as address to fingerprints, for the diagnostics page. */
    fun trustedHostKeys(): Map<String, List<String>>

    /** Drops every key for one address, so the next connection asks about whatever is offered. */
    fun forgetHostKeys(address: String)

    /** One client per machine the configuration describes, in the order it lists them. */
    fun clients(configuration: ControllerConfig?): List<MachineClient>
}

// MARK: - Reading a failure

/** The sentence to put on screen. */
val Throwable.sentence: String
    get() = (this as? AgentFailure)?.summary ?: message ?: "That did not work."

/** The raw text to put under it: ssh output, exit codes, whatever there was. */
val Throwable.rawDetail: String?
    get() = (this as? AgentFailure)?.detail

/**
 * Whether the command may have reached the far side.
 *
 * Anything that is not an [AgentFailure] arrived from somewhere this app does not model, and the
 * conservative answer is the ambiguous one: a change is never repeated on a maybe.
 */
val Throwable.dispatchStage: DispatchStage
    get() = (this as? AgentFailure)?.dispatch ?: DispatchStage.AMBIGUOUS

/** What a failure means for the link, which is what decides how the screen changes. */
fun Throwable.toLinkState(): LinkState = when (this) {
    is AgentFailure.NotAuthorised -> LinkState.NeedsKeyAuthorisation
    is AgentFailure.HostKeyChanged -> LinkState.HostKeyChanged(
        address = address,
        offeredKeyBlob = offeredKeyBlob,
        offeredFingerprint = offeredFingerprint,
        isFirstContact = isFirstContact,
        canApprove = canApprove,
    )

    is AgentFailure.Unquotable -> LinkState.Misconfigured(detail ?: summary)
    // A command that ran too long says nothing about the link, because the link is what carried it
    // right up to the moment we gave up. Drawing that as offline would claim the machine is asleep on
    // the strength of it having answered, and a boot handler that reads an offline link as "the
    // reboot took the connection with it" would report a machine that never moved as restarting.
    is AgentFailure.TimedOut -> LinkState.Unsettled(summary)
    is AgentFailure.BadOutput -> LinkState.Unsettled(summary)
    is AgentFailure.AgentMissing -> system?.let { LinkState.AgentMissing(it) } ?: LinkState.Offline(summary)
    is AgentFailure.Unreachable ->
        // A link that dropped while a command was on it is not the same as one that was never there.
        // The first is unsettled and the second is offline, and only the second is a reason to say
        // the machine is asleep.
        if (dispatch == DispatchStage.AMBIGUOUS) LinkState.Unsettled(summary) else LinkState.Offline(sentence)

    else -> LinkState.Offline(sentence)
}
