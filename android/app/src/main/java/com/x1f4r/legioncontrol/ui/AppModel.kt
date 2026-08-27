package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.data.EXAMPLE_CONTROLLER_CONFIG
import com.x1f4r.legioncontrol.data.SetupSource
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * The app, which is a list of machines and this phone.
 *
 * Everything that is not about one particular machine lives here: the configuration that decides
 * which machines there are at all, this phone's key, and the one clock the age lines are read
 * against. A machine of its own is a [MachineModel], built when the configuration says so and
 * thrown away when it stops saying so.
 */
@Stable
class AppModel(
    private val services: ControlServices,
    private val remembered: RememberedSettings,
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var clockJob: Job? = null
    private var polling = false

    /** One per configured machine, in the order the configuration lists them. */
    var machines: List<MachineModel> by mutableStateOf(emptyList())
        private set

    /** Read once, on the way in. Generating the key is the slow part and it happens exactly once. */
    var publicKey: String by mutableStateOf("")
        private set

    /** Ticks while the screen is up, purely so "checked two minutes ago" stops being a lie. */
    var now: Long by mutableStateOf(System.currentTimeMillis())
        private set

    /** What is in the configuration field, which is not what is in force until Apply says so. */
    var configDraft: String by mutableStateOf(services.configText.value)

    /** Why the last Apply changed nothing. Null when it did. */
    var configError: String? by mutableStateOf(null)
        private set

    /** One line of acknowledgement after an Apply that worked. */
    var configNote: String? by mutableStateOf(null)
        private set

    // MARK: - Fetching the setup from a machine

    /**
     * The address the setup is fetched from, as three fields.
     *
     * Text rather than an [Int] for the port, because a field that is being typed into is text and
     * a half-typed number is not one. It starts on the port ssh is on, which is the answer for every
     * machine that has not been moved off it.
     */
    var fetchHost: String by mutableStateOf(services.setupSource.value?.host.orEmpty())
    var fetchPort: String by mutableStateOf(
        services.setupSource.value?.port?.toString() ?: DEFAULT_SSH_PORT,
    )
    var fetchUser: String by mutableStateOf(services.setupSource.value?.user.orEmpty())

    /** Whether a fetch is on the wire, so the button can spin rather than look ignored. */
    var fetching: Boolean by mutableStateOf(false)
        private set

    /** How the last fetch ended, in one sentence. */
    var fetchOutcome: String? by mutableStateOf(null)
        private set

    /**
     * The host key question a fetch raised, asked in the same words a machine asks it in.
     *
     * An address given by hand has never been seen before, so the first fetch to a machine that is
     * not in a configuration yet is exactly the case the trust question exists for.
     */
    var fetchDialog: MachineModel.Dialog.ConfirmTrustHostKey? by mutableStateOf(null)

    /** Enough typed in to be worth a round trip. The port falls back to 22 rather than blocking. */
    val canFetch: Boolean
        get() = !fetching && fetchHost.isNotBlank() && fetchUser.isNotBlank()

    /**
     * What one machine's handover of the setup did, per machine.
     *
     * Held here rather than on the machine model because applying a document rebuilds every model
     * there is, including the one that just fetched it: the line would be thrown away by the change
     * that earned it.
     */
    private val handovers = mutableStateMapOf<String, Handover>()

    private data class Handover(val line: String, val at: Long)

    private val setup = object : SetupSync {
        override val appliedHash: String? get() = services.configHash.value

        override fun applyFromMachine(document: String, hash: String?): String? =
            services.applyFetchedConfig(document, hash)

        override fun remember(machineId: String, line: String) {
            handovers[machineId] = Handover(line, System.currentTimeMillis())
        }

        override fun noteFor(machineId: String, now: Long): String? =
            handovers[machineId]?.takeIf { now - it.at < HANDOVER_NOTE_MS }?.line
    }

    val hasMachines: Boolean get() = machines.isNotEmpty()

    /** Which page is in front, so the footer can follow the machine that page belongs to. */
    var currentPage: Int by mutableStateOf(0)

    /**
     * The machine the footer speaks for.
     *
     * Set by the pages as they come to the front, and left alone by the pages that belong to no
     * machine. The line at the bottom is a running commentary on the last thing the app did, and
     * swiping to this phone's own page is not a reason for it to go blank.
     */
    var footerMachine: MachineModel? by mutableStateOf(null)
        private set

    fun showing(model: MachineModel?) {
        if (model != null && model !== footerMachine) footerMachine = model
    }

    /** The one question on screen, whichever machine asked it. */
    val dialogOwner: MachineModel?
        get() = machines.firstOrNull { it.dialog != null }

    /**
     * Whether the phone's key has been refused by whatever it is talking to.
     *
     * The key screen takes over the whole app rather than being a line on a page, so it is asked of
     * every machine at once: with one machine it is the state the app is in, and with several it is
     * still the one thing that has to be fixed before any of them will answer.
     */
    val needsKeyAuthorisation: Boolean
        get() = machines.isNotEmpty() && machines.all { it.needsKeyAuthorisation }

    val keyAuthorisationDetail: String?
        get() = machines.firstOrNull { it.needsKeyAuthorisation }?.statusDetail

    init {
        // The configuration decides which machines exist, so this is the one subscription that runs
        // for the life of the process rather than only while the screen is in front.
        scope.launch {
            services.config.collect { configuration ->
                rebuild(configuration?.let { services.clients(it) } ?: emptyList())
            }
        }
    }

    fun startPolling() {
        polling = true
        if (publicKey.isEmpty()) {
            scope.launch { publicKey = runCatching { services.publicKey() }.getOrDefault("") }
        }
        machines.forEach { it.startPolling() }
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

    /** Everything stops here. Called when the activity goes away for good. */
    fun close() {
        stopPolling()
        scope.cancel()
    }

    /** Re-reads every machine at once. The pull to refresh gesture lands here. */
    fun refreshAll() {
        machines.forEach { it.refreshNow() }
    }

    // MARK: - Configuration

    fun applyConfig() {
        val failure = services.applyConfig(configDraft)
        configError = failure
        configNote = if (failure == null) "The configuration is in force." else null
    }

    /**
     * Reads the setup off one machine and puts it in force.
     *
     * The one thing in this app that talks to an address nobody has been told about, because it is
     * the address the configuration is about to come from. Everything after this fetch is ordinary:
     * the document is validated exactly as a pasted one is, and a document that does not hold up
     * leaves the phone with whatever it had before.
     */
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
                        // The trust question is the one failure with something to press, and it is
                        // asked in exactly the words a machine asks it in.
                        (failure as? AgentFailure.HostKeyChanged)?.let {
                            fetchDialog = MachineModel.Dialog.ConfirmTrustHostKey(
                                address = it.address,
                                keyBlob = it.offeredKeyBlob,
                                detail = it.detail,
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

    /** Trusts the key the fetch was stopped by, then goes back and asks the same machine again. */
    fun trustFetchHostKey(address: String, keyBlob: String) {
        if (fetching) return
        fetching = true
        fetchOutcome = "Trusting the key for $address."
        scope.launch {
            services.trustHostKey(address, keyBlob)
            fetching = false
            fetchSetup()
        }
    }

    private fun applyFetched(fetched: ControllerFetch, source: SetupSource): String = when (fetched) {
        is ControllerFetch.Document -> {
            val refused = services.applyFetchedConfig(fetched.text, fetched.hash)
            val found = services.config.value?.machines?.size ?: 0
            refused?.let { "The setup from ${source.host} was refused. $it" }
                ?: "The setup came from ${source.host}: $found " +
                    "machine${if (found == 1) "" else "s"}."
        }

        ControllerFetch.NothingStored ->
            "That machine has no setup stored yet. Share it from the Mac first."

        ControllerFetch.TooOld ->
            "The agent on that machine is too old to share the setup (needs 2.1.0)."

        // The fetch itself never ends here: a reply it could not read is a reason to try the next
        // command shape, and running out of shapes is a failure rather than an outcome.
        ControllerFetch.Unreadable ->
            "${source.host} answered with something that is not a setup."
    }

    /**
     * A failed fetch, in one sentence.
     *
     * The transport already writes these, and they are the same sentences the machine pages use, so
     * the only one worth adding to is the refused key: on a phone with no configuration there is no
     * key screen to fall into, and the page holding the key is a swipe away rather than in front of
     * you.
     */
    private fun explain(failure: Throwable, source: SetupSource): String = when (failure) {
        is AgentFailure.NotAuthorised ->
            "This phone's key is not authorised on ${source.host} yet. This device shows the line " +
                "to add to authorized_keys there."

        else -> failure.sentence
    }

    fun insertExample() {
        configDraft = EXAMPLE_CONTROLLER_CONFIG
        configError = null
        configNote = "An example, not a working configuration. Put your own addresses, user names, " +
            "hardware address and agent paths in it before applying it."
    }

    private fun rebuild(clients: List<MachineClient>) {
        machines.forEach { it.stopPolling() }
        machines = clients.map { client -> MachineModel(client, remembered, scope, setup) }
        footerMachine = machines.firstOrNull()
        if (polling) machines.forEach { it.startPolling() }
        // The field follows what is in force, so a document applied on one screen is what the next
        // one shows. What the user is halfway through typing is only ever lost by their own Apply.
        configDraft = services.configText.value
    }

    private companion object {
        const val CLOCK_INTERVAL_MS = 5_000L

        /** The port every machine's sshd is on until somebody moves it. */
        const val DEFAULT_SSH_PORT = "22"

        /** How long "just now" is true for. After that the line about the handover is only noise. */
        const val HANDOVER_NOTE_MS = 300_000L
    }
}
