package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentAction
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.DoctorReport
import com.x1f4r.legioncontrol.agent.EndpointDiagnosis
import com.x1f4r.legioncontrol.agent.InterruptRequest
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.OpKind
import com.x1f4r.legioncontrol.agent.OpSummary
import com.x1f4r.legioncontrol.agent.OperationIntent
import com.x1f4r.legioncontrol.agent.OperationRecord
import com.x1f4r.legioncontrol.agent.PolicyPatch
import com.x1f4r.legioncontrol.agent.ServiceStatus
import com.x1f4r.legioncontrol.agent.TrackedState
import com.x1f4r.legioncontrol.agent.UpdateRequest
import com.x1f4r.legioncontrol.agent.WakeHelper
import com.x1f4r.legioncontrol.agent.WakePlan
import com.x1f4r.legioncontrol.agent.helperFailure
import com.x1f4r.legioncontrol.agent.planWake
import com.x1f4r.legioncontrol.agent.resolveSystem
import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.SiteMatch
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.UUID

/**
 * One machine, as the screen knows it, and the only thing that talks to that machine's agent.
 *
 * It polls while the screen is on and not one moment longer. A phone is not a desk: there is no
 * service, no worker, no alarm and nothing left running in the background. When the app is put away
 * these stop dead, which is why [startPolling] and [stopPolling] are driven straight from the
 * activity's resume and pause rather than from a composable that might outlive the visible screen.
 *
 * Changes are the exception, and deliberately so: an update the user asked for runs on the
 * application scope through [OperationCenter], because locking the phone is not withdrawing the
 * request. What stops is the asking, not the doing.
 */
