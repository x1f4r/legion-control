package com.x1f4r.legioncontrol.ui

import android.net.Network
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
import com.x1f4r.legioncontrol.agent.LaunchGate
import com.x1f4r.legioncontrol.agent.LogsReply
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.PolicyPatch
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.agent.SignedTarball
import com.x1f4r.legioncontrol.agent.UpdateRequest
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.OperationStore
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.DocumentAcceptance
import com.x1f4r.legioncontrol.data.KeyChange
import com.x1f4r.legioncontrol.data.LaunchReviewEntry
import com.x1f4r.legioncontrol.data.SetupDivergence
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.SetupSource
import com.x1f4r.legioncontrol.net.LocalAddress
import com.x1f4r.legioncontrol.net.NetworkPaths
import com.x1f4r.legioncontrol.net.RouteKind
import com.x1f4r.legioncontrol.net.WakeSend
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow

/**
 * The shape the screen expects from the transport.
 *
 * The payload types are the transport's own: one description of what the agent says, shared by the
 * layer that parses it and the layer that draws it. A second copy over here would only be a second
 * thing to keep in step, and the first field that drifted would be invisible until it was wrong on
 * screen. What this file does add is the handful of ideas that only exist for the sake of the
 * screen: what the link looks like and which address answered.
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

    /**
     * Nothing answered on any address. Asleep, off, or off the network.
     *
     * [noTunnel] when what went unanswered was a tailnet address no tunnel on this phone carries,
     * which is the phone's to fix and not the machine's. Offline all the same, so a wake is still
     * offered: the machine may be asleep as well.
     */
    data class Offline(val reason: String, val noTunnel: Boolean = false) : LinkState

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

    /**
     * This phone's own key could not be read, so nothing was sent.
     *
     * Not offline, and not a fault in the setup: the machine may well have answered, and what failed
     * is on this side. Drawn as offline it read as a machine that was asleep, offered to wake it, and
     * during a reboot went on waiting for a machine that may have been back for minutes.
     */
    data class PhoneKeyUnavailable(val reason: String) : LinkState
}

/**
 * One machine's transport, as the screen sees it.
 *
 * Every call comes back as a [Result] rather than throwing, because on this screen a failure is not
 * an exceptional case: it is the machine being asleep, which is most of the time.
 */
interface MachineClient {
    val machine: Machine

    /** What the last reply said the far side is: which contract and which version. */
    val abilities: AgentAbilities

    /** Which address answered last. Null until one has. */
    val route: Route?

    suspend fun status(): Result<AgentStatus>

    suspend fun update(request: UpdateRequest, operationId: String?): Result<AgentActionResult>

    suspend fun restart(
        serviceId: String?,
        request: InterruptRequest,
        operationId: String?,
    ): Result<AgentActionResult>

    // The schedule and policy writes take no operation id: the agent takes none for them.
    suspend fun setAutoUpdate(on: Boolean, serviceId: String?): Result<AgentActionResult>

    suspend fun pauseUpdates(duration: String, serviceId: String?): Result<AgentActionResult>

    suspend fun resumeUpdates(serviceId: String?): Result<AgentActionResult>

    suspend fun writePolicy(patch: PolicyPatch, serviceId: String?): Result<AgentActionResult>

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

    /**
     * Sends this machine's magic packets from this phone, as a direct wake step says: to every one of
     * [broadcasts] on every one of [ports], bound to [via], the network of the address that placed
     * the machine on this device's network.
     */
    suspend fun sendWakePacket(broadcasts: List<String>, ports: List<Int>, via: Network?): WakeSend

    /** Puts a signed agent bundle on the machine and installs it. */
    suspend fun installAgent(tarball: SignedTarball, operationId: String?): Result<AgentActionResult>

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

    /** When this phone's key was replaced because the old one could no longer be opened, until seen. */
    suspend fun keyChange(): KeyChange?

    /** The user has seen [keyChange]. */
    suspend fun acknowledgeKeyChange()

    /**
     * Makes this phone a new key because the old one keeps failing to read, and only because the
     * user confirmed it. A key that reads by now is kept.
     */
    suspend fun replaceUnreadableKey()

    /** The configuration in force, or null when there is none. What it says, for the screen. */
    val config: StateFlow<ControllerConfig?>

