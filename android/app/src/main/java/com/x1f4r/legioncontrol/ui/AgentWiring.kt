package com.x1f4r.legioncontrol.ui

import android.content.Context
import com.x1f4r.legioncontrol.agent.AgentAbilities
import com.x1f4r.legioncontrol.agent.AgentActionResult
import com.x1f4r.legioncontrol.agent.AgentDeploymentPath
import com.x1f4r.legioncontrol.agent.AgentFailure
import com.x1f4r.legioncontrol.agent.AgentReply
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ConfigMetaReply
import com.x1f4r.legioncontrol.agent.ConfigSetReply
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.DoctorReport
import com.x1f4r.legioncontrol.agent.EndpointDiagnosis
import com.x1f4r.legioncontrol.agent.HistoryReply
import com.x1f4r.legioncontrol.agent.InterruptRequest
import com.x1f4r.legioncontrol.agent.LaunchGate
import com.x1f4r.legioncontrol.agent.LegionControl
import com.x1f4r.legioncontrol.agent.LogsReply
import com.x1f4r.legioncontrol.agent.Machine
import com.x1f4r.legioncontrol.agent.MachineControl
import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.PolicyPatch
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.agent.SignedTarball
import com.x1f4r.legioncontrol.agent.UpdateRequest
import com.x1f4r.legioncontrol.agent.readControllerReply
import com.x1f4r.legioncontrol.agent.resolveSystem
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
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject

/**
 * The one seam between the screen and the transport.
 *
 * The transport throws and the screen wants results; the transport hands back the endpoint it used
 * and the screen wants to know how to name it. Both differences are settled here, in one file, so
 * that neither side has to be written around the other.
 */
fun createControlServices(context: Context): ControlServices = WiredServices(LegionControl(context))

private class WiredServices(private val control: LegionControl) : ControlServices {

    override suspend fun publicKey(): String = control.identity.identity().authorizedKeysLine

    override suspend fun keyFingerprint(): String = control.identity.identity().fingerprint

    override suspend fun keyChange(): KeyChange? = control.identity.change()

    override suspend fun acknowledgeKeyChange() = control.identity.acknowledgeChange()

    override suspend fun replaceUnreadableKey() {
        control.identity.replaceUnreadable()
    }

    override val config: StateFlow<ControllerConfig?> get() = control.config.config

    override val launch: StateFlow<LaunchGate.Effective> get() = control.config.launch

    override val launchProblem: StateFlow<String?> get() = control.config.launchProblem

    override val configText: StateFlow<String> get() = control.config.text

    override val provenance: StateFlow<SetupProvenance?> get() = control.config.provenance

    override val setupSource: StateFlow<SetupSource?> get() = control.config.source

    override val operations: OperationStore get() = control.operations

    override val document: StateFlow<ControllerDocument?> get() = control.config.document

    override val divergence: StateFlow<SetupDivergence?> get() = control.config.divergence

    override val identityClash: StateFlow<SetupIdentityClash?> get() = control.config.identityClash

    override fun networkAddresses(): Flow<List<String>> = control.homeNetwork.observeAddresses()

    override fun deviceName(): String = control.deviceName()

    override fun forgetRoute(machineId: String) = control.settings.forgetRoute(machineId)

    override fun prepareRoute(machineId: String, systemId: String, onSite: Boolean) =
        control.prepareRoute(machineId, systemId, onSite)

    override fun applyPasted(text: String, deviceName: String?): String? =
        control.config.applyPasted(text, deviceName)

    override fun applyEdit(document: ControllerDocument, deviceName: String?): String? =
        control.config.applyEdit(document, deviceName)

    override fun applyEnrolment(text: String, deviceName: String?): String? =
        control.config.applyEnrolment(text, deviceName)

    override fun approveLaunch(entries: List<LaunchReviewEntry>): String? = control.config.approveLaunch(entries)

    override val acceptance: StateFlow<DocumentAcceptance.Verdict?> get() = control.config.acceptance

    override fun appliedHash(): String? = control.config.appliedHash()

    override fun adoptFromMachine(
        text: String,
        expectedRemoteHash: String?,
        expectedLocalHash: String?,
        machineName: String,
    ): SetupOutcome = control.config.adoptFromMachine(text, expectedRemoteHash, expectedLocalHash, machineName)

    override fun recordDivergence(machineId: String, machineName: String, reason: String, theirText: String) =
        control.config.recordDivergence(machineId, machineName, reason, theirText)

    override fun recordIdentityClash(machineId: String, machineName: String, reason: String, theirText: String) =
        control.config.recordIdentityClash(machineId, machineName, reason, theirText)

    override fun clearDivergence() = control.config.clearDivergence()

    override fun clearIdentityClash() = control.config.clearIdentityClash()

    /** Take theirs: the verified bytes the screen showed, over the document the screen was built against. */
    override fun adoptDivergent(divergence: SetupDivergence): SetupOutcome =
        control.config.adoptFromMachine(
            text = divergence.theirText,
            expectedRemoteHash = divergence.theirProvenance.hash,
            expectedLocalHash = divergence.mineProvenance?.hash,
            machineName = divergence.machineName,
        ).also { if (it is SetupOutcome.Applied || it == SetupOutcome.Unchanged) control.config.clearDivergence() }