@Stable
class MachineModel(
    private val client: MachineClient,
    private val settings: RememberedSettings,
    private val scope: CoroutineScope,
    private val operations: OperationCenter,
    /** The setup this machine is one of the carriers of. */
    private val setup: SetupSync,
    /** Everything about the fleet this machine needs to know: siblings, sites, where we are. */
    private val fleet: FleetContext,
) {
    val machine: Machine get() = client.machine
    val serviceSetup = ServiceSetupModel(client, scope) { refreshNow() }
    val serviceSetupUnavailable: String? get() = when {
        !speaksV3 -> "Agent 3 is required."
        status?.agent?.restrictedSession == true -> "Service setup requires an administrator SSH key."
        else -> null
    }

    /** Which button is busy, as an identity rather than as a sentence. */
    enum class Task { WAKE, SLEEP, BOOT, UPDATE, RESTART, AUTO_UPDATE, TRUST_KEY, RUN, POLICY, DOCTOR, AGENT }

    /** Questions asked before something interrupts work or reboots the machine. */
    sealed interface Dialog {
        val title: String
        val message: String
        val confirmLabel: String

        /** A second, quieter choice on the same sheet: the same thing, but when the machine is idle. */
        val alternativeLabel: String? get() = null

        data class ConfirmBoot(
            val target: MachineSystem,
            val machineName: String,
            val busyReason: String?,
        ) : Dialog {
            override val title get() = "Boot into ${target.name}?"
            override val message get() = busyReason?.let {
                "$machineName looked busy at the last reading ($it). The switch is checked against " +
                    "the machine as it is now, and held back if work is still running."
            } ?: "$machineName reboots now and comes back up in ${target.name}. Nothing here calls " +
                "it done until it answers as ${target.name}."
            override val confirmLabel get() = "Reboot now"
            override val alternativeLabel get() = "When it is idle"
        }

        data class OfferForceBoot(val target: MachineSystem, override val message: String) : Dialog {
            override val title get() = "Switch to ${target.name} anyway?"
            override val confirmLabel get() = "Switch anyway"
        }

        data class ConfirmRestart(
            val service: ServiceStatus,
            val machineName: String,
            val busyReason: String?,
        ) : Dialog {
            override val title get() = "Restart ${service.displayName}?"
            override val message get() = busyReason?.let {
                "${service.displayName} stops and starts again. $machineName looked busy at the " +
                    "last reading ($it), so the restart is checked against the machine as it is " +
                    "now, and held back if work is still running."
            } ?: "${service.displayName} stops and starts again. Anything in flight is dropped."
            override val confirmLabel get() = "Restart"
            override val alternativeLabel get() = "When it is idle"
        }

        data class OfferForceRestart(
            val service: ServiceStatus,
            override val message: String,
        ) : Dialog {
            override val title get() = "Restart ${service.displayName} anyway?"
            override val confirmLabel get() = "Restart anyway"
        }

        data class OfferForceUpdate(
            val service: ServiceStatus,
            override val message: String,
        ) : Dialog {
            override val title get() = "Update ${service.displayName} anyway?"
            override val confirmLabel get() = "Update anyway"
        }

        data class ConfirmSleep(val machineName: String, val busyReason: String?) : Dialog {
            override val title get() = "Put $machineName to sleep?"
            override val message get() = busyReason?.let {
                "$machineName looked busy at the last reading ($it). Sleep is checked against the " +
                    "machine as it is now, and held back if work is still running."
            } ?: "$machineName suspends to memory and stops answering. Waking it needs this device " +
                "on its network, or a helper that is."
            override val confirmLabel get() = "Sleep now"
            override val alternativeLabel get() = "When it is idle"
        }

        data class OfferForceSleep(override val message: String) : Dialog {
            override val title get() = "Put it to sleep anyway?"
            override val confirmLabel get() = "Sleep anyway"
        }

        data class ConfirmRun(val action: AgentAction, override val message: String) : Dialog {
            override val title get() = "${action.displayName}?"
            override val confirmLabel get() = "Run it"
        }

        data class OfferForceRun(val action: AgentAction, override val message: String) : Dialog {
            override val title get() = "${action.displayName} anyway?"
            override val confirmLabel get() = "Run it anyway"
        }

        /**
         * A host key nobody has vouched for yet, or one that has changed.
         *
         * Both are questions and neither is ever answered quietly. This app does not accept the
         * first key it is shown: on that one connection an attacker in the middle has a free hand,
         * and the pin is only worth anything if it started from something somebody checked.
         */
        data class ConfirmTrustHostKey(
            val address: String,
            val keyBlob: String,
            val fingerprint: String,
            val isFirstContact: Boolean,
            val canApprove: Boolean,
            val detail: String?,
            val trust: com.x1f4r.legioncontrol.data.HostTrustSnapshot,
            val suggestedSystems: List<com.x1f4r.legioncontrol.data.HostIdentitySystem>,
        ) : Dialog {
            override val title
                get() = if (isFirstContact) "Trust this host key?" else "Trust the new host key?"
            override val message get() = listOfNotNull(
                detail?.takeIf { it.isNotBlank() },
                if (isFirstContact) {
                    "Check it against `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` run on the " +
                        "machine itself. Nothing is trusted for this address until you say so."
                } else {
                    "Accept this only if you know why the key is new. A system that has never been " +
                        "trusted on this address explains it, and nothing else does. The keys " +
                        "already trusted stay."
                },
            ).joinToString(" ")
            override val confirmLabel get() = "Trust it"
        }

        /** Installing a signed control agent, which is the one action that replaces code. */
        data class ConfirmAgentInstall(
            val machineName: String,
            val version: String,
            val current: String?,
        ) : Dialog {
            override val title get() = "Install control agent $version?"
            override val message
                get() = "This sends the signed agent bundle this app carries to $machineName and " +
                    (current?.let { "replaces $it. " } ?: "installs it. ") +
                    "The bundle's signature has already been checked against the release key pinned " +
                    "in this app; the machine checks it again before it swaps anything."
            override val confirmLabel get() = "Install it"
        }

        /** Waking a helper so that it can wake something else. Never done without being asked. */
        data class ConfirmWakeHelper(val helperName: String, val targetName: String) : Dialog {
            override val title get() = "Wake $helperName first?"
            override val message
                get() = "$targetName can only be woken by a machine on its network, and " +
                    "$helperName is one. Waking it starts a second machine, which is why this is a " +
                    "separate decision."
            override val confirmLabel get() = "Wake $helperName"
        }
    }

    private var pollJob: Job? = null

    var link: LinkState by mutableStateOf(LinkState.Unknown)
        private set
    var status: AgentStatus? by mutableStateOf(null)
        private set
    var remembered: Map<String, RememberedSystem> by mutableStateOf(
        settings.load(client.machine.id, client.machine.systems.map { it.id }),
    )
        private set
    var lastChecked: Long? by mutableStateOf(null)
        private set
    var route: Route? by mutableStateOf(null)
        private set
    var isRefreshing: Boolean by mutableStateOf(false)
        private set
    var isRefreshingVisibly: Boolean by mutableStateOf(false)
        private set

    /** Non-null while a user-initiated action is being sent. Doubles as "controls are disabled". */
    var activity: String? by mutableStateOf(null)
        private set
    var task: Task? by mutableStateOf(null)
        private set
    var subject: String? by mutableStateOf(null)
        private set

    private var messageGeneration = 0L
    private var lastMessage by mutableStateOf(MachineMessage())
    private var operationRecords by mutableStateOf(operations.records.value)

    init {
        scope.launch {
            operations.records.collect { operationRecords = it }
        }
    }

    var statusLine: String
        get() = lastMessage.resolve(operationRecords).line
        private set(value) { lastMessage = lastMessage.copy(line = value, operationId = null) }
    var statusIsError: Boolean
        get() = lastMessage.resolve(operationRecords).isError
        private set(value) { lastMessage = lastMessage.copy(isError = value) }
    var statusDetail: String?
        get() = lastMessage.resolve(operationRecords).detail
        private set(value) { lastMessage = lastMessage.copy(detail = value) }
    var dialog: Dialog? by mutableStateOf(null)

    /** What the last diagnostics run found, per address. Empty until somebody asks for it. */
    var diagnostics: List<EndpointDiagnosis> by mutableStateOf(emptyList())
        private set
    var diagnosing: Boolean by mutableStateOf(false)
        private set
    var doctorReport: DoctorReport? by mutableStateOf(null)
        private set

    /** The agent's own log, when it has been asked for. */
    var recentLog: List<String> by mutableStateOf(emptyList())
        private set

    /** A diagnostic bundle waiting to be shared. */
    var bundleText: String? by mutableStateOf(null)
        private set

    /** What this machine says about waking, worked out from where this device is standing. */
    var wakeExplanation: String by mutableStateOf("")
        private set

    /** A helper that could itself be woken, offered as a separate press after a failed wake. */
    var wakeableHelper: WakeHelper? by mutableStateOf(null)
        private set

    var knownServices: List<RememberedService> by mutableStateOf(settings.services(client.machine.id))
        private set

    var now: Long by mutableStateOf(System.currentTimeMillis())
        private set

    /**
     * Set the moment a reboot is handed off, so a dead link reads as restarting rather than gone.
     *
     * It changes the words and nothing else. Whether the boot actually happened is settled by the
     * machine answering as the system that was asked for, in [OperationCenter], and never here.
     */
    private var rebootTarget: MachineSystem? by mutableStateOf(null)
    private var rebootUntil: Long = 0L
    private var sleepUntil: Long = 0L

    /** The setup hashes already asked about, so a diverged machine is not interrogated on a loop. */
    private val askedAboutSetup = mutableSetOf<String>()

    val isWorking: Boolean get() = activity != null

    fun isBusyWith(task: Task, subject: String? = null): Boolean =
        this.task == task && this.subject == subject

    val currentSystem: MachineSystem?
        get() = (link as? LinkState.Online)?.system

    val isAwake: Boolean
        get() = link is LinkState.Online || link is LinkState.AgentMissing

    val needsKeyAuthorisation: Boolean
        get() = link is LinkState.NeedsKeyAuthorisation

    val hostKeyChanged: LinkState.HostKeyChanged?
        get() = link as? LinkState.HostKeyChanged

    val misconfigured: LinkState.Misconfigured?
        get() = link as? LinkState.Misconfigured

    /** True when the last reading left an outcome hanging rather than proving anything. */
    val isUnsettled: Boolean get() = link is LinkState.Unsettled

    val rebootInProgress: MachineSystem?
        get() = rebootTarget?.takeIf { now < rebootUntil }

    val busyReason: String?
        get() = status?.busy?.takeIf { it.isBusy || it.isUnknown }?.summary

    // Read Compose state here so a skipped child row observes the same refresh as the main pane.
    val abilities: AgentAbilities
        get() = status?.takeIf { it.ok != false }?.let {
            AgentAbilities.of(it).copy(verbs = client.abilities.verbs)
        } ?: client.abilities

    /** Whether this machine's agent speaks the contract this build was written against. */
    val speaksV3: Boolean get() = abilities.speaksV3

    val canWake: Boolean get() = machine.wake != null

    /** What this machine's last handover of the setup did, while that is still news. */
    val setupNote: String? get() = setup.noteFor(machine.id, now)

    val actions: List<AgentAction> get() = status?.actions.orEmpty().filter { !it.id.isNullOrBlank() }

    /** Every change this app has asked this machine for, newest first. */
    val history: List<OperationRecord> get() = operations.forMachine(machine.id)

    val pending: List<OperationRecord> get() = operations.pendingFor(machine.id)

    /** What the agent says it is running or has queued, which may include other devices' work. */
    val agentOperations: List<OpSummary>
        get() = status?.operations?.let { it.running.orEmpty() + it.queued.orEmpty() }.orEmpty()

    val recentAgentOperations: List<OpSummary>
        get() = status?.operations?.recent.orEmpty().take(5)

    val bootChoices: List<MachineSystem>
        get() {
            val running = currentSystem ?: return emptyList()
            val others = machine.systems.filter { it.id != running.id }
            val offered = status?.bootTargets ?: return others
            val ids = offered.mapNotNull { it.id }.toSet()
            return others.filter { it.id in ids }
        }

    fun service(id: String): ServiceStatus? =
        status?.effectiveServices?.firstOrNull { it.id == id }

    fun autoUpdateValue(system: MachineSystem): Boolean? {
        if (currentSystem?.id == system.id) {
            status?.updates?.automatic?.let { return it }
            status?.autoUpdate?.let { return it }
        }
        return remembered[system.id]?.autoUpdate
    }

    // MARK: - Polling

    fun startPolling() {
        if (pollJob == null) {
            pollJob = scope.launch {
                while (isActive) {
                    refresh(userInitiated = false)
                    delay(pollInterval())
                }
            }
        }
    }

    /**
     * Fifteen seconds, except while the link is blocked on something the user has to do somewhere
     * else. A rejected key cannot become an accepted one between two polls, and each attempt is one
     * more failed login in the machine's auth log, so those states back off.
     */
    private fun pollInterval(): Long = when (link) {
        is LinkState.NeedsKeyAuthorisation, is LinkState.HostKeyChanged, is LinkState.Misconfigured ->
            BLOCKED_POLL_INTERVAL_MS

        else -> POLL_INTERVAL_MS
    }

    fun stopPolling() {
        pollJob?.cancel()
        pollJob = null
    }

    fun tick(now: Long) {
        this.now = now
    }

    fun refreshNow() {
        if (isWorking) return
        messageGeneration++
        scope.launch { refresh(userInitiated = true) }
    }

    private suspend fun refresh(userInitiated: Boolean, duringAction: Boolean = false) {
        if (isRefreshing) {
            if (userInitiated) isRefreshingVisibly = true
            return
        }
        if (!duringAction && isWorking) return
        isRefreshing = true
        val startedGeneration = messageGeneration
        var messageBeforeReply: MachineMessage? = null
        var replyGeneration = startedGeneration
        if (userInitiated) {
            statusLine = "Checking ${machine.name}."
            statusIsError = false
            statusDetail = null
            isRefreshingVisibly = true
        }
        try {
            val result = client.status()
            replyGeneration = messageGeneration
            if (lastMessage.operationId != null || duringAction || startedGeneration != replyGeneration) {
                messageBeforeReply = lastMessage
            }
            now = System.currentTimeMillis()
            lastChecked = now
            route = client.route
            result.fold(
                onSuccess = { reply ->
                    if (reply.ok == false) {
                        status = reply
                        link = LinkState.Offline(reply.message ?: "the agent reported a failure")
                        statusLine = "The control agent reported a problem."
                        statusIsError = true
                        statusDetail = reply.message ?: reply.error
                    } else {
                        applyReading(reply)
                        reconcileSetup(reply)
                        if (userInitiated && startedGeneration == messageGeneration) {
                            statusLine = "Status refreshed."
                            statusIsError = false
                            statusDetail = null
                        }
                    }
                },
                onFailure = { error -> describeFailure(error) },
            )
        } finally {
            if (messageGeneration == replyGeneration) messageBeforeReply?.let { lastMessage = it }
            isRefreshing = false
            isRefreshingVisibly = false
        }
    }

    private fun applyReading(reply: AgentStatus) {
        val system = machine.resolveSystem(reply.systemId, reply.systemName, reply.osName)
            ?: currentSystem
        if (system == null) {
            status = reply
            link = LinkState.Offline("the agent did not say which system it is")
            return
        }
        status = reply
        link = LinkState.Online(system)
        rebootTarget = null
        sleepUntil = 0L

        // The transport has already remembered the authenticated route that produced this reading.
        // Keep it even when the system changed: forgetting here would discard the new route rather
        // than the old one the boot request used.

        rememberServices(reply)
        settings.save(
            machine.id,
            system.id,
            reply.updates?.automatic ?: reply.autoUpdate ?: remembered[system.id]?.autoUpdate,
            System.currentTimeMillis(),
        )
        remembered = settings.load(machine.id, machine.systems.map { it.id })

        // Anything this app asked for and never heard the end of is now followed up.
        operations.pendingFor(machine.id).forEach { operations.follow(client, it.id) }
    }

    /**
     * R1: what to do about the setup copy this machine is holding.
     *
     * Every peer edits, so every poll is also a reconciliation. Only two things happen without
     * asking, and both are along strict descent: push when this device's copy was made from theirs,
     * fetch when theirs was made from this device's. Everything else stops at a person.
     */
    private suspend fun reconcileSetup(reply: AgentStatus) {
        val note = setup.reconcile(
            client = client,
            reportedHash = reply.controller?.hash,
            speaksV3 = reply.speaksV3,
            asked = askedAboutSetup,
        )
        if (note != null) setup.remember(machine.id, note)
    }

    private fun rememberServices(reply: AgentStatus) {
        val services = reply.effectiveServices
            .filter { !it.id.isNullOrBlank() }
            .map { RememberedService(it.id.orEmpty(), it.displayName) }
        if (services.isEmpty()) return
        val merged = services + knownServices.filterNot { known -> services.any { it.id == known.id } }
        if (merged == knownServices) return
        knownServices = merged
        settings.saveServices(machine.id, merged)
    }

    private fun describeFailure(error: Throwable) {
        val state = error.toLinkState()
        link = state
        status = null
        statusDetail = error.rawDetail
        when (state) {
            is LinkState.Offline -> {
                // Silence alone cannot establish whether a sleep or reboot actually happened.
                val reboot = rebootInProgress
                when {
                    reboot != null -> {
                        statusLine = "Waiting for ${reboot.name} to come back."
                        statusIsError = false
                    }

                    System.currentTimeMillis() < sleepUntil -> {
                        statusLine = "Sleep was requested; ${machine.name} is not answering. Its outcome is not confirmed."
                        statusIsError = false
                        statusDetail = null
                    }

                    else -> {
                        statusLine = error.sentence
                        statusIsError = true
                    }
                }
            }

            is LinkState.Unsettled -> {
                statusLine = "${machine.name} answered and then stopped part way. " +
                    "Nothing is being assumed about what ran."
                statusIsError = true
            }

            else -> {
                statusLine = error.sentence
                statusIsError = true
            }
        }
    }

    // MARK: - Waking

    /**
     * Wakes this machine, from here if that is possible and through a helper if it is not.
     *
     * W2. The order is fixed and the failures are explained rather than retried: a packet sent at
     * the wrong house is not a packet that will work if sent again, and a helper that is itself
     * asleep is a second machine somebody has to decide to start.
     */
    private var wakeTarget: MachineSystem? = null

    fun wake(target: MachineSystem? = null) {
        if (isWorking) return
        wakeTarget = target
        val plan = planWake(
            target = machine,
            sites = fleet.sites,
            siteMatch = fleet.siteMatch,
            machines = fleet.machines,
            localAddresses = fleet.localAddresses,
        )
        when (plan) {
            WakePlan.NotConfigured -> {
                statusLine = "${machine.name} has no wake configuration."
                statusIsError = true
            }

            is WakePlan.Impossible -> {
                statusLine = "Cannot wake ${machine.name} from here."
                statusIsError = true
                statusDetail = plan.reasons.joinToString(" ")
                wakeableHelper = null
            }

            is WakePlan.Direct -> runTask(Task.WAKE, "Waking ${machine.name}") {
                statusLine = "Wake packet sent. ${plan.why} Waiting for ${machine.name} to answer."
                val problem = client.sendWakePacket(plan.broadcasts, plan.prefixes)
                if (problem != null) {
                    statusLine = "The wake packet could not be sent."
                    statusIsError = true
                    statusDetail = problem
                }
                val answered = problem == null && awaitWake()
                if (!answered && machine.wake?.helpers.orEmpty().isNotEmpty()) {
                    statusLine = "Direct wake did not produce an authenticated reply. Trying the configured helpers."
                    wakeViaHelpers(machine.wake?.helpers.orEmpty())
                }
            }

            is WakePlan.ViaHelpers -> runTask(Task.WAKE, "Waking ${machine.name}") {
                statusLine = "${plan.why} Asking ${plan.helpers.size} helper" +
                    (if (plan.helpers.size == 1) "" else "s") + "."
                wakeViaHelpers(plan.helpers)
            }
        }
    }

    /**
     * Walks the helpers in order.
     *
     * A helper whose reply is lost is not simply tried again, and that distinction matters: a `wol`
     * action is the agent sending a UDP packet, which is the same however many times it happens, but
     * a `command` action is somebody's script and running it twice is running it twice. So an
     * ambiguous general command stops the walk until its recorded outcome is known.
     */
    private suspend fun wakeViaHelpers(helpers: List<WakeHelper>) {
        val attempts = mutableListOf<String>()
        for (helper in helpers) {
            val helperClient = fleet.client(helper.machineId)
            val helperName = fleet.machines.firstOrNull { it.id == helper.machineId }?.name
                ?: helper.machineId
            if (helperClient == null) {
                attempts += "$helperName is not in this setup."
                continue
            }
            if (fleet.helperAwake(helper.machineId) == false) {
                attempts += "$helperName is asleep or unreachable."
                continue
            }
            statusLine = "Asking $helperName to wake ${machine.name}."
            val operationId = UUID.randomUUID().toString()
            val helperAction = fleet.action(helper.machineId, helper.actionId)
            val outcome = helperClient.run(
                actionId = helper.actionId,
                request = InterruptRequest(),
                operationId = operationId,
            )
            val result = outcome.getOrNull()
            if (result?.effectiveAction == "ran") {
                statusLine = "$helperName sent the packet. Waiting for ${machine.name}."
                statusDetail = result.effectiveOutput
                if (awaitWake()) return
                attempts += "$helperName sent the packet, but ${machine.name} did not answer."
                continue
            }

            val failure = outcome.exceptionOrNull()
            if (failure?.dispatchStage == com.x1f4r.legioncontrol.agent.DispatchStage.AMBIGUOUS ||
                (result != null && (result.op?.state in listOf("queued", "running") || result.effectiveAction == null))) {
                val settled = if (helperClient.abilities.speaksV3) {
                    helperClient.operation(operationId, waitSeconds = 20).getOrNull()
                } else {
                    null
                }
                when {
                    settled?.result?.action == "ran" -> {
                        statusLine = "$helperName sent the packet. Waiting for ${machine.name}."
                        statusDetail = settled.result.output
                        if (awaitWake()) return
                        attempts += "$helperName sent the packet, but ${machine.name} did not answer."
                    }

                    settled?.isFinished == true -> attempts +=
                        "$helperName's wake action finished as ${settled.result?.action ?: "failed"}."

                    helperAction?.isIdempotent == true -> attempts +=
                        "$helperName may have sent its idempotent wake packet, but the result is not known."

                    else -> {
                        statusLine = "$helperName may have run \"${helper.actionId}\"."
                        statusIsError = true
                        statusDetail = "Its operation $operationId could not be reconciled. A general " +
                            "helper command is not run again and no later helper is tried until its " +
                            "outcome is known."
                        return
                    }
                }
                continue
            }

            if (failure != null) {
                attempts += "$helperName ${failure.sentence.removeSuffix(".")}."
            } else {
                attempts += "$helperName ran \"${helper.actionId}\" and it " +
                    "${result?.effectiveAction ?: "did not report success"}."
            }
        }

        if (attempts.isNotEmpty()) {
            // Even a lost reply may have sent a packet, so it is worth looking before giving up.
            if (awaitWake()) return
            val failure = helperFailure(machine, attempts, fleet.machines, helpers)
            statusLine = "Cannot wake ${machine.name} from here."
            statusIsError = true
            statusDetail = failure.reasons.joinToString(" ")
            wakeableHelper = helpers.firstOrNull { helper ->
                fleet.machines.firstOrNull { it.id == helper.machineId }?.wake != null
            }
        }
    }

    /** Whether the machine started answering. A packet leaving is not a machine waking. */
    private suspend fun awaitWake(): Boolean {
        val answered = client.waitForWake()
        if (answered) {
            statusLine = "${machine.name} is awake."
            statusIsError = false
            rebootTarget = null
            sleepUntil = 0L
            refreshQuietly()
            wakeTarget?.let { target ->
                if (currentSystem?.id != target.id) {
                    if (currentSystem != null) requestBoot(target)
                    else {
                        statusLine = "${machine.name} answered, but its running system is unknown."
                        statusIsError = true
                    }
                }
            }
            wakeTarget = null
            return true
        }
        statusLine = "${machine.name} has not answered yet."
        statusIsError = true
        statusDetail = client.wakeProbeAddress?.let { "Nothing on $it within the wait." }
        return false
    }

    /** Offers to wake the helper that could have woken this machine. Always a separate decision. */
    fun requestWakeHelper() {
        val helper = wakeableHelper ?: return
        val name = fleet.machines.firstOrNull { it.id == helper.machineId }?.name ?: helper.machineId
        dialog = Dialog.ConfirmWakeHelper(name, machine.name)
    }

    fun wakeHelper() {
        val helper = wakeableHelper ?: return
        fleet.wakeMachine(helper.machineId)
        wakeableHelper = null
    }

    // MARK: - Changes

    fun requestSleep() {
        dialog = Dialog.ConfirmSleep(machine.name, busyReason)
    }

    fun sleep(force: Boolean = false, whenIdle: Boolean = false) {
        val request = InterruptRequest(force = force, whenIdle = whenIdle, expires = QUEUE_EXPIRY)
        change(
            task = Task.SLEEP,
            label = "Putting ${machine.name} to sleep",
            intent = OperationIntent(
                kind = OpKind.SLEEP,
                force = force,
                whenIdle = whenIdle,
                expires = QUEUE_EXPIRY.takeIf { whenIdle },
            ),
            subjectName = machine.name,
        ) { id ->
            client.sleep(request, id)
        }
        sleepUntil = System.currentTimeMillis() + SLEEP_GRACE_MS
    }

    fun requestBoot(target: MachineSystem) {
        dialog = Dialog.ConfirmBoot(target, machine.name, busyReason)
    }

    fun boot(target: MachineSystem, force: Boolean = false, whenIdle: Boolean = false) {
        val request = InterruptRequest(force = force, whenIdle = whenIdle, expires = QUEUE_EXPIRY)
        change(
            task = Task.BOOT,
            label = "Switching to ${target.name}",
            intent = OperationIntent(
                kind = OpKind.BOOT,
                target = target.id,
                force = force,
                whenIdle = whenIdle,
                expires = QUEUE_EXPIRY.takeIf { whenIdle },
            ),
            subjectName = target.name,
            subjectId = target.id,
        ) { id ->
            client.boot(target.id, request, noReboot = false, operationId = id)
        }
        rebootTarget = target
        rebootUntil = System.currentTimeMillis() + REBOOT_GRACE_MS
        // W4. The route that answered belonged to the system that is on its way out. Prefer the
        // requested system's endpoint, then a LAN endpoint only when this device is on that site.
        val onSite = machine.siteId?.let { fleet.siteMatch.site?.id == it }
            ?: machine.wake?.lanPrefix?.let { prefix -> fleet.localAddresses.any { it.startsWith(prefix) } }
            ?: false
        fleet.prepareRoute(machine.id, target.id, onSite)
    }

    fun requestRestart(service: ServiceStatus) {
        dialog = Dialog.ConfirmRestart(service, machine.name, busyReason)
    }

    fun restart(service: ServiceStatus, force: Boolean = false, whenIdle: Boolean = false) {
        val request = InterruptRequest(
            force = force,
            whenIdle = whenIdle,
            expires = QUEUE_EXPIRY,
            detach = speaksV3,
        )
        change(
            task = Task.RESTART,
            label = "Restarting ${service.displayName}",
            intent = OperationIntent(
                kind = OpKind.RESTART,
                service = service.id,
                force = force,
                whenIdle = whenIdle,
                expires = QUEUE_EXPIRY.takeIf { whenIdle },
            ),
            subjectName = service.displayName,
            subjectId = service.id,
        ) { id ->
            client.restart(serviceArgument(service.id), request, id)
        }
    }

    /**
     * Asks for an update.
     *
     * Not forced and not scheduled: under contract 3 a manual request ignores the schedule, the
     * pause and the maintenance windows outright, and is still checked against whether the machine
     * is working. The ordinary Update button therefore no longer has to reach for force just
     * because a machine does not update itself.
     */
    fun requestUpdate(service: ServiceStatus) = update(service)

    fun update(service: ServiceStatus, force: Boolean = false, whenIdle: Boolean = false) {
        val request = UpdateRequest(
            serviceId = serviceArgument(service.id),
            force = force,
            whenIdle = whenIdle,
            expires = QUEUE_EXPIRY,
            // An update is the one change long enough to outlive a phone's attention, so on a v3
            // agent it always detaches and is followed by its id.
            detach = speaksV3,
        )
        change(
            task = Task.UPDATE,
            label = "Updating ${service.displayName}",
            intent = OperationIntent(
                kind = OpKind.UPDATE,
                service = service.id,
                force = force,
                whenIdle = whenIdle,
                expires = QUEUE_EXPIRY.takeIf { whenIdle },
            ),
            subjectName = service.displayName,
            subjectId = service.id,
        ) { id ->
            client.update(request, id)
        }
    }

    fun requestRun(action: AgentAction) {
        val confirm = action.confirm?.takeIf { it.isNotBlank() }
        if (confirm == null) runAction(action) else dialog = Dialog.ConfirmRun(action, confirm)
    }

    fun runAction(action: AgentAction, force: Boolean = false, whenIdle: Boolean = false) {
        val id = action.id ?: return
        val request = InterruptRequest(
            force = force,
            whenIdle = whenIdle,
            expires = QUEUE_EXPIRY,
            detach = speaksV3,
        )
        change(
            task = Task.RUN,
            label = action.displayName,
            intent = OperationIntent(
                kind = OpKind.RUN,
                actionId = id,
                force = force,
                whenIdle = whenIdle,
                expires = QUEUE_EXPIRY.takeIf { whenIdle },
            ),
            subjectName = action.displayName,
            subjectId = id,
        ) { operationId ->
            client.run(id, request, operationId)
        }
    }

    fun setAutoUpdate(enabled: Boolean, on: MachineSystem, serviceId: String? = null) {
        if (currentSystem?.id != on.id) return
        change(
            task = Task.AUTO_UPDATE,
            label = "Saving the update schedule",
            intent = OperationIntent(kind = OpKind.UPDATE, service = serviceId),
            subjectName = serviceId ?: on.name,
            subjectId = serviceId ?: on.id,
            track = false,
        ) { id ->
            client.setAutoUpdate(enabled, serviceId, id)
        }
    }

    fun pauseUpdates(duration: String, serviceId: String? = null) {
        change(
            task = Task.POLICY,
            label = "Pausing updates",
            intent = OperationIntent(kind = OpKind.UPDATE, service = serviceId),
            subjectName = serviceId ?: machine.name,
            subjectId = serviceId,
            track = false,
        ) { id ->
            client.pauseUpdates(duration, serviceId, id)
        }
    }

    fun resumeUpdates(serviceId: String? = null) {
        change(
            task = Task.POLICY,
            label = "Resuming updates",
            intent = OperationIntent(kind = OpKind.UPDATE, service = serviceId),
            subjectName = serviceId ?: machine.name,
            subjectId = serviceId,
            track = false,
        ) { id ->
            client.resumeUpdates(serviceId, id)
        }
    }

    fun writePolicy(patch: PolicyPatch, serviceId: String? = null) {
        change(
            task = Task.POLICY,
            label = "Saving the update policy",
            intent = OperationIntent(kind = OpKind.UPDATE, service = serviceId),
            subjectName = serviceId ?: machine.name,
            subjectId = serviceId,
            track = false,
        ) { id ->
            client.writePolicy(patch, serviceId, id)
        }
    }

    /** Reads the canonical server record before withdrawing another peer's queued request. */
    fun cancelRemote(summary: OpSummary) {
        val id = summary.id ?: return
        if (summary.state != "queued") return
        runTask(Task.RUN, "Cancelling queued change") {
            val current = client.operation(id, 0).getOrElse { report(it); return@runTask }
            if (current.state != "queued") {
                statusLine = "That change is no longer queued."
                refreshQuietly()
                return@runTask
            }
            client.cancel(id).fold(
                onSuccess = { result -> statusLine = result.op?.result?.message ?: result.message ?: "Change $id: ${result.effectiveAction ?: "outcome unknown"}." },
                onFailure = { report(it) },
            )
            refreshQuietly()
        }
    }

    var operationDetails: String? by mutableStateOf(null)
        private set

    fun showOperation(id: String) {
        val saved = operations.get(id)
        if (saved != null) operationDetails = operationDetails(saved)
        if (!speaksV3 && saved != null) return
        runTask(Task.DOCTOR, "Reading change details") {
            client.operation(id, 0).fold(
                onSuccess = { record -> operationDetails = operationDetails(saved, record) },
                onFailure = {
                    if (saved == null) report(it)
                    else statusLine = "Showing saved change details. ${machine.name} could not provide a newer record."
                },
            )
        }
    }

    fun dismissOperationDetails() { operationDetails = null }

    fun cancel(record: OperationRecord) {
        if (!record.isCancellable) return
        messageGeneration++
        lastMessage = MachineMessage.forOperation(record)
        operations.cancel(client, record)
    }

    /** Sends a change again, but only one that is known not to have happened. */
    fun retry(record: OperationRecord) {
        if (!record.retryable) return
        messageGeneration++
        lastMessage = MachineMessage.forOperation(record)
        val intent = record.intent
        operations.retry(client, record) { id ->
            when (intent.kind) {
                OpKind.UPDATE -> client.update(
                    UpdateRequest(
                        serviceId = intent.service,
                        force = intent.force,
                        whenIdle = intent.whenIdle,
                        expires = intent.expires,
                        detach = speaksV3,
                    ),
                    id,
                )

                OpKind.RESTART -> client.restart(
                    intent.service,
                    InterruptRequest(intent.force, intent.whenIdle, intent.expires, speaksV3),
                    id,
                )

                OpKind.BOOT -> client.boot(
                    intent.target.orEmpty(),
                    InterruptRequest(intent.force, intent.whenIdle, intent.expires),
                    noReboot = false,
                    operationId = id,
                )

                OpKind.SLEEP -> client.sleep(
                    InterruptRequest(intent.force, intent.whenIdle, intent.expires),
                    id,
                )

                OpKind.RUN -> client.run(
                    intent.actionId.orEmpty(),
                    InterruptRequest(intent.force, intent.whenIdle, intent.expires, speaksV3),
                    id,
                )

                else -> Result.failure(IllegalStateException("that change cannot be sent again"))
            }
        }
    }

    // MARK: - Diagnostics

    fun runDiagnostics() {
        if (diagnosing) return
        diagnosing = true
        statusLine = "Checking every address of ${machine.name}."
        statusIsError = false
        scope.launch {
            try {
                diagnostics = client.diagnose()
                val bad = diagnostics.count { !it.status.isGood }
                statusLine = if (bad == 0) {
                    "Every address of ${machine.name} answered."
                } else {
                    "$bad of ${diagnostics.size} addresses did not answer properly."
                }
                statusIsError = bad > 0
            } finally {
                diagnosing = false
            }
        }
    }

    fun runDoctor(deep: Boolean = false) {
        if (!speaksV3) return
        runTask(Task.DOCTOR, if (deep) "Running deep diagnostics" else "Checking the agent") {
            client.doctor(deep = deep).fold(
                onSuccess = { reply ->
                    doctorReport = reply
                    val failures = reply.failures.size
                    val warnings = reply.warnings.size
                    statusLine = if (failures == 0 && warnings == 0 && reply.ok != false) "Agent checks passed."
                        else "$failures failed checks, $warnings warnings."
                    statusIsError = failures > 0 || reply.ok == false
                    statusDetail = reply.message ?: reply.error
                },
                onFailure = { report(it) },
            )
        }
    }

    fun hideDiagnostics() { diagnostics = emptyList(); doctorReport = null; recentLog = emptyList() }

    fun readLog() {
        scope.launch {
            client.logs(lines = 100, operationId = null).fold(
                onSuccess = { reply ->
                    recentLog = reply.entries.mapNotNull { line ->
                        line.text?.takeIf { it.isNotBlank() }
                    }
                    if (recentLog.isEmpty()) statusLine = "The agent's log is empty."
                },
                onFailure = { report(it) },
            )
        }
    }

    fun buildBundle() {
        scope.launch {
            statusLine = "Collecting diagnostics from ${machine.name}."
            client.bundle().fold(
                onSuccess = {
                    bundleText = it
                    statusLine = "Diagnostics ready to share."
                    statusIsError = false
                },
                onFailure = { report(it) },
            )
        }
    }

    fun clearBundle() {
        bundleText = null
    }

    // MARK: - Trust and the agent

    var trustSettingsAddress: String? by mutableStateOf(null)
        private set
    var trustSettingsSnapshot: com.x1f4r.legioncontrol.data.HostTrustSnapshot? by mutableStateOf(null)
        private set

    fun manageHostIdentities(address: String) {
        trustSettingsAddress = address
        trustSettingsSnapshot = client.hostTrust(address)
    }

    fun closeHostIdentities() { trustSettingsAddress = null; trustSettingsSnapshot = null }

    fun saveHostIdentities(systems: List<com.x1f4r.legioncontrol.data.HostIdentitySystem>) {
        val address = trustSettingsAddress ?: return
        val snapshot = trustSettingsSnapshot ?: return
        try {
            client.configureHostSystems(address, systems, snapshot.revision)
            closeHostIdentities()
            statusLine = "Local host identities saved. New keys still require fingerprint approval."
            refreshNow()
        } catch (failure: Exception) {
            statusLine = failure.message ?: "Could not save host identities."
            statusIsError = true
        }
    }

    fun requestTrustHostKey() {
        val changed = hostKeyChanged ?: return
        if (!changed.canApprove) {
            statusLine = "No additional host key can be trusted for ${changed.address}."
            statusIsError = true
            return
        }
        dialog = Dialog.ConfirmTrustHostKey(
            address = changed.address,
            keyBlob = changed.offeredKeyBlob,
            fingerprint = changed.offeredFingerprint,
            isFirstContact = changed.isFirstContact,
            canApprove = changed.canApprove,
            detail = statusDetail,
            trust = client.hostTrust(changed.address),
            suggestedSystems = machine.systems.map { com.x1f4r.legioncontrol.data.HostIdentitySystem(it.id, it.name) },
        )
    }

    fun trustHostKey(address: String, keyBlob: String, approval: com.x1f4r.legioncontrol.data.HostTrustApproval) {
        runTask(Task.TRUST_KEY, "Trusting the host key") {
            try { client.trustHostKey(address, keyBlob, approval) } catch (failure: Exception) {
                statusLine = failure.message ?: "Could not save host trust."
                statusIsError = true
                return@runTask
            }
            statusLine = "The key for $address is trusted, alongside any already known."
            statusIsError = false
            statusDetail = null
            refresh(userInitiated = false, duringAction = true)
        }
    }

    fun requestAgentInstall(version: String) {
        dialog = Dialog.ConfirmAgentInstall(machine.name, version, abilities.agentVersion)
    }

    fun installAgent() {
        val bundle = fleet.agentBundle
        if (bundle == null) {
            statusLine = "This build carries no signed control agent to install."
            statusIsError = true
            statusDetail = fleet.agentBundleProblem
            return
        }
        change(
            task = Task.AGENT,
            label = "Installing the control agent",
            intent = OperationIntent(kind = OpKind.SELF_UPDATE),
            subjectName = machine.name,
        ) { id ->
            client.installAgent(bundle, id)
        }
    }

    // MARK: - Plumbing

    private fun serviceArgument(id: String?): String? =
        id?.takeIf { status?.reportsServices == true && Contract.isToken(it) }

    /**
     * Every change goes through here.
     *
     * The acknowledgement is written before anything is sent, so the button has its spinner and the
     * footer its sentence in the frame the touch landed rather than whenever the first ssh packet
     * comes back. The record is durable from that same moment, which is what makes the press
     * survivable.
     */
    private fun change(
        task: Task,
        label: String,
        intent: OperationIntent,
        subjectName: String,
        subjectId: String? = null,
        track: Boolean = true,
        dispatch: suspend (String) -> Result<com.x1f4r.legioncontrol.agent.AgentActionResult>,
    ) {
        if (isWorking) return
        messageGeneration++
        this.task = task
        this.subject = subjectId
        activity = label
        statusLine = "$label."
        statusIsError = false
        statusDetail = null

        val record = operations.start(
            client = client,
            intent = intent,
            subjectName = subjectName,
            initiator = fleet.deviceName,
            dispatch = dispatch,
        )

        // The footer resolves this identity from the same records as Changes. Queuing releases
        // the controls but keeps the message attached through cancellation or later completion.
        lastMessage = MachineMessage.forOperation(record)
        scope.launch {
            try {
                while (isActive) {
                    val current = operations.get(record.id) ?: break
                    if (current.isOver || current.state == TrackedState.QUEUED) {
                        offerForce(current)
                        break
                    }
                    if (current.state == TrackedState.AWAITING_DECISION) {
                        offerForce(current)
                        break
                    }
                    delay(FOLLOW_INTERVAL_MS)
                }
            } finally {
                activity = null
                this@MachineModel.task = null
                this@MachineModel.subject = null
                if (track) refreshQuietly()
            }
        }
    }

    /**
     * Offers the forcing version of a change the agent held back.
     *
     * Only after the agent has looked at the machine as it is now and said no, and only for the
     * reasons force actually overrides. Force skips the busy gate and nothing else: it never breaks
     * the operation lock, never overrules a broken configuration, and never turns an unverifiable
     * install into a success.
     */
    private fun offerForce(record: OperationRecord) {
        if (!record.invitesForce) return
        val why = record.reasonSentence ?: record.detail ?: "the machine is busy"
        dialog = when (record.kind) {
            OpKind.UPDATE -> service(record.intent.service.orEmpty())?.let {
                Dialog.OfferForceUpdate(
                    it,
                    "$why. Updating now stops the service first, so that work is lost.",
                )
            }

            OpKind.RESTART -> service(record.intent.service.orEmpty())?.let {
                Dialog.OfferForceRestart(
                    it,
                    "$why. Restarting now interrupts that work and what is in progress is lost.",
                )
            }

            OpKind.BOOT -> machine.system(record.intent.target)?.let {
                Dialog.OfferForceBoot(
                    it,
                    "$why. Switching now interrupts that work and what is in progress is lost.",
                )
            }

            OpKind.SLEEP -> Dialog.OfferForceSleep(
                "$why. Sleeping now interrupts that work and what is in progress is lost.",
            )

            OpKind.RUN -> actions.firstOrNull { it.id == record.intent.actionId }?.let {
                Dialog.OfferForceRun(it, "$why. Running it now interrupts that work.")
            }

            else -> null
        }
    }

    private fun runTask(task: Task, label: String, work: suspend () -> Unit) {
        if (isWorking) return
        messageGeneration++
        this.task = task
        this.subject = null
        activity = label
        statusLine = "$label."
        statusIsError = false
        statusDetail = null
        scope.launch {
            try {
                work()
            } finally {
                activity = null
                this@MachineModel.task = null
                this@MachineModel.subject = null
            }
        }
    }

    private suspend fun refreshQuietly() {
        refresh(userInitiated = false, duringAction = true)
    }

    private fun report(error: Throwable) {
        statusLine = error.sentence
        statusIsError = true
        statusDetail = error.rawDetail
        link = error.toLinkState()
    }

    companion object {
        private const val POLL_INTERVAL_MS = 15_000L
        private const val BLOCKED_POLL_INTERVAL_MS = 60_000L
        private const val REBOOT_GRACE_MS = 180_000L
        private const val SLEEP_GRACE_MS = 600_000L
        private const val FOLLOW_INTERVAL_MS = 500L

        /** How long a queued change waits for an idle moment before it gives up. */
        const val QUEUE_EXPIRY = "4h"
    }
}