    /**
     * What machines are driven from: the configuration in force with every command as this device
     * approved it (contract section 9.9), and each launch value that waits for approval.
     */
    val launch: StateFlow<LaunchGate.Effective>

    /** Why the record of approved commands could not be read, when it could not. */
    val launchProblem: StateFlow<String?>

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

    /**
     * Every address this device could send a wake packet from, each with its network, so the site
     * can be worked out and a packet bound to the right network.
     */
    fun networkAddresses(): Flow<List<LocalAddress>>

    /** The way out of this phone as it changes: its default network, and what each tunnel carries. */
    fun networkPaths(): Flow<NetworkPaths>

    /**
     * Hands the transport the way out of this phone as it is now. A change lets every address be
     * dialled again, and the tunnels decide whether a silent tailnet address is the phone's doing.
     */
    fun followPaths(paths: NetworkPaths)

    /** A person asked to check again: every address held back is dialled on its machine's next call. */
    fun retryAddresses()

    /**
     * Hands the transport the machines this phone is on the network of now, by id: the test route
     * order and direct wake share. On one of them a silent LAN address is what is said, and not a
     * tunnel that is off.
     */
    fun followPlacement(onSite: Set<String>)

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

    /**
     * Puts a fetched setup in force after a person was shown all of its launch values, if it hashes
     * to [servedHash] and the document in force is still [expectedHead]. Approves them on a device
     * that applied no setup before, and nothing on one that did. See
     * [com.x1f4r.legioncontrol.data.ConfigStore.applyEnrolment].
     */
    fun applyEnrolment(text: String, servedHash: String?, expectedHead: String?, servedBy: String, deviceName: String?): SetupOutcome

    /** Records exactly the values [entries] showed as approved on this device. Returns null, or why not. */
    fun approveLaunch(entries: List<LaunchReviewEntry>): String?

    /**
     * What contract section 9.7 says of the document in force: its warnings and, when an earlier
     * build stored it under other rules, its errors. While it has one, nothing is published.
     */
    val acceptance: StateFlow<DocumentAcceptance.Verdict?>

    /** The hash of the document in force as storage has it, which every adoption is compared against. */
    fun appliedHash(): String?

    /**
     * Takes a copy off a machine, stored exactly as read, if it hashes to [expectedRemoteHash] and
     * the document in force is still [expectedLocalHash]. See [com.x1f4r.legioncontrol.data.ConfigStore.adoptFromMachine].
     */
    fun adoptFromMachine(text: String, expectedRemoteHash: String?, expectedLocalHash: String?, machineName: String): SetupOutcome

    /** A copy of the same setup that neither side descends from, from the verified bytes of the machine [machineId]. */
    fun recordDivergence(machineId: String, machineName: String, reason: String, theirText: String)

    /** The machine [machineId] carrying another setup entirely, from its verified bytes. */
    fun recordIdentityClash(machineId: String, machineName: String, reason: String, theirText: String)

    fun clearDivergence()

    fun clearIdentityClash()

    /** Takes the other side of a divergence wholesale, if this device's copy is still the one it was built from. */
    fun adoptDivergent(divergence: SetupDivergence): SetupOutcome

    /**
     * Puts the result of Merge or Keep mine in force, if this device's copy is still [expectedHead].
     *
     * Not `--replace`: a revision whose ancestry contains the other machine's hash is one that every
     * machine on either branch accepts as an ordinary fast-forward, so nothing has to be overridden.
     */
    fun applyMerge(mergedText: String, expectedHead: String?): SetupOutcome

    /** Drops this device's setup and follows the other one, if this device's copy is still the one it was asked about. */
    fun adoptOtherSetup(clash: SetupIdentityClash): SetupOutcome

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

    /** One client per machine the gate's configuration describes, in the order it lists them. */
    fun clients(launch: LaunchGate.Effective): List<MachineClient>
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

    is AgentFailure.BadRequest -> LinkState.Misconfigured(detail ?: summary)
    is AgentFailure.KeyUnavailable -> LinkState.PhoneKeyUnavailable(summary)
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
        if (dispatch.mayHaveRun) LinkState.Unsettled(summary) else LinkState.Offline(sentence)

    is AgentFailure.NoTunnel -> LinkState.Offline(sentence, noTunnel = true)
    else -> LinkState.Offline(sentence)
}