    override fun applyMerge(mergedText: String, expectedHead: String?): SetupOutcome =
        control.config.applyMerge(mergedText, expectedHead)

    override fun adoptOtherSetup(clash: SetupIdentityClash): SetupOutcome =
        control.config.adoptFromMachine(
            text = clash.theirText,
            expectedRemoteHash = clash.theirProvenance.hash,
            expectedLocalHash = clash.mineProvenance?.hash,
            machineName = clash.machineName,
        ).also { if (it is SetupOutcome.Applied || it == SetupOutcome.Unchanged) control.config.clearIdentityClash() }

    override fun revision(hash: String?): String? = control.config.revision(hash)

    /**
     * The address is remembered as soon as a machine serves a document, not once that document has
     * been accepted. It answered and it had the setup, which is everything this field is for; a
     * document that then fails validation is a fault on the far side, and making the user type the
     * address again is no part of fixing it.
     */
    override suspend fun fetchSetupFrom(source: SetupSource): Result<ControllerFetch> = try {
        val fetched = control.fetchSetup(source)
        if (fetched is ControllerFetch.Document) control.config.rememberSource(source)
        Result.success(fetched)
    } catch (failure: AgentFailure) {
        Result.failure(failure)
    }

    /**
     * One key for an address given by hand. It is one system until a configuration says otherwise,
     * and the configuration that is about to arrive sets the capacity for every address it names.
     */
    override fun configureHostSystems(address: String, systems: List<com.x1f4r.legioncontrol.data.HostIdentitySystem>, revision: Long) = control.hostKeys.configureSystems(address, systems, revision)
    override fun hostTrust(address: String) = control.hostKeys.snapshot(address)
    override suspend fun trustHostKey(address: String, keyBlobBase64: String, approval: com.x1f4r.legioncontrol.data.HostTrustApproval) {
        control.hostKeys.approve(address, keyBlobBase64, approval)
    }

    override fun trustedHostKeys(): Map<String, List<String>> = control.hostKeys.fingerprints()

    override fun forgetHostKeys(address: String) = control.hostKeys.forget(address)

    override fun clients(launch: LaunchGate.Effective): List<MachineClient> =
        control.controls(launch).map { WiredMachineClient(control, it) }
}