/**
 * What one machine needs to know about the rest of the fleet.
 *
 * A machine used to be able to answer every question on its own. Waking through a helper, and
 * routing after a boot, both need more than that: which other machines exist, which sites there are,
 * and where this device is standing. Passing an interface rather than the whole app model keeps that
 * dependency one way.
 */
interface FleetContext {
    val machines: List<Machine>
    val sites: List<SiteConfig>
    val siteMatch: SiteMatch
    val localAddresses: List<String>

    /** What to call this device in an operation record. */
    val deviceName: String

    /** The signed agent bundle this build carries, or null with a reason. */
    val agentBundle: java.io.File?
    val agentBundleProblem: String?

    fun client(machineId: String): MachineClient?

    /** The helper's cached state; null before it has been checked. */
    fun helperAwake(machineId: String): Boolean?

    /** The helper action as last reported, including whether an ambiguous retry is idempotent. */
    fun action(machineId: String, actionId: String): AgentAction?

    /** Drops the remembered route, after a boot or an observed system change. */
    fun forgetRoute(machineId: String)

    fun prepareRoute(machineId: String, systemId: String, onSite: Boolean)

    /** Starts a wake of another machine, for the "wake the helper first" offer. */
    fun wakeMachine(machineId: String)
}

/**
 * The phone's copy of the setup, from the point of view of one machine's poll.
 *
 * It lives outside the machine model because applying a document rebuilds the machine models, and
 * the model that earned a line is thrown away by the very change it made.
 */
interface SetupSync {
    /**
     * R1 for one machine: decide from the status, ask for the ancestry when that is not enough, and
     * act only along strict descent. Returns a line worth showing, or null.
     */
    suspend fun reconcile(
        client: MachineClient,
        reportedHash: String?,
        speaksV3: Boolean,
        asked: MutableSet<String>,
    ): String?

    fun remember(machineId: String, line: String)

    fun noteFor(machineId: String, now: Long): String?
}

/** How old a reading is, in words. Nothing polls while the app is away, so this has to be visible. */
fun freshness(checkedAt: Long?, now: Long): String {
    if (checkedAt == null) return "not checked yet"
    val seconds = ((now - checkedAt) / 1000L).coerceAtLeast(0L)
    return when {
        seconds < 20 -> "checked just now"
        seconds < 90 -> "checked $seconds seconds ago"
        seconds < 5400 -> {
            val minutes = (seconds + 30) / 60
            "checked $minutes minute${if (minutes == 1L) "" else "s"} ago"
        }

        else -> {
            val hours = (seconds + 1800) / 3600
            "checked $hours hour${if (hours == 1L) "" else "s"} ago"
        }
    }
}
