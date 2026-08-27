package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.data.EXAMPLE_CONTROLLER_CONFIG
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

    fun insertExample() {
        configDraft = EXAMPLE_CONTROLLER_CONFIG
        configError = null
        configNote = "An example, not a working configuration. Put your own addresses, user names, " +
            "hardware address and agent paths in it before applying it."
    }

    private fun rebuild(clients: List<MachineClient>) {
        machines.forEach { it.stopPolling() }
        machines = clients.map { client -> MachineModel(client, remembered, scope) }
        footerMachine = machines.firstOrNull()
        if (polling) machines.forEach { it.startPolling() }
        // The field follows what is in force, so a document applied on one screen is what the next
        // one shows. What the user is halfway through typing is only ever lost by their own Apply.
        configDraft = services.configText.value
    }

    private companion object {
        const val CLOCK_INTERVAL_MS = 5_000L
    }
}