private class WiredMachineClient(
    private val control: LegionControl,
    private val wiring: MachineControl,
) : MachineClient {

    override val machine: Machine get() = wiring.machine

    override val abilities: AgentAbilities get() = wiring.agent.abilities

    override fun wakeReadiness(): Flow<WakeReadiness> =
        control.homeNetwork.observe(machine.wake?.lanPrefix).map { state ->
            WakeReadiness(possible = state.onHomeNetwork, explanation = state.explanation)
        }

    @Volatile
    private var lastRoute: Route? = null

    override val route: Route? get() = lastRoute

    override suspend fun status(): Result<AgentStatus> = attempt { wiring.agent.status() }

    override suspend fun update(
        request: UpdateRequest,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.update(request, operationId) }

    override suspend fun restart(
        serviceId: String?,
        request: InterruptRequest,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.restart(serviceId, request, operationId) }

    override suspend fun setAutoUpdate(
        on: Boolean,
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.setAutoUpdate(on, serviceId, operationId) }

    override suspend fun pauseUpdates(
        duration: String,
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.pauseUpdates(duration, serviceId, operationId) }

    override suspend fun resumeUpdates(
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.resumeUpdates(serviceId, operationId) }

    override suspend fun writePolicy(
        patch: PolicyPatch,
        serviceId: String?,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.writePolicy(patch, serviceId, operationId) }

    override suspend fun readPolicy(serviceId: String?): Result<AgentActionResult> =
        attempt { wiring.agent.readPolicy(serviceId) }

    override suspend fun boot(
        targetId: String,
        request: InterruptRequest,
        noReboot: Boolean,
        operationId: String?,
    ): Result<AgentActionResult> =
        attempt { wiring.agent.boot(targetId, request, noReboot, operationId) }

    override suspend fun sleep(
        request: InterruptRequest,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.sleep(request, operationId) }

    override suspend fun run(
        actionId: String,
        request: InterruptRequest,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.run(actionId, request, operationId) }

    override suspend fun operation(operationId: String, waitSeconds: Int?): Result<OpRecord> =
        attempt { wiring.agent.operation(operationId, waitSeconds) }

    override suspend fun configMeta(): Result<ConfigMetaReply> =
        attempt { wiring.agent.configMeta() }

    override suspend fun configSet(
        canonicalDocument: String,
        setupId: String,
        revision: Long,
        replace: Boolean,
    ): Result<ConfigSetReply> =
        attempt { wiring.agent.configSet(canonicalDocument, setupId, revision, replace) }

    override suspend fun cancel(operationId: String): Result<AgentActionResult> =
        attempt { wiring.agent.cancel(operationId) }

    override suspend fun cycle(dryRun: Boolean, operationId: String?): Result<AgentActionResult> =
        attempt { wiring.agent.cycle(dryRun, operationId) }

    override suspend fun history(limit: Int): Result<HistoryReply> =
        attempt { wiring.agent.history(limit) }

    override suspend fun logs(lines: Int, operationId: String?): Result<LogsReply> =
        attempt { wiring.agent.logs(lines, operationId) }

    override suspend fun serviceConfigGet(): Result<JsonObject> = attempt { wiring.agent.serviceConfigGet() }
    override suspend fun serviceConfigWrite(action: String, payload: JsonObject): Result<JsonObject> = attempt { wiring.agent.serviceConfigWrite(action, payload) }

    override suspend fun doctor(deep: Boolean): Result<DoctorReport> =
        attempt { wiring.agent.doctor(deep = deep) }

    override suspend fun bundle(): Result<String> =
        attempt { wiring.agent.bundle() }.map { Pretty.encodeToString(JsonObject.serializer(), it) }

    override suspend fun diagnose(): List<EndpointDiagnosis> =
        machine.endpoints.map { wiring.agent.diagnose(it) }

    override suspend fun fetchSetup(): Result<ControllerFetch> =
        attempt { wiring.agent.config() }.map { readControllerReply(it, source = machine.name) }

    override suspend fun sendWakePacket(broadcasts: List<String>, prefixes: List<String>): String? {
        val wake = wiring.wake ?: return "This machine has no wake configuration."
        // The machine's own network, not whatever the default route happens to be. With a tunnel up
        // the default is the tunnel, and a broadcast put on that reaches nothing.
        return wake.send(
            via = control.homeNetwork.networkForPrefixes(prefixes),
            broadcasts = broadcasts,
        )
    }

    override suspend fun waitForWake(): Boolean = withTimeoutOrNull(WAKE_WAIT_MILLIS) {
        while (true) {
            delay(WAKE_POLL_MILLIS)
            val status = runCatching { wiring.agent.status() }.getOrNull()?.value
            if (status?.ok != false && status?.systemId != null) return@withTimeoutOrNull true
        }
        @Suppress("UNREACHABLE_CODE")
        false
    } ?: false

    override val wakeProbeAddress: String? get() = wiring.wake?.probeAddress

    override suspend fun wakeProxyFor(
        machineId: String,
        operationId: String?,
    ): Result<AgentActionResult> = attempt { wiring.agent.wakeProxy(machineId, operationId) }

    override suspend fun hasWakeProxy(): Boolean = try {
        wiring.agent.readVerbs()
        wiring.agent.abilities.hasWakeProxy
    } catch (_: AgentFailure) {
        false
    }

    /**
     * Puts the signed bundle on the machine, by whichever route that machine can take.
     *
     * Decided here again rather than taken from the screen, from the agent client's own last
     * authenticated status: a button that was enabled a minute ago is not permission. A machine on
     * contract 3 takes the bundle on standard input in one command. One on 2.x has no `self-update`
     * at all, so it gets the legacy bootstrap, the one path that runs code the far side has only just
     * received. Anything else is refused before a byte is sent.
     */
    override suspend fun installAgent(
        tarball: SignedTarball,
        operationId: String?,
    ): Result<AgentActionResult> {
        val status = wiring.agent.lastStatus
        val system = status?.let { machine.resolveSystem(it.systemId, it.systemName, it.osName) }
        return when (val path = AgentDeploymentPath.decide(wiring.agent.abilities, status, system, tarball.version)) {
            AgentDeploymentPath.Stdin ->
                attempt { wiring.agent.selfUpdateFromStdin(tarball.file, operationId) }

            is AgentDeploymentPath.UploadAndInstall ->
                attempt { wiring.agent.bootstrapLegacy(path.system, path.layout, tarball, operationId) }

            is AgentDeploymentPath.Unavailable ->
                Result.failure(AgentFailure.BadRequest("The control agent cannot be installed from here.", path.reason))
        }
    }

    override fun configureHostSystems(address: String, systems: List<com.x1f4r.legioncontrol.data.HostIdentitySystem>, revision: Long) = control.hostKeys.configureSystems(address, systems, revision)
    override fun hostTrust(address: String) = control.hostKeys.snapshot(address)
    override suspend fun trustHostKey(address: String, keyBlobBase64: String, approval: com.x1f4r.legioncontrol.data.HostTrustApproval) {
        control.hostKeys.approve(address, keyBlobBase64, approval)
    }

    /**
     * Runs one call and remembers which way it went.
     *
     * Only [AgentFailure] is caught. Anything else is a defect in this app rather than a fact about
     * the machine, and swallowing it into a status line at the bottom of the screen would hide it.
     */
    private suspend fun <T : Any> attempt(call: suspend () -> AgentReply<T>): Result<T> = try {
        val reply = call()
        lastRoute = Route(reply.route.label, reply.route.kind)
        Result.success(reply.value)
    } catch (failure: AgentFailure) {
        Result.failure(failure)
    }

    private companion object {
        const val WAKE_POLL_MILLIS = 3_000L
        const val WAKE_WAIT_MILLIS = 45_000L
    }
}

private val Pretty = Json {
    prettyPrint = true
    prettyPrintIndent = "  "
}
