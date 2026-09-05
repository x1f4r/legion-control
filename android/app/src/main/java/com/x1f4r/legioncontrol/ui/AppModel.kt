package com.x1f4r.legioncontrol.ui

import android.content.Context
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.AgentBundle
import com.x1f4r.legioncontrol.agent.AgentAction
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.SetupDecision
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.agent.decideFromMeta
import com.x1f4r.legioncontrol.agent.decideFromStatus
import com.x1f4r.legioncontrol.agent.provenanceOf
import com.x1f4r.legioncontrol.agent.shouldAskForMeta
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.EXAMPLE_CONTROLLER_CONFIG
import com.x1f4r.legioncontrol.agent.SetupChangePreview
import com.x1f4r.legioncontrol.data.SetupMerge
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.endpointCount
import com.x1f4r.legioncontrol.data.machineIds
import com.x1f4r.legioncontrol.data.machineLabel
import com.x1f4r.legioncontrol.data.sameMachine
import com.x1f4r.legioncontrol.data.SetupSource
import com.x1f4r.legioncontrol.data.advisories
import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.SiteMatch
import com.x1f4r.legioncontrol.net.matchSite
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.io.File
import java.util.concurrent.ConcurrentHashMap

/**
 * The app, which is a list of machines, a shared setup, and this phone.
 *
 * Everything that is not about one particular machine lives here: the document that decides which
 * machines there are, this phone's key, the one clock the age lines are read against, and the
 * reconciliation that keeps every peer's copy of the setup in step.
 *
 * The phone is a peer now. It edits the setup, it publishes to every machine that will take it, and
 * it adopts a newer copy when somebody else made one. What keeps that safe is not a rule about who
 * is allowed to write, it is ancestry: this device pushes only where its own copy descends from what
 * the machine holds, adopts only where the machine's copy descends from its own, and stops at a
 * question for everything else.
 */
