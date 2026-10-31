package com.x1f4r.legioncontrol.ui

import android.content.Context
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.AgentBundle
import com.x1f4r.legioncontrol.agent.SignedTarball
import com.x1f4r.legioncontrol.agent.HelperState
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.Contract
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.SetupPeerState
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.DocumentProblem
import com.x1f4r.legioncontrol.data.EXAMPLE_CONTROLLER_CONFIG
import com.x1f4r.legioncontrol.data.KeyChange
import com.x1f4r.legioncontrol.data.LaunchReviewEntry
import com.x1f4r.legioncontrol.data.LaunchValues
import com.x1f4r.legioncontrol.agent.SetupChangePreview
import com.x1f4r.legioncontrol.data.SetupMerge
import com.x1f4r.legioncontrol.data.SetupDivergence
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.SetupOutcome
import com.x1f4r.legioncontrol.data.endpointCount
import com.x1f4r.legioncontrol.data.machineIds
import com.x1f4r.legioncontrol.data.machineLabel
import com.x1f4r.legioncontrol.data.sameMachine
import com.x1f4r.legioncontrol.data.SetupSource
import com.x1f4r.legioncontrol.data.advisories
import com.x1f4r.legioncontrol.data.SiteConfig
import com.x1f4r.legioncontrol.net.LocalAddress
import com.x1f4r.legioncontrol.net.SiteMatch
import com.x1f4r.legioncontrol.net.matchSite
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.time.Instant

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
    /** Held by every change while it is sent or followed, which keeps the process running. */
    keepAlive: KeepAlive,
) {
    private val app = context.applicationContext

    /**
     * The scope changes run on.
     *
     * Deliberately not tied to the screen, and never cancelled: this model belongs to the process.
     * Polling stops when the app is put away; an update that was asked for does not, because locking
     * the phone is not withdrawing the request. A failure nothing caught is logged rather than
     * allowed to end the process.
     */
    private val scope =
        CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate + uncaughtFailureHandler("AppModel"))
    private var clockJob: Job? = null
    private var polling = false

    /**
     * Whether the screen is in front of somebody: between the activity's resume and pause, which is
     * also exactly when it polls. A change that ends now is told by the page, not by a notification.
     */
    val inFront: Boolean get() = polling

    private val notifier = AndroidOperationNotifier(app, enabled = { notifications.enabled }, inFront = { inFront })

    /** Every change this app has asked for, across launches. */
    val operations = OperationCenter(services.operations, scope, notifier, keepAlive)

    var machines: List<MachineModel> by mutableStateOf(emptyList())
        private set

    private var clients: List<MachineClient> = emptyList()

    var publicKey: String by mutableStateOf("")
        private set
    var keyFingerprint: String by mutableStateOf("")
        private set

    /** Why this phone's key could not be read the last time it was asked for. Nothing was replaced. */
    var keyProblem: String? by mutableStateOf(null)
        private set

    /** This phone's key was replaced because the old one could no longer be opened, until seen. */
    var keyChange: KeyChange? by mutableStateOf(null)
        private set

    var now: Long by mutableStateOf(System.currentTimeMillis())
        private set

    /** Where this device is standing, and how sure that is. */
    var siteMatch: SiteMatch by mutableStateOf(SiteMatch.NoSites)
        private set

    /**
     * This device's candidate addresses, each with its network (contract section 10.1). Read by
     * every placement: the site line, route order and the wake plan, which is worked out again from
     * these immediately before every wake.
     */
    var addresses: List<LocalAddress> by mutableStateOf(emptyList())
        private set

    /** The signed agent bundle this build carries, once it has been checked. */
    var agentBundle: SignedTarball? by mutableStateOf(null)
        private set
    var agentBundleVersion: String? by mutableStateOf(null)
        private set
    var agentBundleProblem: String? by mutableStateOf(null)
        private set

    /** The setup editor, which every peer has. */
    val editor = SetupEditorModel(services)

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

    /** Setup sync with every machine: what to send, when, and what to take (contract section 9.5). */
    private val sync = SetupPeerSync(services)

    /** Why the setup is not being sent to each machine, by machine id, while it is not. Drawn from. */
    private val holds = mutableStateMapOf<String, String>()

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

    /**
     * Warnings about the setup that are worth showing and are never a reason to refuse it: what
     * contract section 9.7 says this app cannot use, and what this app says beside it.
     */
    val advisories: List<String>
        get() = services.acceptance.value?.warnings.orEmpty().map { it.message } +
            services.config.value?.advisories().orEmpty()

    /**
     * What is wrong with the document in force, when an earlier build stored one this one would
     * refuse. It stays in force; nothing is published and no machine an error is inside is driven
     * until it is fixed in the editor.
     */
    val setupErrors: List<DocumentProblem>
        get() = services.acceptance.value?.errors.orEmpty()

    /** Why this device's setup cannot replace another machine's copy, or null when it can. */
    val replaceProblem: String? get() = whyNotReplace(setupErrors)

    private val fleet = object : FleetContext {
        override val machines: List<Machine> get() = clients.map { it.machine }
        override val sites: List<SiteConfig> get() = this@AppModel.sites
        override val localAddresses: List<LocalAddress> get() = addresses
        override val currentSite: String? get() = bindings.bindings.value.currentSite
        override val deviceName: String get() = services.deviceName()
        override val agentBundle: SignedTarball? get() = this@AppModel.agentBundle
        override val agentBundleProblem: String? get() = this@AppModel.agentBundleProblem

        override fun client(machineId: String): MachineClient? =
            clients.firstOrNull { it.machine.id == machineId }

        override fun helperState(machineId: String): HelperState =
            this@AppModel.machines.firstOrNull { it.machine.id == machineId }?.helperState ?: HelperState.UNKNOWN

        override suspend fun readHelper(machineId: String): HelperState =
            this@AppModel.machines.firstOrNull { it.machine.id == machineId }?.readForWake() ?: HelperState.UNKNOWN

        override fun forgetRoute(machineId: String) = services.forgetRoute(machineId)

        override fun prepareRoute(machineId: String, systemId: String, onSite: Boolean) =
            services.prepareRoute(machineId, systemId, onSite)

        override fun wakeMachine(machineId: String) {
            this@AppModel.machines.firstOrNull { it.machine.id == machineId }?.wake()
        }
    }

    private val setup = object : SetupSync {
        override fun state(machineId: String): SetupPeerState = sync.state(machineId)

        override suspend fun reconcile(
            client: MachineClient,
            reply: AgentStatus,
            localMutation: Boolean,
            state: SetupPeerState,
        ): String? = try {
            sync.reconcile(client, reply, localMutation, state)
        } finally {
            noteHold(client.machine.id)
        }

        override fun remember(machineId: String, line: String) {
            handovers[machineId] = Handover(line, System.currentTimeMillis())
        }

        override fun noteFor(machineId: String, now: Long): String? =
            handovers[machineId]?.takeIf { now - it.at < HANDOVER_NOTE_MS }?.line

        override fun holdFor(machineId: String): String? = holds[machineId]

        override fun retry(machineId: String) {
            sync.retry(machineId)
            noteHold(machineId)
        }
    }

    /** Copies one machine's hold into what the screen draws from. */
    private fun noteHold(machineId: String) {
        val hold = sync.hold(machineId)
        if (hold == null) holds.remove(machineId) else holds[machineId] = hold
    }

    init {
        // Machines are rebuilt from what the gate lets this device run, which changes with the
        // document and with every approval. The editor and the raw field follow the document alone:
        // approving a command must not throw away an edit somebody is halfway through.
        scope.launch {
            services.launch.collect { gated ->
                rebuild(gated.config?.let { services.clients(gated) } ?: emptyList())
                recomputeSite()
            }
        }
        scope.launch {
            services.config.collect { followDocument() }
        }
        scope.launch {
            services.networkAddresses().collect { current ->
                addresses = current
                recomputeSite()
            }
        }
        scope.launch {
            services.networkPaths().collect(services::followPaths)
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
        if (publicKey.isEmpty()) loadKey()
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

    /**
     * Reads this phone's key, and whether it was replaced.
     *
     * A key that could not be read is said so, rather than drawn as one that has not been generated
     * yet: it is there, nothing replaced it, and this is asked again on the next return to the app
     * or a press of Try again.
     */
    fun loadKey() {
        scope.launch {
            try {
                publicKey = services.publicKey()
                keyFingerprint = services.keyFingerprint()
                keyProblem = null
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                keyProblem = failure.message ?: "This phone's key could not be read just now."
            }
            keyChange = try {
                services.keyChange()
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: Exception) {
                keyChange
            }
        }
    }

    /**
     * Replaces a key that keeps failing to read, once the user has confirmed it.
     *
     * Never done on the app's own say: every machine that trusted the old key stops accepting this
     * phone. What follows is the same as after any replacement, the new key and a note saying it
     * changed; a key that read by the time this ran is kept, and nothing is said.
     */
    fun replaceUnreadableKey() {
        scope.launch {
            try {
                services.replaceUnreadableKey()
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                keyProblem = failure.message ?: "A new key for this phone could not be made just now."
                return@launch
            }
            loadKey()
        }
    }

    /** The user has seen that the key changed and has the new one. */
    fun acknowledgeKeyChange() {
        keyChange = null
        scope.launch { runCatching { services.acknowledgeKeyChange() } }
    }

    fun stopPolling() {
        polling = false
        machines.forEach { it.stopPolling() }
        clockJob?.cancel()
        clockJob = null
    }

    /**
     * The machine a tapped notification asked to show, until the screen has moved to it. Only ever a
     * machine this setup has: the id comes in on an intent, which anything on the phone can send.
     */
    var requestedMachine: String? by mutableStateOf(null)
        private set

    fun openMachine(machineId: String) {
        if (machines.any { it.machine.id == machineId }) requestedMachine = machineId
    }

    /** The screen is on the requested machine's page now. */
    fun openedMachine() {
        requestedMachine = null
    }

    /**
     * Pull to refresh, or the refresh button. Asked for by a person, so every address is dialled
     * again, the ones held back after going unanswered included.
     */
    fun refreshAll() {
        services.retryAddresses()
        machines.forEach { it.refreshNow() }
    }

    private fun recomputeSite() {
        siteMatch = matchSite(sites, addresses, bindings.bindings.value.currentSite)
    }

    // MARK: - The setup

    fun applyConfig() {
        // Shown once, as section 9.5 asks: a document that named no setup is given an id here.
        val named = ControllerDocument.parse(configDraft).getOrNull()?.setupId != null
        val failure = services.applyPasted(configDraft, services.deviceName())
        configError = failure
        configNote = if (failure == null) {
            draftMatchesApplied = true
            val assigned = services.provenance.value?.authority?.takeUnless { named }
            "The setup is in force and will be sent to every machine that will take it." +
                (assigned?.let { " It was given the setup id $it." } ?: "")
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

    /**
     * Take theirs: the other side's verified bytes, unchanged, over the document this screen was
     * built against and no other.
     */
    fun takeTheirs() {
        val divergence = divergence ?: return
        settled(services.adoptDivergent(divergence)) { "Took the copy from ${divergence.machineName}." }
    }

    /** Keeps this device's copy, as a revision that descends from both so every machine takes it. */
    fun keepMine() {
        val divergence = divergence ?: return
        val choices = divergenceDifferences().associate { it.entry to SetupMerge.Choice.MINE }
        mergeAndApply(divergence, choices) { "Kept this device's setup. It will be sent to every machine." }
    }

    fun dismissDivergence() = services.clearDivergence()

    // MARK: - Merging

    /** Which side each differing entry takes. Filled with the defaults when a divergence arrives. */
    val mergeChoices = mutableStateMapOf<String, SetupMerge.Choice>()

    /**
     * The two hashes [mergeChoices] were made for. A choice about one copy says nothing about
     * another, so the choices start again from the defaults whenever the question on screen is a
     * different one.
     */
    private var choicesFor: String? = null

    /**
     * Why Merge and Keep mine cannot be offered for the divergence on screen, or null: this device's
     * copy has an error that leaves its entries undefined, and is fixed in the editor first. Take
     * theirs is always offered.
     */
    val mergeBlocked: String?
        get() {
            divergence ?: return null
            val mine = services.document.value ?: return "There is no setup here to merge."
            return SetupMerge.mineBlocks(mine.canonicalText)
        }

    /**
     * What the two sides disagree about, at the granularity somebody can decide from (contract
     * section 9.8).
     *
     * The base is the newest revision both were made from, when this device still has a copy of it
     * that hashes to it. With it, an entry only one side touched needs no answer at all; without it,
     * every difference is a two-way choice, which is a worse afternoon rather than a wrong answer.
     */
    fun divergenceDifferences(): List<SetupMerge.Difference> {
        val divergence = divergence ?: return emptyList()
        val mine = services.document.value ?: return emptyList()
        val differences = try {
            SetupMerge.differences(mine.canonicalText, divergence.theirText, baseOf(divergence))
        } catch (refused: SetupMerge.Unmergeable) {
            return emptyList()
        }
        val question = "${mine.hash} ${divergence.theirProvenance.hash}"
        if (choicesFor != question) {
            mergeChoices.clear()
            choicesFor = question
        }
        if (mergeChoices.isEmpty()) differences.forEach { mergeChoices[it.entry] = it.defaultChoice }
        return differences
    }

    fun chooseMerge(entry: String, choice: SetupMerge.Choice) {
        mergeChoices[entry] = choice
    }

    /** Writes one revision made from both, which every machine takes as an ordinary fast-forward. */
    fun applyMerge() {
        val divergence = divergence ?: return
        // Make sure the defaults exist even if the screen was never scrolled through.
        divergenceDifferences()
        mergeAndApply(divergence, mergeChoices.toMap()) { "Merged. The result is on its way to every machine." }
    }

    private fun mergeAndApply(divergence: SetupDivergence, choices: Map<String, SetupMerge.Choice>, done: () -> String) {
        val mine = services.document.value ?: return
        val outcome = try {
            SetupMerge.merge(
                baseText = baseOf(divergence),
                mineText = mine.canonicalText,
                theirsText = divergence.theirText,
                mineHash = mine.hash,
                theirsHash = divergence.theirProvenance.hash ?: return,
                writer = SetupMerge.Writer(ControllerDocument.CLIENT_KIND, services.deviceName(), Instant.now().toString()),
                choices = choices,
            )
        } catch (refused: SetupMerge.Unmergeable) {
            configError = refused.message
            return
        }
        val text = outcome.resultText ?: run {
            configError = "Those choices would make a setup that breaks a rule every client holds it to " +
                "(${outcome.error}), so nothing was written. Take the other side of the entry that names it."
            return
        }
        settled(services.applyMerge(text, divergence.mineProvenance?.hash)) {
            mergeChoices.clear()
            done()
        }
    }

    /** The kept base of a divergence, which the store already checked hashes to it. */
    private fun baseOf(divergence: SetupDivergence): String? = divergence.baseHash?.let(services::revision)

    /**
     * What a decision on a divergence or a different setup came to. A document that changed while
     * the screen was open is not replaced: the screen closes, and the next check compares again.
     */
    private fun settled(outcome: SetupOutcome, done: () -> String) {
        when (outcome) {
            is SetupOutcome.Applied, SetupOutcome.Unchanged -> {
                configError = null
                configNote = done()
            }

            SetupOutcome.HeadMoved -> {
                services.clearDivergence()
                services.clearIdentityClash()
                mergeChoices.clear()
                configError = null
                configNote = "The setup here changed while this was open; comparing again."
            }

            is SetupOutcome.Invalid -> configError = outcome.reason
            is SetupOutcome.NotSaved -> configError = outcome.reason
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

    /**
     * Whether Android lets this app post a notification now. Asked rather than remembered: the user
     * can take it back in the system settings at any time, and the setting here is not told.
     */
    fun notificationsPermitted(): Boolean = notifier.permitted()

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

    /**
     * Whether a Replace is out to a machine. The button waits for it: the reply to a second press
     * could come back after the first one's and say the wrong thing about it.
     */
    var replacing: Boolean by mutableStateOf(false)
        private set

    /**
     * What the last Replace came to, with the question it was pressed on. It is drawn on that
     * question's page, where it was pressed, for as long as that question is the one open: the
     * Device page, where the setup's other notes go, is not where anybody pressing it is looking.
     */
    private var replaced: Pair<SetupIdentityClash, SetupPeerSync.Replaced>? by mutableStateOf(null)

    /** What Replace came to on the question on screen, or null when it was not pressed there. */
    internal val replaceOutcome: SetupPeerSync.Replaced?
        get() = replaced?.takeIf { it.first == identityClash }?.second

    /**
     * Whether Replace can be pressed on the question on screen: not while one is out, and not again
     * on a question it already found out of date, where all it could do is say so again.
     */
    val canReplace: Boolean
        get() = replaceProblem == null && !replacing && replaceOutcome !is SetupPeerSync.Replaced.Stale

    /**
     * Replaces a machine's different setup with this one. The only place `--replace` is ever sent,
     * so it goes to the machine that served the copy on screen, found by its id: a name is free
     * text, and another machine may carry the same one. It is checked against what that machine's
     * latest status says it holds, which [heldBy] finds the same way.
     *
     * The question closes only when the machine says it took the document, or, when it is out of
     * date, at the machine's next status, which asks again; until then it says why nothing was
     * sent. A refusal, a push the lock held back and a reply that leaves it open whether the copy
     * was replaced all keep it on screen, saying so.
     */
    fun replaceTheirSetup() {
        val clash = identityClash ?: return
        if (replacing) return
        val client = clients.firstOrNull { it.machine.id == clash.machineId } ?: return
        // Asked of the setup as it is now, not only when the page was drawn.
        replaceProblem?.let {
            configError = it
            return
        }
        val latest = machines.heldBy(clash.machineId)
        replacing = true
        replaced = null
        configError = null
        scope.launch {
            try {
                val outcome = sync.replace(client, clash, latest)
                replaced = clash to outcome
                // The question and its page are gone, so the line goes with the setup's others.
                if (outcome is SetupPeerSync.Replaced.Done) configNote = outcome.line
            } finally {
                replacing = false
                // A refusal no retry can cure holds this hash for the machine, as a push's does, and
                // its page says so with Try again.
                noteHold(client.machine.id)
            }
        }
    }

    /** Adopts the other setup entirely, dropping this device's. */
    fun adoptTheirSetup() {
        val clash = identityClash ?: return
        replaced = null
        settled(services.adoptOtherSetup(clash)) { "This device now follows the setup from ${clash.machineName}." }
    }

    /**
     * Leave it: not asked about again until this device's setup or that copy changes, on any
     * machine carrying the same copy.
     */
    fun dismissIdentityClash() {
        val clash = identityClash ?: return
        sync.leave(clash.theirProvenance.hash)
        services.clearIdentityClash()
    }

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

    /**
     * What a fetch found. A document is not applied here: it is shown first, every command, key
     * file and update source in it, and only [confirmEnrolment] puts it in force.
     */
    private fun applyFetched(fetched: ControllerFetch, source: SetupSource): String = when (fetched) {
        is ControllerFetch.Document -> enrolment.offer(fetched, source.host)

        ControllerFetch.NothingStored ->
            "That machine has no setup stored yet. Paste one here, or fetch from a machine that has one."

        ControllerFetch.TooOld ->
            "The control agent on that machine is too old to carry a setup. It needs contract 3."

        ControllerFetch.Unreadable ->
            "${source.host} answered with something that is not a setup."
    }

    // MARK: - Enrolment and the commands this device runs (contract section 9.9)

    /** A fetched setup waiting to be confirmed or cancelled. Nothing of it is applied until then. */
    val enrolment = SetupEnrolment(services)

    /** Confirm on the summary of a fetched setup. See [SetupEnrolment.confirm]. */
    fun confirmEnrolment() {
        enrolment.confirm()?.let { fetchOutcome = it }
    }

    /** Cancel on the summary of a fetched setup, which leaves this device as it was. */
    fun cancelEnrolment() {
        enrolment.cancel()?.let { fetchOutcome = it }
    }

    /** Each launch value of the setup in force that waits for this device's approval. */
    val launchReview: StateFlow<List<LaunchReviewEntry>> =
        services.launch.map { it.review }.stateIn(scope, SharingStarted.Eagerly, services.launch.value.review)

    /** Why the record of approved commands could not be read, when it could not. */
    val launchProblem: StateFlow<String?> get() = services.launchProblem

    var launchNote: String? by mutableStateOf(null)
        private set

    /** The revision whose review was put off with Not now. Kept in memory, for this run only. */
    private var deferredReview: String? by mutableStateOf(null)

    /** Set by Review: the review page shows even for a revision it was put off for. */
    private var reviewAsked: Boolean by mutableStateOf(false)

    /**
     * Review was pressed and the screen has not moved to the page yet. Taken once, as the machine a
     * notification asked for is: this model outlives every screen, and a count of presses was read
     * again by each screen built after them, which moved to the review page nobody had asked for.
     */
    var launchReviewRequested: Boolean by mutableStateOf(false)
        private set

    /** Whether the review page is up: something waits, and it was not put off for this revision. */
    fun showsLaunchReview(review: List<LaunchReviewEntry>): Boolean =
        review.isNotEmpty() && (reviewAsked || deferredReview != setupProvenance?.hash)

    fun openLaunchReview() {
        reviewAsked = true
        launchReviewRequested = true
    }

    /** The screen has moved to the review page, or had none to move to. */
    fun openedLaunchReview() {
        launchReviewRequested = false
    }

    /** Not now: the page does not come back for this revision in this run. The badge stays. */
    fun deferLaunchReview(hash: String?) {
        deferredReview = hash
        reviewAsked = false
    }

    /** Approve: records exactly the values shown, at their places. */
    fun approveLaunch(entries: List<LaunchReviewEntry>) {
        launchNote = services.approveLaunch(entries)
            ?: if (entries.size == 1) "Approved on this device." else "Approved ${entries.size} on this device."
    }

    /** Whether [entry] has an approved value at its place that an edit can put back. */
    fun canRevertLaunch(entry: LaunchReviewEntry): Boolean =
        entry.approved != null && services.document.value?.let { LaunchValues.reverted(it.root, entry) } != null

    /**
     * Revert: an ordinary edit made here that puts back the value this device approved at that
     * place, made by the editor, which waits while it holds an edit of its own.
     */
    fun revertLaunch(entry: LaunchReviewEntry) {
        launchNote = editor.revertLaunch(entry)
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
    }

    private fun followDocument() {
        editor.reset(services.document.value)
        // A hold is for one revision, so a new one lifts it on screen at once rather than at the
        // next check.
        machines.forEach { noteHold(it.machine.id) }
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

/**
 * Why a setup with [errors] cannot be sent over another machine's copy with `--replace`, or null
 * when it can. A device whose setup has a section 9.7 error pushes nothing until it is fixed
 * (contract section 9.5), and a replacement least of all: an agent before 3.1.0 stores whatever it
 * is sent.
 */
internal fun whyNotReplace(errors: List<DocumentProblem>): String? {
    val first = errors.firstOrNull() ?: return null
    return "This device's setup has an error to fix in the editor before it can replace another copy: ${first.message}"
}

/**
 * The line under Replace on the different-setup question, once nothing is out: what the last
 * Replace on it came to, or the setup's latest note. While [problem] says why Replace is off, that
 * goes before what an earlier press came to, which may say to try again when nothing can be tried
 * until the error is fixed.
 */
internal fun replaceLine(outcome: SetupPeerSync.Replaced?, configError: String?, problem: String?): String? =
    if (problem != null) configError ?: problem else outcome?.line ?: configError

/**
 * The document [machineId]'s latest status says it holds, or null when it has none to go by. Found
 * by id, as Replace finds the machine it sends to: a name is free text, and another machine may
 * carry the same one.
 */
internal fun List<MachineModel>.heldBy(machineId: String): String? =
    firstOrNull { it.machine.id == machineId }?.status?.controller?.hash
