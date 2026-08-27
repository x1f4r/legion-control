package com.x1f4r.legioncontrol.ui

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.x1f4r.legioncontrol.agent.AgentAction
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineSystem
import com.x1f4r.legioncontrol.agent.ServiceStatus
import com.x1f4r.legioncontrol.agent.resolveSystem
import com.x1f4r.legioncontrol.agent.shouldFetchSetup
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * One machine, as the screen knows it, and the only thing that talks to that machine's agent.
 *
 * It polls while the screen is on and not one moment longer. A phone is not a desk: there is no
 * service, no worker, no alarm and nothing left running in the background. When the app is put away
 * these stop dead, which is why [startPolling] and [stopPolling] are driven straight from the
 * activity's resume and pause rather than from a composable that might outlive the visible screen.
 */
@Stable
class MachineModel(
    private val client: MachineClient,
    private val settings: RememberedSettings,
    private val scope: CoroutineScope,
    /** The phone's copy of the setup, which this machine is one of the carriers of. */
    private val setup: SetupSync,
) {
    val machine: Machine get() = client.machine

    /**
     * Which action is in flight, as an identity rather than as a sentence.
     *
     * The status line already says what is happening in words, but a button cannot compare itself
     * against a sentence that includes the name of a system. This is what lets exactly the button
     * that was pressed show a spinner, in the frame the press lands, while the others simply go
     * quiet. [subject] is the service or action it was pressed on, because a machine can have
     * several of each and only one of them was touched.
     */
    enum class Task { WAKE, SLEEP, BOOT, UPDATE, RESTART, AUTO_UPDATE, TRUST_KEY, RUN }

    /** Questions asked before something interrupts work or reboots the machine. */
    sealed interface Dialog {
        val title: String
        val message: String
        val confirmLabel: String

        data class ConfirmBoot(
            val target: MachineSystem,
            val machineName: String,
            val busyReason: String?,
        ) : Dialog {
            override val title get() = "Boot into ${target.name}?"
            override val message get() = busyReason?.let {
                "$machineName looked busy at the last reading ($it). The switch is checked against " +
                    "the machine as it is now, and held back if work is still running."
            } ?: "$machineName will reboot now and come back up in ${target.name}. " +
                "This takes about a minute."
            override val confirmLabel get() = "Reboot now"
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

        /**
         * Sleep is gated exactly like a reboot, and for the same reason: it takes the machine away
         * from whatever it was doing. It is the gentlest of the three only in that Wake undoes it.
         */
        data class ConfirmSleep(val machineName: String, val busyReason: String?) : Dialog {
            override val title get() = "Put $machineName to sleep?"
            override val message get() = busyReason?.let {
                "$machineName looked busy at the last reading ($it). Sleep is checked against the " +
                    "machine as it is now, and held back if work is still running."
            } ?: "$machineName suspends to memory and stops answering. Wake brings it back, as " +
                "long as this phone is on its network."
            override val confirmLabel get() = "Sleep now"
        }

        data class OfferForceSleep(override val message: String) : Dialog {
            override val title get() = "Put it to sleep anyway?"
            override val confirmLabel get() = "Sleep anyway"
        }

        /**
         * An action the agent offered, with the sentence the agent's own configuration wrote for
         * it. Asked only when there is one: an action with nothing to warn about runs on the press.
         */
        data class ConfirmRun(val action: AgentAction, override val message: String) : Dialog {
            override val title get() = "${action.displayName}?"
            override val confirmLabel get() = "Run it"
        }

        data class OfferForceRun(val action: AgentAction, override val message: String) : Dialog {
            override val title get() = "${action.displayName} anyway?"
            override val confirmLabel get() = "Run it anyway"
        }

        data class ConfirmTrustHostKey(
            val address: String,
            val keyBlob: String,
            val detail: String?,
        ) : Dialog {
            override val title get() = "Trust the new host key?"
            override val message get() = listOfNotNull(
                detail?.takeIf { it.isNotBlank() },
                "Accept this only if you know why the key is new. A system that has never been " +
                    "trusted on this address explains it, and nothing else does. The keys already " +
                    "trusted stay, so each system asks this once.",
            ).joinToString(" ")
            override val confirmLabel get() = "Trust it"
        }
    }

    private var pollJob: Job? = null
    private var networkJob: Job? = null

    /** The last setup hash this machine reported, so one that leads nowhere is only followed once. */
    private var lastSetupHash: String? = null

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

    /**
     * Whether a reading the user asked for is in flight, which is a different question from whether
     * any reading is. The screen polls itself every fifteen seconds, and driving the pull to refresh
     * spinner off that would drop it down the page four times a minute unasked, which reads as the
     * app doing something rather than as it sitting still.
     */
    var isRefreshingVisibly: Boolean by mutableStateOf(false)
        private set

    /** Non-null while a user-initiated action runs. Doubles as the "controls are disabled" flag. */
    var activity: String? by mutableStateOf(null)
        private set

    /** Which action that is, so the button that was pressed can say so and the rest need not. */
    var task: Task? by mutableStateOf(null)
        private set

    /** Which service or action it was pressed on, when the machine has more than one of them. */
    var subject: String? by mutableStateOf(null)
        private set

    var statusLine: String by mutableStateOf("Ready.")
        private set
    var statusIsError: Boolean by mutableStateOf(false)
        private set
    var statusDetail: String? by mutableStateOf(null)
        private set
    var dialog: Dialog? by mutableStateOf(null)

    /** Follows the network, so the wake action stops lying the moment the phone leaves the house. */
    var wakeReadiness: WakeReadiness by mutableStateOf(
        WakeReadiness(possible = false, explanation = ""),
    )
        private set

    /**
     * The services this machine has, as pages: what it reported if it has answered, and what it
     * reported last time if it has not. A page that comes and goes with the machine's power state
     * would renumber the pager under the user's thumb.
     */
    var knownServices: List<RememberedService> by mutableStateOf(settings.services(client.machine.id))
        private set

    /** Ticks with the rest of the app, purely so "checked two minutes ago" stops being a lie. */
    var now: Long by mutableStateOf(System.currentTimeMillis())
        private set

    /** Set the moment a reboot is handed off, so a dead link reads as restarting rather than gone. */
    private var rebootTarget: MachineSystem? by mutableStateOf(null)
    private var rebootUntil: Long = 0L

    /**
     * Set the moment sleep is handed off. A machine that was deliberately put to sleep is not
     * reachable, and it is also not a fault, so for a while afterwards the poll that finds nothing
     * says so in those words instead of raising the amber mark at the bottom of the screen.
     */
    private var sleepUntil: Long = 0L

    val isWorking: Boolean get() = activity != null

    /** True for the one button that was pressed, so only it shows a spinner. */
    fun isBusyWith(task: Task, subject: String? = null): Boolean =
        this.task == task && this.subject == subject

    /** The system we can actually command. Every action keys off this. */
    val currentSystem: MachineSystem?
        get() = (link as? LinkState.Online)?.system

    val isAwake: Boolean
        get() = link is LinkState.Online || link is LinkState.AgentMissing

    val needsKeyAuthorisation: Boolean
        get() = link is LinkState.NeedsKeyAuthorisation

    val hostKeyChanged: LinkState.HostKeyChanged?
        get() = link as? LinkState.HostKeyChanged

    val rebootInProgress: MachineSystem?
        get() = rebootTarget?.takeIf { now < rebootUntil }

    val busyReason: String?
        get() = status?.busy?.takeIf { it.isBusy }?.summary

    /** Whether waking is on offer at all: a machine the configuration cannot wake never shows it. */
    val canWake: Boolean get() = machine.wake != null

    /** What this machine's last handover of the setup did, while that is still news. */
    val setupNote: String? get() = setup.noteFor(machine.id, now)

    /** The actions the agent offered. An older agent offers none and the block is not drawn. */
    val actions: List<AgentAction> get() = status?.actions.orEmpty().filter { !it.id.isNullOrBlank() }

    /**
     * Which systems this machine can be sent to.
     *
     * The agent's own list when it has one, narrowed to the systems the configuration knows how to
     * name and reach. An agent that predates boot targets offers no list at all, and every other
     * configured system is the right answer for it, because that is what it has always accepted.
     *
     * Empty while nothing is answering. Arming the next boot is something the running system does,
     * so a machine that is asleep has nothing to offer here rather than a row of dead buttons.
     */
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
        if (currentSystem?.id == system.id) status?.autoUpdate?.let { return it }
        return remembered[system.id]?.autoUpdate
    }

    // MARK: - Polling

    fun startPolling() {
        if (networkJob == null) {
            networkJob = scope.launch {
                client.wakeReadiness().collect { wakeReadiness = it }
            }
        }
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
     * more failed login in the machine's auth log, so those states back off. The screen still
     * recovers on its own once the key is added; it just does not hammer the door to find out.
     */
    private fun pollInterval(): Long = when (link) {
        is LinkState.NeedsKeyAuthorisation, is LinkState.HostKeyChanged -> BLOCKED_POLL_INTERVAL_MS
        else -> POLL_INTERVAL_MS
    }

    fun stopPolling() {
        pollJob?.cancel()
        pollJob = null
        networkJob?.cancel()
        networkJob = null
    }

    /** Driven by the one clock in the app rather than one of its own per machine. */
    fun tick(now: Long) {
        this.now = now
    }

    /** The pull to refresh gesture and the refresh action both land here. */
    fun refreshNow() {
        if (isWorking) return
        scope.launch { refresh(userInitiated = true) }
    }

    private suspend fun refresh(userInitiated: Boolean, duringAction: Boolean = false) {
        if (isRefreshing) {
            // A poll is already on the wire. Adopt it rather than dropping the gesture: it is asking
            // the machine the same question and it will answer in a moment, and its own exit clears
            // the spinner again. A second connection would only make the same round trip twice.
            if (userInitiated) isRefreshingVisibly = true
            return
        }
        if (!duringAction && isWorking) return
        isRefreshing = true
        if (userInitiated) {
            statusLine = "Checking ${machine.name}."
            statusIsError = false
            statusDetail = null
            isRefreshingVisibly = true
        }
        try {
            val result = client.status()
            now = System.currentTimeMillis()
            lastChecked = now
            route = client.route
            result.fold(
                onSuccess = { reply ->
                    // A reading that arrived cleanly can still be the agent reporting its own
                    // failure. Painting that as a healthy machine would be the worst kind of wrong.
                    if (reply.ok == false) {
                        status = null
                        link = LinkState.Offline(reply.message ?: "the agent reported a failure")
                        statusLine = "The control agent reported a problem."
                        statusIsError = true
                        statusDetail = reply.message ?: reply.error
                    } else {
                        applyReading(reply)
                        syncSetup(reply.controller?.hash)
                        if (userInitiated) {
                            statusLine = "Status refreshed."
                            statusIsError = false
                            statusDetail = null
                        }
                    }
                },
                onFailure = { error -> describeFailure(error) },
            )
        } finally {
            isRefreshing = false
            isRefreshingVisibly = false
        }
    }

    private fun applyReading(reply: AgentStatus) {
        // The agent's own reply is authoritative. Falling back to the system we were already talking
        // to is safe; guessing from the address is not, because every system on the machine answers
        // on the same LAN address and only one of them is ever up.
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

        rememberServices(reply)
        settings.save(
            machine.id,
            system.id,
            reply.autoUpdate ?: remembered[system.id]?.autoUpdate,
            System.currentTimeMillis(),
        )
        remembered = settings.load(machine.id, machine.systems.map { it.id })
    }

    /**
     * Takes the setup from this machine when it is carrying one the phone has not got.
     *
     * Every machine carries the document, so the one that has just answered is also the cheapest
     * place to read it from: the link is up, the route is remembered, and this is one more command
     * over it. It is the whole of what keeps an edit made on the Mac reaching the phone without
     * anybody pasting anything.
     *
     * The hash is written down before the fetch and only forgotten again when nothing came back,
     * which is what keeps a document that will not validate from being fetched every fifteen
     * seconds for as long as it stays wrong: it is asked for once and refused once. A round trip
     * that failed taught us nothing about the document, so that one is worth repeating.
     */
    private suspend fun syncSetup(reported: String?) {
        if (!shouldFetchSetup(reported, setup.appliedHash, lastSetupHash)) return
        lastSetupHash = reported
        val reply = client.fetchSetup().getOrNull()
        if (reply == null) {
            // The document was not read at all: a link that went away between two commands, or one
            // that did not finish in time. Nothing was learned about the document, so the hash is
            // forgotten again and the next poll asks once more. Only a document that did arrive and
            // was refused stays remembered, because asking for it again gets it refused again.
            lastSetupHash = null
            return
        }
        val fetched = reply as? ControllerFetch.Document ?: return
        val refused = setup.applyFromMachine(fetched.text, fetched.hash)
        setup.remember(
            machine.id,
            refused?.let { "${machine.name} carries a setup this app cannot use. $it" }
                ?: "Setup updated from ${machine.name} just now.",
        )
    }

    /**
     * The service pages this machine gets.
     *
     * Only written when the reading actually described services, so a system that has none does not
     * take away the pages the other system on the same machine has: the pager belongs to the
     * machine, and half a dual boot answering must not renumber it.
     */
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
                val reboot = rebootInProgress
                when {
                    reboot != null -> {
                        statusLine = "Waiting for ${reboot.name} to come back."
                        statusIsError = false
                    }

                    System.currentTimeMillis() < sleepUntil -> {
                        statusLine = "${machine.name} is asleep. Wake brings it back."
                        statusIsError = false
                        statusDetail = null
                    }

                    else -> {
                        statusLine = error.sentence
                        statusIsError = true
                    }
                }
            }

            else -> {
                statusLine = error.sentence
                statusIsError = true
            }
        }
    }

    // MARK: - Actions

    fun wake() {
        run(Task.WAKE, "Waking ${machine.name}") {
            statusLine = "Wake packet sent. Waiting for ${machine.name} to answer."
            client.wake().fold(
                onSuccess = {
                    statusLine = "${machine.name} is awake."
                    statusIsError = false
                    rebootTarget = null
                    sleepUntil = 0L
                },
                onFailure = { error ->
                    statusLine = error.sentence
                    statusIsError = true
                    statusDetail = error.rawDetail
                },
            )
            refreshQuietly()
        }
    }

    fun requestSleep() {
        dialog = Dialog.ConfirmSleep(machine.name, busyReason)
    }

    /**
     * Suspend the machine, gated by the agent exactly as a reboot is.
     *
     * Not forced on the first attempt, for the reason every action here shares: nothing polls while
     * the app is away, so the busy flag on this screen can be minutes old. The agent checks the
     * machine as it is at the moment the command lands, and if it says no, the question is asked.
     *
     * A suspend takes the connection with it, so a dropped link after the command was accepted is
     * what success looks like. That is only read as success when the agent had no reason to refuse,
     * which is the same test the boot action applies for the same reason.
     */
    fun sleep(force: Boolean) {
        run(Task.SLEEP, "Putting ${machine.name} to sleep") {
            var settled = false
            client.sleep(force).fold(
                onSuccess = { result ->
                    statusDetail = result.message ?: result.error
                    when (result.action) {
                        "sleeping" -> {
                            expectSleep()
                            settled = true
                        }

                        "deferred" -> {
                            val reason = result.message ?: "the machine is busy."
                            statusLine = "Sleep was held back: $reason"
                            statusIsError = false
                            statusDetail = null
                            if (!force) {
                                settled = true
                                dialog = Dialog.OfferForceSleep(
                                    "$reason Sleeping now interrupts that work and what is in " +
                                        "progress is lost.",
                                )
                            }
                        }

                        else -> {
                            statusLine = "${machine.name} did not go to sleep."
                            statusIsError = true
                        }
                    }
                },
                onFailure = { error ->
                    val dropped = error.toLinkState() is LinkState.Offline
                    if (dropped && (force || busyReason == null)) {
                        expectSleep()
                        settled = true
                    } else {
                        report(error)
                    }
                },
            )
            // Nothing to re-read in either settled case: the machine is either on its way down, in
            // which case a status call would only produce a connection failure that overwrites the
            // sentence just written, or it refused and changed nothing while a question is on screen.
            if (!settled) refreshQuietly()
        }
    }

    private fun expectSleep() {
        rebootTarget = null
        sleepUntil = System.currentTimeMillis() + SLEEP_GRACE_MS
        link = LinkState.Offline("Asleep.")
        status = null
        statusLine = "${machine.name} is going to sleep. Wake brings it back."
        statusIsError = false
    }

    fun requestBoot(target: MachineSystem) {
        dialog = Dialog.ConfirmBoot(target, machine.name, busyReason)
    }

    fun boot(target: MachineSystem, force: Boolean) {
        run(Task.BOOT, "Switching to ${target.name}", target.id) {
            client.boot(target.id, force).fold(
                onSuccess = { result ->
                    statusDetail = result.message ?: result.error
                    when (result.action) {
                        "rebooting" -> expectReboot(target, result.message)
                        "armed" -> {
                            statusLine = "The next boot is set to ${target.name}. " +
                                "Nothing has rebooted yet."
                            statusIsError = false
                        }

                        "noop" -> {
                            statusLine = "${target.name} is already running."
                            statusIsError = false
                        }

                        "deferred" -> {
                            val reason = result.message ?: "${machine.name} is busy."
                            statusLine = "The switch was held back: $reason"
                            statusIsError = false
                            statusDetail = null
                            dialog = Dialog.OfferForceBoot(
                                target,
                                "$reason Switching now interrupts that work and what is in " +
                                    "progress is lost.",
                            )
                        }

                        else -> {
                            statusLine = "The switch to ${target.name} failed."
                            statusIsError = true
                        }
                    }
                },
                onFailure = { error ->
                    // A reboot can cut the link before the command returns, which is exactly what
                    // success looks like once the machine has taken the instruction.
                    val dropped = error.toLinkState() is LinkState.Offline
                    if (dropped && (force || busyReason == null)) {
                        expectReboot(
                            target,
                            "The connection dropped, which is what a reboot looks like.",
                        )
                    } else {
                        report(error)
                    }
                },
            )
        }
    }

    private fun expectReboot(target: MachineSystem, detail: String?) {
        rebootTarget = target
        rebootUntil = System.currentTimeMillis() + REBOOT_GRACE_MS
        link = LinkState.Offline("Restarting into ${target.name}.")
        status = null
        statusLine = "Rebooting into ${target.name}. This takes about a minute."
        statusIsError = false
        statusDetail = detail
    }

    fun requestRestart(service: ServiceStatus) {
        dialog = Dialog.ConfirmRestart(service, machine.name, busyReason)
    }

    /**
     * Not forced on the first attempt, for the same reason the update is not.
     *
     * A restart kills whatever the service is in the middle of, and the only thing that knows
     * whether it is in the middle of anything is the agent, checking the machine at the moment the
     * command lands. Nothing polls while the app is away, so the busy flag on this screen can be
     * minutes old, and the agent fails closed when it cannot read the probe at all. Send the plain
     * restart, and if the agent says no, ask.
     */
    fun restart(service: ServiceStatus, force: Boolean) {
        val id = service.id
        run(Task.RESTART, "Restarting ${service.displayName}", id) {
            var awaitingDecision = false
            client.restart(serviceArgument(id), force).fold(
                onSuccess = { result ->
                    statusDetail = result.message ?: result.error
                    when (result.action) {
                        "restarted" -> {
                            statusLine = "${service.displayName} restarted."
                            statusIsError = false
                        }

                        "deferred" -> {
                            val reason = result.message ?: "the machine is busy."
                            statusLine = "The restart was held back: $reason"
                            statusIsError = false
                            statusDetail = null
                            if (!force) {
                                awaitingDecision = true
                                dialog = Dialog.OfferForceRestart(
                                    service,
                                    "$reason Restarting now interrupts that work and what is in " +
                                        "progress is lost.",
                                )
                            }
                        }

                        else -> {
                            statusLine = "The restart failed."
                            statusIsError = true
                        }
                    }
                },
                onFailure = { report(it) },
            )
            // Nothing changed on the machine when the agent held the restart back, so there is
            // nothing to re-read. Skipping it also keeps this action from still being in flight
            // while the question is on screen, which would swallow the answer.
            if (!awaitingDecision) refreshQuietly()
        }
    }

    /**
     * Deliberately not forced, and deliberately not judged from what was last read.
     *
     * Nothing polls while the app is away, so a cached busy flag can be minutes old and work that
     * started since then would be invisible here. Send the plain update, let the agent run its own
     * check against the machine as it is right now, and if it says no, ask. That turns force into
     * something the user chose rather than something a stale reading let through.
     */
    fun requestUpdate(service: ServiceStatus) {
        update(service, force = false)
    }

    fun update(service: ServiceStatus, force: Boolean) {
        val id = service.id
        run(Task.UPDATE, "Updating ${service.displayName}", id) {
            var awaitingDecision = false
            client.update(serviceArgument(id), force).fold(
                onSuccess = { result ->
                    val from = result.from ?: "the installed version"
                    val to = result.to ?: "the newest build"
                    when (result.action) {
                        "updated" -> {
                            statusLine = "${service.displayName} updated from $from to $to."
                            statusIsError = false
                        }

                        // The agent says "current" when it looked and there was nothing to do, and
                        // "noop" when it did not have to look. Both mean the same thing here.
                        "current", "noop" -> {
                            statusLine = "${service.displayName} is already up to date."
                            statusIsError = false
                        }

                        "deferred" -> {
                            val reason = result.message ?: "the machine is busy."
                            statusLine = "The update was held back: $reason"
                            statusIsError = false
                            if (!force) {
                                awaitingDecision = true
                                dialog = Dialog.OfferForceUpdate(
                                    service,
                                    "$reason Updating now stops the service first, so that work " +
                                        "is lost.",
                                )
                            }
                        }

                        "rolled-back" -> {
                            statusLine = "The update failed and the previous version was put back."
                            statusIsError = true
                        }

                        else -> {
                            statusLine = "The update failed."
                            statusIsError = true
                        }
                    }
                    statusDetail = result.message ?: result.error
                },
                onFailure = { report(it) },
            )
            // The agent installed nothing when it held the update back, so there is nothing to
            // re-read, and staying in flight while the question is on screen would swallow the answer.
            if (!awaitingDecision) refreshQuietly()
        }
    }

    /**
     * An id for `--service`, or null.
     *
     * Null whenever the reading came from an agent that predates services. That agent describes one
     * service and refuses any flag it does not know by name, so naming the service it just told us
     * about would fail the command outright.
     */
    private fun serviceArgument(id: String?): String? =
        id?.takeIf { status?.reportsServices == true }

    fun requestRun(action: AgentAction) {
        val confirm = action.confirm?.takeIf { it.isNotBlank() }
        if (confirm == null) {
            runAction(action, force = false)
        } else {
            dialog = Dialog.ConfirmRun(action, confirm)
        }
    }

    /** The same gate as everything else here: try it plainly, and ask if the agent holds it back. */
    fun runAction(action: AgentAction, force: Boolean) {
        val id = action.id ?: return
        run(Task.RUN, action.displayName, id) {
            var awaitingDecision = false
            client.run(id, force).fold(
                onSuccess = { result ->
                    statusDetail = result.message ?: result.error
                    when (result.action) {
                        "ran" -> {
                            statusLine = "${action.displayName} ran."
                            statusIsError = false
                        }

                        "deferred" -> {
                            val reason = result.message ?: "the machine is busy."
                            statusLine = "${action.displayName} was held back: $reason"
                            statusIsError = false
                            statusDetail = null
                            if (!force) {
                                awaitingDecision = true
                                dialog = Dialog.OfferForceRun(
                                    action,
                                    "$reason Running it now interrupts that work.",
                                )
                            }
                        }

                        else -> {
                            statusLine = "${action.displayName} failed."
                            statusIsError = true
                        }
                    }
                },
                onFailure = { report(it) },
            )
            if (!awaitingDecision) refreshQuietly()
        }
    }

    fun setAutoUpdate(enabled: Boolean, on: MachineSystem) {
        if (currentSystem?.id != on.id) return
        run(Task.AUTO_UPDATE, "Saving the update setting", on.id) {
            // The agent's own reply carries the value it ended up with, which is the only thing a
            // follow-up status call would have told us about this. Re-reading the whole machine over
            // a second ssh connection to learn a boolean we were just handed is a round trip for
            // nothing, so it only happens in the one case where the reply did not say.
            var confirmed = false
            client.setAutoUpdate(enabled).fold(
                onSuccess = { result ->
                    val value = result.autoUpdate ?: enabled
                    confirmed = result.autoUpdate != null
                    settings.save(machine.id, on.id, value, System.currentTimeMillis())
                    remembered = settings.load(machine.id, machine.systems.map { it.id })
                    status = status?.copy(autoUpdate = value)
                    statusLine = if (value) {
                        "${on.name} will update on its own."
                    } else {
                        "Automatic updates are off on ${on.name}."
                    }
                    statusIsError = false
                    statusDetail = result.message
                },
                onFailure = { report(it) },
            )
            if (!confirmed) refreshQuietly()
        }
    }

    fun requestTrustHostKey() {
        val changed = hostKeyChanged ?: return
        dialog = Dialog.ConfirmTrustHostKey(changed.address, changed.offeredKeyBlob, statusDetail)
    }

    fun trustHostKey(address: String, keyBlob: String) {
        run(Task.TRUST_KEY, "Trusting the new host key") {
            client.trustHostKey(address, keyBlob)
            statusLine = "The new key for $address is trusted, alongside the ones already known."
            statusIsError = false
            statusDetail = null
            refresh(userInitiated = false, duringAction = true)
        }
    }

    // MARK: - Plumbing

    /**
     * Every action goes through here, and the acknowledgement is written before the coroutine is
     * even launched. That is deliberate: the state change happens inside the press, so the button
     * has its spinner and the footer has its sentence in the same frame the touch landed, rather
     * than whenever the first ssh packet comes back.
     */
    private fun run(task: Task, label: String, subject: String? = null, work: suspend () -> Unit) {
        if (isWorking) return
        this.task = task
        this.subject = subject
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

    /** Re-reads the status without disturbing the sentence the action just wrote. */
    private suspend fun refreshQuietly() {
        val line = statusLine
        val isError = statusIsError
        val detail = statusDetail
        refresh(userInitiated = false, duringAction = true)
        statusLine = line
        statusIsError = isError
        statusDetail = detail
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

        /**
         * How long an unreachable machine still reads as "asleep" rather than as a problem. Long
         * enough to cover looking at the phone again a few minutes later, short enough that a
         * machine that never woke up stops being explained away.
         */
        private const val SLEEP_GRACE_MS = 600_000L
    }
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