@Stable
class AppModel(
    context: Context,
    private val services: ControlServices,
    private val remembered: RememberedSettings,
    private val bindings: com.x1f4r.legioncontrol.data.BindingsStore,
    private val notifications: NotificationSettings,
) {
    private val app = context.applicationContext

    /**
     * The scope changes run on.
     *
     * Deliberately not tied to the screen. Polling stops when the app is put away; an update that
     * was asked for does not, because locking the phone is not withdrawing the request.
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var clockJob: Job? = null
    private var polling = false

    private val notifier = AndroidOperationNotifier(app) { notifications.enabled }

    /** Every change this app has asked for, across launches. */
    val operations = OperationCenter(services.operations, scope, notifier)

    var machines: List<MachineModel> by mutableStateOf(emptyList())
        private set

    private var clients: List<MachineClient> = emptyList()

    var publicKey: String by mutableStateOf("")
        private set
    var keyFingerprint: String by mutableStateOf("")
        private set

    var now: Long by mutableStateOf(System.currentTimeMillis())
        private set

    /** Where this device is standing, and how sure that is. */
    var siteMatch: SiteMatch by mutableStateOf(SiteMatch.NoSites)
        private set
    private var addresses: List<String> = emptyList()

    /** The signed agent bundle this build carries, once it has been checked. */
    var agentBundle: File? by mutableStateOf(null)
        private set
    var agentBundleVersion: String? by mutableStateOf(null)
        private set
    var agentBundleProblem: String? by mutableStateOf(null)
        private set

    /** The setup editor, which every peer has. */
    val editor = SetupEditorModel(services, bindings)

    /** What is in the raw configuration field, which is not in force until Apply says so. */
    var configDraft: String by mutableStateOf(services.configText.value)

    /** Whether the user has changed the field since it was last filled from what is in force. */
    private var draftMatchesApplied: Boolean = true

    var configError: String? by mutableStateOf(null)
        private set
    var configNote: String? by mutableStateOf(null)
        private set

    // MARK: - Fetching the setup from a machine

    var fetchHost: String by mutableStateOf(services.setupSource.value?.host.orEmpty())
    var fetchPort: String by mutableStateOf(
        services.setupSource.value?.port?.toString() ?: DEFAULT_SSH_PORT,
    )
    var fetchUser: String by mutableStateOf(services.setupSource.value?.user.orEmpty())

    var fetching: Boolean by mutableStateOf(false)
        private set
    var fetchOutcome: String? by mutableStateOf(null)
        private set

    var fetchDialog: MachineModel.Dialog.ConfirmTrustHostKey? by mutableStateOf(null)

    val canFetch: Boolean
        get() = !fetching && fetchHost.isNotBlank() && fetchUser.isNotBlank()

    private val handovers = mutableStateMapOf<String, Handover>()

    private data class Handover(val line: String, val at: Long)

    /** Machines whose setup this device is currently publishing to, so nothing is pushed twice. */
    private val publishing = ConcurrentHashMap.newKeySet<String>()

    val hasMachines: Boolean get() = machines.isNotEmpty()

    var currentPage: Int by mutableStateOf(0)

    var footerMachine: MachineModel? by mutableStateOf(null)
        private set

    fun showing(model: MachineModel?) {
        if (model != null && model !== footerMachine) footerMachine = model
    }

    val dialogOwner: MachineModel?
        get() = machines.firstOrNull { it.dialog != null }

    val needsKeyAuthorisation: Boolean
        get() = machines.isNotEmpty() && machines.all { it.needsKeyAuthorisation }

    val keyAuthorisationDetail: String?
        get() = machines.firstOrNull { it.needsKeyAuthorisation }?.statusDetail

    /** The setup in force, described in one line. */
    val setupProvenance: SetupProvenance? get() = services.provenance.value

    val divergence get() = services.divergence.value
    val identityClash get() = services.identityClash.value

    val sites: List<SiteConfig> get() = services.config.value?.sites.orEmpty()

    /** Warnings about the setup that are worth showing and are never a reason to refuse it. */
    val advisories: List<String>
        get() = services.config.value?.advisories().orEmpty()

    private val fleet = object : FleetContext {
        override val machines: List<Machine> get() = clients.map { it.machine }
        override val sites: List<SiteConfig> get() = this@AppModel.sites
        override val siteMatch: SiteMatch get() = this@AppModel.siteMatch
        override val localAddresses: List<String> get() = addresses
        override val deviceName: String get() = services.deviceName()
        override val agentBundle: File? get() = this@AppModel.agentBundle
        override val agentBundleProblem: String? get() = this@AppModel.agentBundleProblem

        override fun client(machineId: String): MachineClient? =
            clients.firstOrNull { it.machine.id == machineId }

        override fun helperAwake(machineId: String): Boolean? =
            this@AppModel.machines.firstOrNull { it.machine.id == machineId }?.let { model ->
                model.isAwake.takeIf { model.lastChecked != null }
            }

        override fun action(machineId: String, actionId: String): AgentAction? =
            this@AppModel.machines.firstOrNull { it.machine.id == machineId }
                ?.actions?.firstOrNull { it.id == actionId }

        override fun forgetRoute(machineId: String) = services.forgetRoute(machineId)

        override fun prepareRoute(machineId: String, systemId: String, onSite: Boolean) =
            services.prepareRoute(machineId, systemId, onSite)

        override fun wakeMachine(machineId: String) {
            this@AppModel.machines.firstOrNull { it.machine.id == machineId }?.wake()
        }
    }

    private val setup = object : SetupSync {
        override suspend fun reconcile(
            client: MachineClient,
            reportedHash: String?,
            speaksV3: Boolean,
            asked: MutableSet<String>,
        ): String? = reconcileSetup(client, reportedHash, speaksV3, asked)

        override fun remember(machineId: String, line: String) {
            handovers[machineId] = Handover(line, System.currentTimeMillis())
        }

        override fun noteFor(machineId: String, now: Long): String? =
            handovers[machineId]?.takeIf { now - it.at < HANDOVER_NOTE_MS }?.line
    }

    init {
        scope.launch {
            services.config.collect { configuration ->
                rebuild(configuration?.let { services.clients(it) } ?: emptyList())
                recomputeSite()
            }
        }
        scope.launch {
            services.networkAddresses().collect { current ->
                addresses = current
                recomputeSite()
            }
        }
        scope.launch {
            bindings.bindings.collect { recomputeSite() }
        }
        scope.launch {
            AgentBundle.load(app).fold(
                onSuccess = {
                    agentBundle = it.tarball
                    agentBundleVersion = it.version
                    agentBundleProblem = null
                },
                onFailure = {
                    agentBundle = null
                    agentBundleVersion = null
                    agentBundleProblem = it.message
                },
            )
        }
    }

    fun startPolling() {
        polling = true
        if (publicKey.isEmpty()) {
            scope.launch {
                publicKey = runCatching { services.publicKey() }.getOrDefault("")
                keyFingerprint = runCatching { services.keyFingerprint() }.getOrDefault("")
            }
        }
        machines.forEach { it.startPolling() }
        // Anything that was in flight when the app was last closed is picked up rather than assumed.
        operations.resume(clients)
        if (clockJob == null) {
            clockJob = scope.launch {
                while (isActive) {
                    now = System.currentTimeMillis()
                    machines.forEach { it.tick(now) }
                    delay(CLOCK_INTERVAL_MS)
                }
            }
        }
    }

    fun stopPolling() {
        polling = false
        machines.forEach { it.stopPolling() }
        clockJob?.cancel()
        clockJob = null
    }

    fun close() {
        stopPolling()
        scope.cancel()
    }

    fun refreshAll() {
        machines.forEach { it.refreshNow() }
    }

    private fun recomputeSite() {
        siteMatch = matchSite(sites, addresses, bindings.bindings.value.currentSite)
    }

    // MARK: - The setup

    /**
     * R1, for one machine, run on every successful poll.
     *
     * Two things happen without asking and both are along strict descent. Everything else either
     * costs one `config meta` round trip or stops at a person, and neither ever happens twice for
     * the same copy.
     */
    private suspend fun reconcileSetup(
        client: MachineClient,
        reportedHash: String?,
        speaksV3: Boolean,
        asked: MutableSet<String>,
    ): String? {
        val applied = services.provenance.value
        val name = client.machine.name

        return when (val decision = decideFromStatus(reportedHash, applied, speaksV3)) {
            SetupDecision.InSync -> null

            is SetupDecision.CannotCarry -> null

            is SetupDecision.Push -> publish(client, decision.reason)

            SetupDecision.AskForMeta -> {
                if (applied == null) return fetchAndAdopt(client, reportedHash)
                if (!shouldAskForMeta(reportedHash, applied.hash, asked)) return null
                reportedHash?.let(asked::add)

                val meta = client.configMeta().getOrNull()?.meta
                if (meta == null) {
                    // Nothing was learned, so nothing is remembered as asked: a round trip that
                    // failed says nothing about the document and is worth repeating.
                    reportedHash?.let(asked::remove)
                    return null
                }
                val held = SetupProvenance(
                    authority = meta.id,
                    revision = meta.revision,
                    hash = meta.hash ?: reportedHash,
                    lineage = meta.lineage.orEmpty(),
                    updatedAt = meta.updatedAt,
                    sourceKind = meta.source,
                    device = meta.device,
                    readFrom = name,
                )
                when (val settled = decideFromMeta(held, applied)) {
                    SetupDecision.InSync -> null
                    is SetupDecision.Push -> publish(client, settled.reason)
                    is SetupDecision.Adopt -> fetchAndAdopt(client, held.hash)
                    is SetupDecision.DifferentSetup -> {
                        val text = fetchText(client) ?: return null
                        services.recordIdentityClash(name, settled.reason, text, held)
                        "$name is carrying a different setup. It is waiting for a decision."
                    }

                    is SetupDecision.Diverged -> {
                        val text = fetchText(client) ?: return null
                        services.recordDivergence(name, settled.reason, text, held)
                        "$name has a copy of the setup that was edited separately. " +
                            "It is waiting to be merged."
                    }

                    SetupDecision.AskForMeta, is SetupDecision.CannotCarry -> null
                }
            }

            is SetupDecision.Adopt -> fetchAndAdopt(client, reportedHash)
            is SetupDecision.DifferentSetup, is SetupDecision.Diverged -> null
        }
    }

    /**
     * Publishes this device's copy to one machine.
     *
     * At most one attempt per machine at a time, and never while a change is running on it: the
     * machine's own lock would refuse it anyway, and hammering it for the privilege is noise.
     */
    private suspend fun publish(client: MachineClient, reason: String): String? {
        val document = services.document.value ?: return null
        val identity = document.identity ?: return null
        val machineId = client.machine.id
        if (!publishing.add(machineId)) return null
        return try {
            client.configSet(
                canonicalDocument = document.canonicalText,
                setupId = identity.id,
                revision = identity.revision,
                replace = false,
            ).fold(
                onSuccess = { reply ->
                    when {
                        reply.unchanged -> null
                        reply.stored -> "The setup was sent to ${client.machine.name}. $reason"
                        else -> "${client.machine.name} would not take the setup: " +
                            (reply.message ?: reply.reason?.sentence ?: "it refused it")
                    }
                },
                onFailure = { failure ->
                    // A push whose reply was lost is byte-identical when it is tried again and the
                    // agent answers `noop`, so there is nothing to recover here: the next poll does
                    // it. Nothing is reported for an ordinary unreachable machine.
                    if (failure.dispatchStage == com.x1f4r.legioncontrol.agent.DispatchStage.AMBIGUOUS) {
                        "The setup may or may not have reached ${client.machine.name}. " +
                            "The next check settles it."
                    } else {
                        null
                    }
                },
            )
        } finally {
            publishing.remove(machineId)
        }
    }

    private suspend fun fetchText(client: MachineClient): String? =
        (client.fetchSetup().getOrNull() as? ControllerFetch.Document)?.text

    /** The machine has a newer copy of the same setup, so take it. */
    private suspend fun fetchAndAdopt(client: MachineClient, expectedHash: String?): String? {
        val text = fetchText(client) ?: return null
        return when (val outcome = services.adoptFromMachine(text, expectedHash, client.machine.name)) {
            is SetupOutcome.Applied -> "Setup updated from ${client.machine.name}."
            SetupOutcome.Unchanged -> null
            is SetupOutcome.Invalid ->
                "${client.machine.name} is carrying a setup this app cannot use. ${outcome.reason}"

            is SetupOutcome.Diverged, is SetupOutcome.DifferentSetup -> null
        }
    }

    fun applyConfig() {
        val failure = services.applyPasted(configDraft, services.deviceName())
        configError = failure
        configNote = if (failure == null) {
            draftMatchesApplied = true
            "The setup is in force and will be sent to every machine that will take it."
        } else {
            null
        }
    }

    fun updateDraft(text: String) {
        configDraft = text
        draftMatchesApplied = text == services.configText.value
    }

    fun insertExample() {
        configDraft = EXAMPLE_CONTROLLER_CONFIG
        draftMatchesApplied = false
        configError = null
        configNote = "An example, not a working configuration. Put your own addresses, user names, " +
            "hardware address and agent paths in it before applying it."
    }

    /** Takes the other side of a divergence wholesale. */
    fun takeTheirs() {
        val divergence = divergence ?: return
        val failure = services.adoptDivergent(divergence)
        configError = failure
        if (failure == null) configNote = "Took the copy from ${divergence.machineName}."
    }

    /** Keeps this device's copy, as a revision that descends from both so every machine takes it. */
    fun keepMine() {
        val divergence = divergence ?: return
        val failure = services.keepMineOver(divergence, services.deviceName())
        configError = failure
        if (failure == null) {
            configNote = "Kept this device's setup. It will be sent to every machine."
        }
    }

    fun dismissDivergence() = services.clearDivergence()

    // MARK: - Merging

    /** Which side each differing entry takes. Filled with the defaults when a divergence arrives. */
    val mergeChoices = mutableStateMapOf<String, SetupMerge.Choice>()

    /**
     * What the two sides disagree about, at the granularity somebody can decide from.
     *
     * The base is the newest revision both were made from, when this device still has a copy of it.
     * With it, an entry only one side touched needs no answer at all; without it, every difference
     * is a two-way choice, which is a worse afternoon rather than a wrong answer.
     */
    fun divergenceDifferences(): List<SetupMerge.Difference> {
        val divergence = divergence ?: return emptyList()
        val mine = services.document.value?.root ?: return emptyList()
        val theirs = ControllerDocument.parse(divergence.theirText).getOrNull()?.root
            ?: return emptyList()
        val base = divergence.baseHash
            ?.let(services::revision)
            ?.let { ControllerDocument.parse(it).getOrNull()?.root }
        val differences = SetupMerge.differences(mine, theirs, base)
        if (mergeChoices.isEmpty()) mergeChoices.putAll(SetupMerge.defaults(differences))
        return differences
    }

    fun chooseMerge(path: String, choice: SetupMerge.Choice) {
        mergeChoices[path] = choice
    }

    /** Writes one revision made from both, which every machine takes as an ordinary fast-forward. */
    fun applyMerge() {
        val divergence = divergence ?: return
        val mine = services.document.value?.root ?: return
        val theirs = ControllerDocument.parse(divergence.theirText).getOrNull()?.root ?: return
        // Make sure the defaults exist even if the screen was never scrolled through.
        divergenceDifferences()
        val merged = SetupMerge.merge(mine, theirs, mergeChoices.toMap())
        val failure = services.applyMerge(
            ControllerDocument.of(merged),
            divergence.theirProvenance,
            services.deviceName(),
        )
        configError = failure
        if (failure == null) {
            mergeChoices.clear()
            configNote = "Merged. The result is on its way to every machine."
        }
    }

    /** What adopting the other setup would change, for the identity question. */
    fun clashPreview(): SetupChangePreview? {
        val clash = identityClash ?: return null
        val mine = services.document.value?.root ?: return null
        val theirs = ControllerDocument.parse(clash.theirText).getOrNull()?.root ?: return null
        val here = mine.machineIds()
        val there = theirs.machineIds()
        return SetupChangePreview(
            added = there.filterNot { it in here }.map(theirs::machineLabel),
            removed = here.filterNot { it in there }.map(mine::machineLabel),
            changed = here.filter { it in there && !sameMachine(mine, theirs, it) }
                .map(mine::machineLabel),
            endpointsBefore = mine.endpointCount(),
            endpointsAfter = theirs.endpointCount(),
        )
    }

    // MARK: - Settings this device keeps to itself

    var notificationsEnabled: Boolean
        get() = notifications.enabled
        set(value) {
            notifications.enabled = value
        }

    val deviceBindings get() = bindings.bindings

    val bindingsActions = object : BindingsActions {
        override fun setDeviceName(name: String) = bindings.setDeviceName(name)
        override fun setCurrentSite(siteId: String?) = bindings.setCurrentSite(siteId)
    }

    /** Whether the editor page is showing. It is a page rather than a screen of its own. */
    var editorOpen: Boolean by mutableStateOf(false)
        private set

    fun openEditor() {
        editor.startFresh()
        editorOpen = true
    }

    fun closeEditor() {
        editorOpen = false
    }

    /** Replaces a machine's different setup with this one. The only place `--replace` is ever sent. */
    fun replaceTheirSetup() {
        val clash = identityClash ?: return
        val client = clients.firstOrNull { it.machine.name == clash.machineName } ?: return
        val document = services.document.value ?: return
        val identity = document.identity ?: return
        scope.launch {
            client.configSet(
                canonicalDocument = document.canonicalText,
                setupId = identity.id,
                revision = identity.revision,
                replace = true,
            ).fold(
                onSuccess = {
                    configNote = "${clash.machineName} now carries this setup."
                    services.clearIdentityClash()
                },
                onFailure = { configError = it.sentence },
            )
        }
    }

    /** Adopts the other setup entirely, dropping this device's. */
    fun adoptTheirSetup() {
        val clash = identityClash ?: return
        val failure = services.adoptOtherSetup(clash)
        configError = failure
        if (failure == null) {
            configNote = "This device now follows the setup from ${clash.machineName}."
        }
    }

    fun dismissIdentityClash() = services.clearIdentityClash()

    // MARK: - Bootstrap

    fun fetchSetup() {
        if (fetching) return
        val source = SetupSource(
            host = fetchHost.trim(),
            port = fetchPort.trim().toIntOrNull()?.takeIf { it in 1..65535 } ?: 22,
            user = fetchUser.trim(),
        )
        if (source.host.isBlank() || source.user.isBlank()) return
        fetching = true
        fetchOutcome = "Asking ${source.host} for the setup."
        scope.launch {
            try {
                services.fetchSetupFrom(source).fold(
                    onSuccess = { fetched -> fetchOutcome = applyFetched(fetched, source) },
                    onFailure = { failure ->
                        (failure as? AgentFailure.HostKeyChanged)?.takeIf { it.canApprove }?.let {
                            fetchDialog = MachineModel.Dialog.ConfirmTrustHostKey(
                                address = it.address,
                                keyBlob = it.offeredKeyBlob,
                                fingerprint = it.offeredFingerprint,
                                isFirstContact = it.isFirstContact,
                                canApprove = it.canApprove,
                                detail = it.detail,
                                trust = services.hostTrust(it.address),
                                suggestedSystems = listOf(com.x1f4r.legioncontrol.data.HostIdentitySystem("system", "System")),
                            )
                        }
                        fetchOutcome = explain(failure, source)
                    },
                )
            } finally {
                fetching = false
            }
        }
    }

    fun trustFetchHostKey(address: String, keyBlob: String, approval: com.x1f4r.legioncontrol.data.HostTrustApproval) {
        if (fetching) return
        fetching = true
        fetchOutcome = "Trusting the key for $address."
        scope.launch {
            try {
                services.trustHostKey(address, keyBlob, approval)
                fetching = false
                fetchSetup()
            } catch (failure: Exception) {
                fetchOutcome = failure.message ?: "Could not save host trust."
                fetching = false
            }
        }
    }

    private fun applyFetched(fetched: ControllerFetch, source: SetupSource): String = when (fetched) {
        is ControllerFetch.Document -> {
            val failure = services.applyPasted(fetched.text, services.deviceName())
            val found = services.config.value?.machines?.size ?: 0
            failure?.let { "The setup from ${source.host} was refused. $it" }
                ?: "The setup came from ${source.host}: $found " +
                    "machine${if (found == 1) "" else "s"}."
        }

        ControllerFetch.NothingStored ->
            "That machine has no setup stored yet. Paste one here, or fetch from a machine that has one."

        ControllerFetch.TooOld ->
            "The control agent on that machine is too old to carry a setup. It needs contract 3."

        ControllerFetch.Unreadable ->
            "${source.host} answered with something that is not a setup."
    }

    private fun explain(failure: Throwable, source: SetupSource): String = when (failure) {
        is AgentFailure.NotAuthorised ->
            "This phone's key is not authorised on ${source.host} yet. This device shows the line " +
                "to add to authorized_keys there."

        else -> failure.sentence
    }

    private fun rebuild(built: List<MachineClient>) {
        machines.forEach { it.stopPolling() }
        clients = built
        machines = built.map { client ->
            MachineModel(client, remembered, scope, operations, setup, fleet)
        }
        footerMachine = machines.firstOrNull()
        if (polling) {
            machines.forEach { it.startPolling() }
            operations.resume(clients)
        }
        editor.reset(services.document.value)
        // The field follows what is in force only while nobody is halfway through typing into it.
        // Losing an edit to a document that arrived from another peer is exactly the thing the whole
        // reconciliation is meant to prevent, and clobbering the field would do it locally instead.
        if (draftMatchesApplied) {
            configDraft = services.configText.value
        } else if (configDraft != services.configText.value) {
            configNote = "The setup in force changed while you were editing. Your text is still here; " +
                "Apply replaces what is in force with it."
        }
    }

    private companion object {
        const val CLOCK_INTERVAL_MS = 5_000L
        const val DEFAULT_SSH_PORT = "22"
        const val HANDOVER_NOTE_MS = 300_000L
    }
}

/** Whether finished changes are announced when the app is not in front. Off until asked for. */
class NotificationSettings(context: Context) {
    private val prefs = context.applicationContext
        .getSharedPreferences("legion-notifications", Context.MODE_PRIVATE)

    var enabled: Boolean
        get() = prefs.getBoolean(KEY, false)
        set(value) {
            prefs.edit().putBoolean(KEY, value).apply()
        }

    private companion object {
        const val KEY = "operation-notifications"
    }
}
