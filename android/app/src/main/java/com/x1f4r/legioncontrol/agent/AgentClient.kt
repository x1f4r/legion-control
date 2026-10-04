package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.MachineHints
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.CommandTransport
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.StdinSource
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonObject
import java.io.File
import java.util.concurrent.ConcurrentHashMap

/** An answer, plus how it was obtained, because the UI is expected to show the route it used. */
data class AgentReply<T>(
    val value: T,
    val route: Endpoint,
    /** Straight out of the agent's own reply. Never inferred from the address. */
    val systemId: String?,
)

/**
 * How an update was asked for, which is three separate ideas rather than one boolean.
 *
 * The old arrangement had exactly two: plain, which an agent with the schedule turned off refuses,
 * and `--force`, which skips the busy check as well. Under contract 3 a manual request ignores the
 * schedule, the pause and the windows outright and is still busy gated, so there is no longer any
 * reason to reach for force in order to update a machine that does not update itself.
 */
data class UpdateRequest(
    val serviceId: String? = null,
    /** Skips the busy gate and nothing else. Never the lock, never a missing postcondition. */
    val force: Boolean = false,
    /** Wait for the machine to be idle rather than refusing. */
    val whenIdle: Boolean = false,
    /** The `DUR` form, e.g. `4h`. Only meaningful with [whenIdle]. */
    val expires: String? = null,
    /** Run it in the background and poll `op` for the result. Always true for updates on 3.x. */
    val detach: Boolean = false,
)

/** The same ideas for the changes that interrupt rather than install. */
data class InterruptRequest(
    val force: Boolean = false,
    val whenIdle: Boolean = false,
    val expires: String? = null,
    val detach: Boolean = false,
)

/**
 * The agent contract, spoken over ssh.
 *
 * One JSON object per invocation, one command per connection. The command line is built rather than
 * pasted together: an argv is not a string, and joining one with spaces works right up to the first
 * path with a space in it. See [buildRemoteCommand]. Every value this app puts on a command line is
 * also checked against the contract's token grammar first, so an id out of a document can never
 * become a second command whatever the quoting does, and a value that fails is a refused request
 * rather than an exception nobody catches.
 *
 * Everything added in contract 3 is gated on the far side saying `"contract": 3`. A 2.x agent
 * refuses a flag it does not recognise rather than ignoring it, so one unknown flag fails the whole
 * command, and every v3 flag below is therefore behind [abilities].
 */
class AgentClient(
    private val machine: Machine,
    private val transport: CommandTransport,
    private val routes: RouteSelector,
    private val settings: MachineHints,
    /**
     * Milliseconds, for the one deadline each command has. Replaced in tests, and only there.
     *
     * Monotonic rather than the wall clock. One deadline now spans every attempt, so a clock set
     * forward by NTP or the network in the middle of a walk would cut a read short, and one set back
     * would give the later attempts more time than the deadline has.
     */
    private val clock: () -> Long = { System.nanoTime() / 1_000_000L },
) {
    /**
     * What the last reply said the far side is.
     *
     * Held here rather than passed in on every call because it is a property of the machine and it
     * only changes when the machine is upgraded. It starts unknown, which is the old contract, so
     * the first call of a session is always one the first version of the agent would have accepted.
     */
    @Volatile
    var abilities: AgentAbilities = AgentAbilities.unknown
        private set

    /**
     * The routes whose last authenticated status said they arrived through the restricted
     * dispatcher, by endpoint id.
     *
     * A key restricted on the far side and not marked so in the document is still restricted, and a
     * command line written for a shell is one the dispatcher refuses. Once a status over a route has
     * said so, every system on that route is written in the dispatcher grammar (contract section
     * 15.1), which is the only way the run time is allowed to change a grammar.
     */
    private val restrictedRoutes = ConcurrentHashMap<String, Boolean>()

    // MARK: - Reads

    /**
     * A snapshot of the machine, inside the one 30 second deadline a status has.
     *
     * Against an agent known to speak contract 3, every attempt sends the `--budget-ms` that the time
     * left still allows (section 15.5, step 3), so a working route that comes third is not cut off by
     * this app's own deadline.
     */
    suspend fun status(): AgentReply<AgentStatus> = call(
        listOf("status"),
        STATUS_TIMEOUT_MILLIS,
        AgentStatus.serializer(),
        CommandSafety.READ,
        budgeted = true,
    ).also { reply ->
        abilities = AgentAbilities.of(reply.value).copy(verbs = abilities.verbs)
        restrictedRoutes[reply.route.id] = reply.value.agent?.restrictedSession == true
    }

    suspend fun busy(): AgentReply<BusyStatus> =
        call(listOf("busy"), STATUS_TIMEOUT_MILLIS, BusyStatus.serializer(), CommandSafety.READ)

    suspend fun version(): AgentReply<AgentVersion> =
        call(listOf("version"), SHORT_TIMEOUT_MILLIS, AgentVersion.serializer(), CommandSafety.READ)
            .also { abilities = AgentAbilities.of(it.value).copy(verbs = abilities.verbs) }

    /**
     * The verb list, read once per session and only because of one optional command.
     *
     * The wake proxy is an optional verb rather than part of contract 3 proper, so this app finds
     * out whether it exists instead of assuming either way.
     */
    suspend fun readVerbs(): Set<String> {
        if (abilities.verbs.isNotEmpty()) return abilities.verbs
        val reply = call(listOf("help"), SHORT_TIMEOUT_MILLIS, HelpReply.serializer(), CommandSafety.READ)
        val verbs = reply.value.verbs
        abilities = abilities.copy(
            contract = reply.value.contract ?: abilities.contract,
            verbs = verbs,
        )
        return verbs
    }

    /** The agent's own preflight. `--deep` adds the network checks and costs seconds. */
    suspend fun doctor(serviceId: String? = null, deep: Boolean = false): AgentReply<DoctorReport> = call(
        buildList {
            add("doctor")
            addService(serviceId)
            if (deep) add("--deep")
        },
        if (deep) DEEP_DOCTOR_TIMEOUT_MILLIS else DOCTOR_TIMEOUT_MILLIS,
        DoctorReport.serializer(),
        CommandSafety.READ,
    )

    suspend fun serviceConfigGet(): AgentReply<JsonObject> =
        call(listOf("service-config", "get"), SHORT_TIMEOUT_MILLIS, JsonObject.serializer(), CommandSafety.READ)

    suspend fun serviceConfigWrite(action: String, payload: JsonObject): AgentReply<JsonObject> {
        if (action != "validate" && action != "set") refuse("service-config takes validate or set, not \"$action\".")
        return call(listOf("service-config", action, "--stdin"), DOCTOR_TIMEOUT_MILLIS,
            JsonObject.serializer(), if (action == "set") CommandSafety.MUTATION else CommandSafety.READ,
            stdin = StdinSource.Bytes(payload.toString().toByteArray(Charsets.UTF_8)))
    }

    /** Doctor, status, history, logs and a redacted config, for the export action. */
    suspend fun bundle(): AgentReply<JsonObject> =
        call(listOf("bundle"), DEEP_DOCTOR_TIMEOUT_MILLIS, JsonObject.serializer(), CommandSafety.READ)

    /**
     * What became of an operation, by the id this app gave it.
     *
     * The command the whole recovery story rests on, and a read: asking twice what happened is
     * asking once, so it may be tried on every route without any of the mutation caution.
     *
     * [waitSeconds] long-polls until the operation finishes or the deadline passes, which is what
     * turns a detached update into something a phone can watch without hammering the machine.
     */
    suspend fun operation(operationId: String, waitSeconds: Int? = null): AgentReply<OpRecord> {
        Contract.requireToken(operationId, "operation id")
        val arguments = buildList {
            add("op")
            add(operationId)
            if (waitSeconds != null && waitSeconds > 0) {
                add("--wait")
                add(waitSeconds.toString())
            }
        }
        val budget = waitSeconds?.let { (it * 1000L) + OP_WAIT_MARGIN_MILLIS } ?: SHORT_TIMEOUT_MILLIS
        val reply = call(arguments, budget, OpEnvelope.serializer(), CommandSafety.READ)
        return AgentReply(reply.value.record(), reply.route, reply.systemId)
    }

    suspend fun history(
        limit: Int = 30,
        serviceId: String? = null,
        kind: OpKind? = null,
    ): AgentReply<HistoryReply> = call(
        buildList {
            add("history")
            add("--limit")
            add(limit.coerceIn(1, 200).toString())
            addService(serviceId)
            kind?.let {
                add("--kind")
                add(it.wire)
            }
        },
        SHORT_TIMEOUT_MILLIS,
        HistoryReply.serializer(),
        CommandSafety.READ,
    )

    suspend fun logs(lines: Int = 100, operationId: String? = null): AgentReply<LogsReply> = call(
        buildList {
            add("logs")
            add("--lines")
            add(lines.coerceIn(1, 500).toString())
            operationId?.takeIf { it.isNotBlank() }?.let {
                add("--op")
                add(Contract.requireToken(it, "operation id"))
            }
        },
        SHORT_TIMEOUT_MILLIS,
        LogsReply.serializer(),
        CommandSafety.READ,
    )

    /**
     * The setup this machine carries, as the raw reply.
     *
     * Raw rather than decoded, because the three things this reply can say are told apart by which
     * keys are there rather than by their values: an agent too old to know the command answers with
     * an object of its own, and a class with a nullable field cannot tell that apart from a machine
     * that is carrying nothing. [readControllerReply] does the reading.
     */
    suspend fun config(): AgentReply<JsonObject> =
        call(listOf("config"), SHORT_TIMEOUT_MILLIS, JsonObject.serializer(), CommandSafety.READ)

    /** Identity, revision, ancestry and hash without the document, for deciding descent. */
    suspend fun configMeta(): AgentReply<ConfigMetaReply> =
        call(listOf("config", "meta"), SHORT_TIMEOUT_MILLIS, ConfigMetaReply.serializer(), CommandSafety.READ)

    /**
     * Publishes a document to this machine.
     *
     * The bytes go on standard input, which is what keeps a whole JSON document off a command line
     * and what lets a restricted key publish at all. The id and the revision go as flags and have to
     * match the block inside the document; the agent checks that they do and refuses otherwise,
     * which is what stops a client from claiming one revision and sending another.
     *
     * [replace] is the explicit human decision to overwrite a different setup, and this app never
     * passes it without one. It is not a way past a divergence: divergence is merged, not replaced.
     */
    suspend fun configSet(
        canonicalDocument: String,
        setupId: String,
        revision: Long,
        replace: Boolean = false,
    ): AgentReply<ConfigSetReply> {
        Contract.requireToken(setupId, "setup id")
        if (revision < 0) refuse("A setup revision counts up from zero, and this one is $revision.")
        return call(
            buildList {
                add("config")
                add("set")
                add("--controller-id")
                add(setupId)
                add("--revision")
                add(revision.toString())
                if (replace) add("--replace")
            },
            SHORT_TIMEOUT_MILLIS,
            ConfigSetReply.serializer(),
            CommandSafety.MUTATION,
            stdin = StdinSource.Bytes(canonicalDocument.toByteArray(Charsets.UTF_8)),
        )
    }

    /** The effective update policy, machine-wide or for one service. */
    suspend fun readPolicy(serviceId: String? = null): AgentReply<AgentActionResult> = call(
        buildList {
            add("policy")
            addService(serviceId)
        },
        SHORT_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.READ,
    )

    // MARK: - Mutations

    /**
     * Writes an update policy from a JSON patch on standard input.
     *
     * On stdin rather than in flags because the patch has three shapes of value in it, one of which
     * is a list of objects, and none of that belongs on a command line under any quoting rules. It
     * is also what lets the restricted dispatcher allow this command at all.
     */
    suspend fun writePolicy(
        patch: PolicyPatch,
        serviceId: String? = null,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("policy")
            add("set")
            addService(serviceId)
            addOperationId(operationId)
        },
        SHORT_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
        stdin = StdinSource.Bytes(encodePolicyPatch(patch)),
    )

    suspend fun update(
        request: UpdateRequest,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("update")
            addService(request.serviceId)
            if (request.force) add("--force")
            addQueueing(request.whenIdle, request.expires)
            addDetach(request.detach)
            addOperationId(operationId)
        },
        if (request.detach && abilities.speaksV3) SHORT_TIMEOUT_MILLIS else UPDATE_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
    )

    suspend fun restart(
        serviceId: String?,
        request: InterruptRequest = InterruptRequest(),
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("restart")
            addService(serviceId)
            if (request.force) add("--force")
            addQueueing(request.whenIdle, request.expires)
            addDetach(request.detach)
            addOperationId(operationId)
        },
        if (request.detach && abilities.speaksV3) SHORT_TIMEOUT_MILLIS else RESTART_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
    )

    /**
     * `auto-update on|off`, the machine-wide or per-service scheduler switch.
     *
     * Still here beside [writePolicy] because it is the only one of the two a 2.x agent understands,
     * and because it is what the switch on screen actually means.
     */
    suspend fun setAutoUpdate(
        enabled: Boolean,
        serviceId: String? = null,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("auto-update")
            add(if (enabled) "on" else "off")
            addService(serviceId)
            addOperationId(operationId)
        },
        SHORT_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
    )

    /** `auto-update pause DUR` and `auto-update resume`. */
    suspend fun pauseUpdates(
        duration: String,
        serviceId: String? = null,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> {
        if (!Contract.isDuration(duration)) refuse("A pause is written like 30m, 4h or 2d, not \"$duration\".")
        return call(
            buildList {
                add("auto-update")
                add("pause")
                add(duration)
                addService(serviceId)
                addOperationId(operationId)
            },
            SHORT_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandSafety.MUTATION,
        )
    }

    suspend fun resumeUpdates(
        serviceId: String? = null,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("auto-update")
            add("resume")
            addService(serviceId)
            addOperationId(operationId)
        },
        SHORT_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
    )

    /**
     * Arms the next boot and reboots. The reboot cuts the connection while we are still reading, so
     * the timeout is short and a dropped link after a good reply is the expected ending, not a fault.
     *
     * Being told is not having done it: whatever comes back, the caller waits to see the machine
     * answer as the target before it says the boot happened.
     */
    suspend fun boot(
        targetId: String,
        request: InterruptRequest = InterruptRequest(),
        noReboot: Boolean = false,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> {
        Contract.requireToken(targetId, "boot target")
        return call(
            buildList {
                add("boot")
                add(targetId)
                if (request.force) add("--force")
                if (noReboot) add("--no-reboot")
                addQueueing(request.whenIdle, request.expires)
                addOperationId(operationId)
            },
            BOOT_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandSafety.MUTATION,
        )
    }

    /** Suspends the machine, once the agent has checked that nothing is running. */
    suspend fun sleep(
        request: InterruptRequest = InterruptRequest(),
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("sleep")
            if (request.force) add("--force")
            addQueueing(request.whenIdle, request.expires)
            addOperationId(operationId)
        },
        SLEEP_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
    )

    /** Runs one of the actions the agent offered. Busy gated by the agent, exactly as the rest are. */
    suspend fun run(
        actionId: String,
        request: InterruptRequest = InterruptRequest(),
        operationId: String? = null,
    ): AgentReply<AgentActionResult> {
        Contract.requireToken(actionId, "action id")
        return call(
            buildList {
                add("run")
                add(actionId)
                if (request.force) add("--force")
                addQueueing(request.whenIdle, request.expires)
                addDetach(request.detach)
                addOperationId(operationId)
            },
            ACTION_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandSafety.MUTATION,
        )
    }

    /** Withdraws a queued operation that has not started. A running one is not killable. */
    suspend fun cancel(operationId: String): AgentReply<AgentActionResult> {
        Contract.requireToken(operationId, "operation id")
        return call(
            listOf("cancel", operationId),
            SHORT_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandSafety.MUTATION,
        )
    }

    /** The scheduled maintenance cycle, run by hand. `--dry-run` reports and touches nothing. */
    suspend fun cycle(dryRun: Boolean = false, operationId: String? = null): AgentReply<AgentActionResult> =
        call(
            buildList {
                add("cycle")
                if (dryRun) add("--dry-run")
                addOperationId(operationId)
            },
            if (dryRun) SHORT_TIMEOUT_MILLIS else UPDATE_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            // A dry run changes nothing, but it still takes the same care: an agent that does not
            // know the flag would run the real thing, and that must never be retried on a second
            // route after an ambiguous failure.
            CommandSafety.MUTATION,
        )

    /**
     * Sends a signed agent bundle on standard input and installs it.
     *
     * The path a machine that already speaks contract 3 takes. Nothing is written to a temporary
     * file on the far side and no shell redirect is involved, which is what lets this work through
     * the restricted dispatcher.
     */
    suspend fun selfUpdateFromStdin(
        tarball: File,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("self-update")
            add("--stdin")
            addOperationId(operationId)
        },
        SELF_UPDATE_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
        stdin = StdinSource.Streamed { tarball.inputStream() },
    )

    /** Installs a bundle that is already on the machine, using the agent that is already there. */
    suspend fun selfUpdateFromPath(
        remotePath: String,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> = call(
        buildList {
            add("self-update")
            add("--from")
            add(remotePath)
            addOperationId(operationId)
        },
        SELF_UPDATE_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
        CommandSafety.MUTATION,
    )

    /**
     * Unpacks an uploaded bundle and lets the new tree install itself.
     *
     * The path onto a machine whose agent predates `self-update` entirely, so there is nothing there
     * that could do the install. The staged tree does it: it verifies its own signed manifest, finds
     * the base it is replacing, and swaps itself in keeping the old tree.
     *
     * Two commands rather than one, and both are visible in the operation log, because this is the
     * only path in the app that runs code the far side has just received. The interpreter is the one
     * the configuration already names for this system, so nothing new is trusted to find node.
     */
    suspend fun bootstrapAgent(
        tarballPath: String,
        stagingDirectory: String,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> {
        val system = machine.systems.firstOrNull { it.isDriven }
            ?: throw AgentFailure.AgentMissing(null, "no system on this machine names an interpreter")

        // tar is on Windows 10 and later, on macOS and on every Linux, so this is one command
        // everywhere rather than three.
        val endpoint = routes.candidates().firstOrNull()
            ?: throw AgentFailure.Unreachable(machine.name, routes.probeSummary())
        val unpack = commandLine(
            grammarOn(endpoint, system, system.agent.size),
            listOf("tar", "-xzf", tarballPath, "-C", stagingDirectory),
        )
        val unpacked = transport.run(endpoint, unpack, SELF_UPDATE_TIMEOUT_MILLIS)
        if (unpacked.timedOut) {
            throw AgentFailure.TimedOut((SELF_UPDATE_TIMEOUT_MILLIS / 1000L).toInt(), condense(unpacked.failureText))
        }
        if (unpacked.exitStatus != null && unpacked.exitStatus != 0) {
            throw AgentFailure.BadOutput(
                "unpacking the agent bundle failed: " +
                    listOf(unpacked.stderr, unpacked.stdout).firstOrNull { it.isNotBlank() },
            )
        }

        // The staged tree's own entry point, run with this system's interpreter. `--install` is the
        // form that means "you are the new agent; put yourself in place".
        val interpreter = system.agent.first()
        val staged = "$stagingDirectory/agent/src/index.mjs"
        return call(
            listOf("self-update", "--install"),
            SELF_UPDATE_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandSafety.MUTATION,
            overrideArgv = listOf(interpreter, staged),
        )
    }

    /**
     * Asks this machine to send a magic packet at another one on its own network.
     *
     * A proposed contract addition, and used only when `help` actually listed the verb. See
     * the app's fallback is an ordinary configured action on the helper machine, which needs
     * nothing new at all.
     */
    suspend fun wakeProxy(
        targetMachineId: String,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> {
        Contract.requireToken(targetMachineId, "machine id")
        return call(
            buildList {
                add("wake")
                add(targetMachineId)
                addOperationId(operationId)
            },
            WAKE_PROXY_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandSafety.MUTATION,
        )
    }

    /** Puts a file on the machine over SFTP. Used only for the agent bootstrap over a 2.x agent. */
    suspend fun upload(source: File, remotePath: String) {
        val endpoint = routes.candidates().firstOrNull()
            ?: throw AgentFailure.Unreachable(machine.name, routes.probeSummary())
        transport.upload(endpoint, source, remotePath, SELF_UPDATE_TIMEOUT_MILLIS)
    }

    /**
     * One authenticated request against exactly one address, and what it did.
     *
     * The diagnostics page runs these on demand and nothing else does. It is the authenticated
     * readiness check: it answers what a bare TCP probe answered and four more questions besides,
     * and because it only happens when a person presses a button it cannot become the background
     * traffic that gets a source penalised.
     */
    suspend fun diagnose(endpoint: Endpoint): EndpointDiagnosis {
        val started = clock()
        fun took() = clock() - started
        return try {
            val reply = call(
                listOf("version"),
                DIAGNOSE_TIMEOUT_MILLIS,
                AgentVersion.serializer(),
                CommandSafety.READ,
                only = endpoint,
            )
            EndpointDiagnosis(
                endpoint = endpoint,
                status = DiagnosisStatus.ANSWERED,
                detail = reply.value.agentVersion?.let {
                    "control agent $it, contract ${reply.value.contract ?: 2}"
                },
                millis = took(),
            )
        } catch (failure: AgentFailure) {
            val status = when (failure) {
                is AgentFailure.NotAuthorised -> DiagnosisStatus.REFUSED_KEY
                is AgentFailure.TimedOut -> DiagnosisStatus.TIMED_OUT
                is AgentFailure.HostKeyChanged -> DiagnosisStatus.HOST_KEY_CHANGED
                is AgentFailure.AgentMissing -> DiagnosisStatus.NO_AGENT
                is AgentFailure.BadOutput -> DiagnosisStatus.UNREADABLE
                is AgentFailure.BadRequest -> DiagnosisStatus.NOT_DIALLED
                else -> DiagnosisStatus.UNREACHABLE
            }
            EndpointDiagnosis(endpoint, status, failure.detail, took())
        }
    }

    // MARK: - Argument helpers

    private fun MutableList<String>.addService(serviceId: String?) {
        val id = serviceId?.takeIf { it.isNotBlank() } ?: return
        add("--service")
        add(Contract.requireToken(id, "service id"))
    }

    private fun MutableList<String>.addOperationId(operationId: String?) {
        val id = operationId?.takeIf { it.isNotBlank() } ?: return
        // A 2.x agent refuses a flag it does not know, so an id sent to one fails the whole command.
        // Where it cannot be sent, the caller is told the operation is not trackable and leaves an
        // ambiguous outcome unknown instead of repeating it.
        if (!abilities.speaksV3) return
        if (!Contract.isOpId(id)) refuse("The operation id \"$id\" is not lowercase, 8 to 64 of [a-z0-9-].")
        add("--op")
        add(id)
    }

    private fun MutableList<String>.addQueueing(whenIdle: Boolean, expires: String?) {
        if (!whenIdle || !abilities.speaksV3) return
        add("--when-idle")
        expires?.takeIf { Contract.isDuration(it) }?.let {
            add("--expires")
            add(it)
        }
    }

    private fun MutableList<String>.addDetach(detach: Boolean) {
        if (!detach || !abilities.speaksV3) return
        add("--detach")
    }

    // MARK: - Transport

    /**
     * One command, across the attempts worth making: contract section 15.5.
     *
     * Attempts are the routes in order, and on each route the systems that can be driven. The route
     * that worked last time leads and is dialled directly, with no probe in front of it. One deadline
     * covers the whole command, every attempt is given what is left of it, and once one attempt has
     * been made no other is started with less than four seconds to go.
     *
     * [safety] decides what may happen after a failure, through [AttemptWalk]. A read moves on after
     * anything. A mutation moves on only while the failure proves nothing reached the far side; the
     * moment one says it may have, the walk stops and the caller is told it is ambiguous, because
     * trying the next address is how one press becomes two restarts.
     */
    private suspend fun <T : Any> call(
        arguments: List<String>,
        timeoutMillis: Long,
        deserializer: KSerializer<T>,
        safety: CommandSafety,
        stdin: StdinSource? = null,
        /**
         * The argv to run instead of the system's configured one.
         *
         * Used by exactly one caller: the bootstrap, which has to run the tree it has just uploaded
         * rather than the one that is already installed. Everything else uses what the setup names,
         * because a machine this app has not been told about must not be reachable by it at all.
         */
        overrideArgv: List<String>? = null,
        /** `status` only: each attempt sends the `--budget-ms` the time left allows. */
        budgeted: Boolean = false,
        /** One address and nothing else, for the diagnostics page. */
        only: Endpoint? = null,
    ): AgentReply<T> {
        val deadline = clock() + timeoutMillis
        val walk = AttemptWalk(safety)
        val failures = Failures()
        val candidates = only?.let(::listOf) ?: routes.candidates()
        if (candidates.isEmpty()) {
            throw AgentFailure.Unreachable(machine.name, routes.probeSummary(), reason = FailureKind.NO_ROUTE)
        }
        var attemptsMade = 0

        routes@ for (endpoint in candidates) {
            if (!Addresses.isDialable(endpoint.host, endpoint.user, endpoint.port)) {
                // Nothing to dial, so nothing to send on any of its systems. Recorded once, and the
                // transport would refuse it in the same words if it were asked.
                routes.note(endpoint, "is not an address this app dials")
                failures.skipped(undialable(endpoint))
                continue
            }
            for (system in attemptOrder(endpoint)) {
                if (walk.skips(endpoint.id)) continue@routes
                val remaining = deadline - clock()
                if (attemptsMade > 0 && remaining < MINIMUM_ATTEMPT_MILLIS) break@routes

                val argv = (overrideArgv ?: system.agent) + arguments + if (budgeted && abilities.speaksV3) {
                    val budget = statusBudget(remaining, attemptsMade) ?: break@routes
                    listOf("--budget-ms", budget.toString())
                } else {
                    emptyList()
                }
                val line = try {
                    commandLine(grammarOn(endpoint, system, (overrideArgv ?: system.agent).size), argv)
                } catch (refused: AgentFailure.BadRequest) {
                    // Nothing was sent, and another system's line may be fine: a request that could
                    // not be made is never sent, which is what lets the walk go on.
                    failures.record(refused)
                    walk.after(endpoint.id, AttemptReading.failure(FailureKind.BAD_REQUEST, DispatchStage.NOT_STARTED))
                    continue
                }

                attemptsMade += 1
                val outcome = try {
                    transport.run(endpoint, line, remaining.coerceAtLeast(1L), stdin)
                } catch (failure: AgentFailure) {
                    noteFailure(endpoint, failure)
                    failures.record(failure)
                    val reading = AttemptReading.failure(
                        failure.kind ?: FailureKind.LINK_LOST,
                        failure.dispatch,
                    )
                    if (walk.after(endpoint.id, reading) == WalkEnding.STOPPED) throw failure
                    continue
                }

                var reading = AttemptClassifier.classify(outcome)
                if (reading.isReply) {
                    val decoded = decode(outcome, deserializer)
                    if (decoded != null) {
                        if (safety == CommandSafety.READ) requireAcceptedRead(decoded)
                        routes.remember(endpoint)
                        val reported = reportedSystem(decoded)?.also { settings.rememberSystem(machine.id, it) }
                        return AgentReply(value = decoded, route = endpoint, systemId = reported)
                    }
                    // The agent answered with an object this build cannot read. It had the command,
                    // so a change that ends here is reconciled rather than sent again.
                    reading = AttemptReading.failure(FailureKind.UNREADABLE_OUTPUT, DispatchStage.ACKNOWLEDGED)
                }

                val failure = failureFor(reading, outcome, system, remaining)
                noteFailure(endpoint, failure)
                failures.record(failure)
                if (walk.after(endpoint.id, reading) == WalkEnding.STOPPED) throw failure
            }
        }

        if (attemptsMade == 0) {
            failures.worthReporting()?.let { throw it }
            // Not a single system on this machine can be driven: every one either names no agent or
            // has a shell this app does not write for. Saying "did not answer" would send the user
            // to look at the network.
            machine.systems.firstOrNull { it.isConfigured && it.grammar == null }?.let { system ->
                throw AgentFailure.BadRequest(
                    "This app cannot write commands for ${system.name}.",
                    describeRefusal(system.grammarRefusal ?: UNKNOWN_SHELL, null),
                )
            }
        }
        throw failures.worthReporting() ?: AgentFailure.Unreachable(machine.name, routes.probeSummary())
    }

    /**
     * The grammar a system's line is written in on [endpoint].
     *
     * The document's grammar, unless the last status over this route said it came through the
     * restricted dispatcher, which moves it to the dispatcher grammar and nowhere else.
     */
    private fun grammarOn(endpoint: Endpoint, system: MachineSystem, agentLength: Int): RemoteShell {
        if (restrictedRoutes[endpoint.id] == true) {
            val choice = grammarFor(null, null, null, restrictedSession = true, agentLength = agentLength)
            return choice.grammar ?: throw AgentFailure.BadRequest(
                "This app cannot write commands for ${system.name} through a restricted key.",
                describeRefusal(choice.refusal ?: RESTRICTED_AGENT_SHAPE, null),
            )
        }
        return system.grammar ?: throw AgentFailure.BadRequest(
            "This app cannot write commands for ${system.name}.",
            describeRefusal(system.grammarRefusal ?: UNKNOWN_SHELL, null),
        )
    }

    /** One command line, or the refusal as a request that could not be made. */
    private fun commandLine(grammar: RemoteShell, argv: List<String>): String = try {
        buildRemoteCommand(grammar, argv)
    } catch (refusal: CommandLineRefusal) {
        // Nothing was sent, and the fix is a field in the document.
        throw AgentFailure.BadRequest(
            "This app cannot write that command for the shell on the far side.",
            refusal.message,
        )
    }

    /** The reply decoded, or null when it is a JSON object this build cannot read as [deserializer]. */
    private fun <T : Any> decode(outcome: CommandOutcome, deserializer: KSerializer<T>): T? {
        val json = sliceJsonObject(outcome.stdout) ?: return null
        return try {
            AgentJson.decodeFromString(deserializer, json)
        } catch (_: Exception) {
            null
        }
    }

    /**
     * What a failed attempt is called on screen, from what section 15.4 made of it.
     *
     * The stage comes from the reading and nothing else. Every sentence below is about what the far
     * side printed, and none of it decides whether the command may be sent again.
     */
    private fun failureFor(
        reading: AttemptReading,
        outcome: CommandOutcome,
        system: MachineSystem,
        budgetMillis: Long,
    ): AgentFailure = when (reading.kind) {
        FailureKind.TIMED_OUT -> AgentFailure.TimedOut(
            seconds = (budgetMillis / 1000L).toInt(),
            detail = "The connection was open and the command had not finished. " +
                "Partial output: ${outcome.stdout.take(200).trim()}",
        )

        FailureKind.LINK_LOST -> AgentFailure.Unreachable(
            machine.name,
            "The link closed with the command running: ${condense(outcome.failureText)}",
            stage = reading.dispatch,
            reason = FailureKind.LINK_LOST,
        )

        FailureKind.AGENT_MISSING, FailureKind.INTERPRETER_MISSING ->
            AgentFailure.AgentMissing(system, condense(outcome.failureText), reading.dispatch, reading.kind)

        else -> AgentFailure.BadOutput(
            if (reading.dispatch == DispatchStage.ACKNOWLEDGED) {
                "The reply could not be read. Output: ${condense(outcome.stdout)}"
            } else {
                condense(outcome.failureText)
            },
            stage = reading.dispatch,
        )
    }

    /** What an address did, for the next call's order and for the sentence when nothing answers. */
    private fun noteFailure(endpoint: Endpoint, failure: AgentFailure) {
        when (failure.kind) {
            // Only an address that did not get as far as a session is held back. One that answered
            // and turned something down is answering, and holding it back would hide that.
            FailureKind.HOST_UNREACHABLE, FailureKind.CONNECTION_REFUSED, FailureKind.NO_ROUTE ->
                routes.penalise(endpoint, "did not answer")

            FailureKind.AUTHENTICATION_FAILED -> routes.note(endpoint, "refused this phone's key")
            FailureKind.HOST_KEY_CHANGED, FailureKind.HOST_KEY_UNKNOWN ->
                routes.note(endpoint, "offered an untrusted host key")

            FailureKind.TIMED_OUT -> routes.note(endpoint, "did not finish in time")
            FailureKind.LINK_LOST -> routes.note(endpoint, "dropped the link with the command running")
            FailureKind.AGENT_MISSING -> routes.note(endpoint, "answered without the control agent installed")
            FailureKind.UNREADABLE_OUTPUT -> routes.note(endpoint, "answered with something unreadable")
            else -> Unit
        }
    }

    private fun undialable(endpoint: Endpoint) = AgentFailure.BadRequest(
        "This app does not dial ${endpoint.label} as it is written.",
        "${endpoint.label} on ${machine.name} is not a host, user and port this app dials, scans or " +
            "pins a host key for, so that address was skipped. Correct it in the setup.",
    )

    /**
     * The failures collected across every address and system, and which one to report.
     *
     * The order is not arbitrary. On a machine with more than one system, every attempt but one is
     * aimed at a system that is not running and is guaranteed to fail, so the loudest failure is
     * usually the least informative. An interpreter that was not found means the command shape was
     * wrong, which says nothing. An interpreter that ran and could not find the script means the
     * shape was right and the agent really is missing, which is the thing worth telling the user.
     *
     * A host key that does not match its pin comes first of all, and a refused key next, because
     * they are questions rather than reports and the only ones with an action attached. A request
     * that could not be made is a fault in the document, which is the next most useful thing to say.
     *
     * An address skipped because it is outside the dial grammar comes last of all, after a machine
     * that did not answer. The document holding it is valid and Apply has already named the entry,
     * so it is worth saying only when nothing else happened: reported first, beside a good address
     * that was asleep, it turned every sleep or reboot into a setup that cannot work, polled at the
     * blocked rate. The route selector leaves such an address out whenever there is a good one, and
     * this keeps the same order should one ever be handed over anyway.
     */
    private class Failures {
        var hostKeyChanged: AgentFailure? = null
        var notAuthorised: AgentFailure? = null
        var badRequest: AgentFailure? = null
        var scriptMissing: AgentFailure? = null
        var wrongShape: AgentFailure? = null
        var unreadable: AgentFailure? = null
        var unreachable: AgentFailure? = null
        var ambiguous: AgentFailure? = null
        var undialable: AgentFailure? = null

        /** An address this app does not dial, kept apart from a command line it would not write. */
        fun skipped(failure: AgentFailure) {
            undialable = undialable ?: failure
        }

        fun record(failure: AgentFailure) {
            when (failure) {
                is AgentFailure.HostKeyChanged -> hostKeyChanged = hostKeyChanged ?: failure
                is AgentFailure.NotAuthorised -> notAuthorised = notAuthorised ?: failure
                is AgentFailure.BadRequest -> badRequest = badRequest ?: failure
                is AgentFailure.AgentMissing ->
                    if (failure.kind == FailureKind.AGENT_MISSING) {
                        scriptMissing = scriptMissing ?: failure
                    } else {
                        wrongShape = wrongShape ?: failure
                    }

                is AgentFailure.BadOutput -> unreadable = unreadable ?: failure
                else ->
                    if (failure.dispatch.mayHaveRun) {
                        ambiguous = ambiguous ?: failure
                    } else {
                        unreachable = unreachable ?: failure
                    }
            }
        }

        fun worthReporting(): AgentFailure? =
            hostKeyChanged ?: notAuthorised ?: badRequest ?: scriptMissing ?: unreadable ?: ambiguous ?:
                wrongShape ?: unreachable ?: undialable
    }

    /**
     * The systems to try on one address, in the order of section 15.5.
     *
     * The system the address belongs to, when the configuration says, then the one that answered
     * last, then the rest in document order. Only systems that can be driven, and only the command
     * they name: no launcher, interpreter or grammar is derived here, because a guessed second shape
     * is one more way for one press to run twice.
     */
    private fun attemptOrder(endpoint: Endpoint): List<MachineSystem> {
        val driven = machine.systems.filter { it.isDriven }
        val hinted = machine.system(endpoint.systemHint)
        val preferred = machine.system(settings.lastKnownSystem(machine.id).value)
        return (listOfNotNull(hinted, preferred).filter { it in driven } + driven).distinct()
    }

    private fun <T> reportedSystem(value: T): String? = when (value) {
        is AgentStatus -> value.systemId
        else -> null
    }

    private fun condense(text: String): String {
        val trimmed = text.trim()
        return if (trimmed.length <= 400) trimmed else trimmed.take(400) + "..."
    }

    /** A value this app was about to send that it must not, as a request that was never made. */
    private fun refuse(sentence: String): Nothing =
        throw AgentFailure.BadRequest("This app will not send that request.", sentence)

    private companion object {
        const val SHORT_TIMEOUT_MILLIS = 40_000L

        /** The client's own budget for a snapshot. The agent bounds itself well inside this. */
        const val STATUS_TIMEOUT_MILLIS = 30_000L
        const val DOCTOR_TIMEOUT_MILLIS = 60_000L
        const val DEEP_DOCTOR_TIMEOUT_MILLIS = 120_000L
        const val BOOT_TIMEOUT_MILLIS = 60_000L
        const val SLEEP_TIMEOUT_MILLIS = 60_000L
        const val RESTART_TIMEOUT_MILLIS = 240_000L
        const val WAKE_PROXY_TIMEOUT_MILLIS = 90_000L
        const val SELF_UPDATE_TIMEOUT_MILLIS = 300_000L

        /** A configured action is somebody else's script, and the agent gives it its own timeout. */
        const val ACTION_TIMEOUT_MILLIS = 180_000L

        /**
         * The synchronous update budget, used only against a 2.x agent that cannot detach.
         *
         * Deliberately generous. The agent's own worst case is
         * cache warming plus an npm install plus a health check, and seven minutes could expire in
         * the middle of a perfectly healthy install, leaving an outcome nobody could resolve. Against
         * a 3.x agent this is never reached: the update detaches and the phone polls `op`.
         */
        const val UPDATE_TIMEOUT_MILLIS = 1_500_000L

        /** How much longer than its own `--wait` a long poll is given before the client gives up. */
        const val OP_WAIT_MARGIN_MILLIS = 20_000L

        /** Short. A diagnosis that has to wait a minute has already told you what you needed. */
        const val DIAGNOSE_TIMEOUT_MILLIS = 20_000L
    }
}

private val CommandOutcome.failureText: String
    get() = listOf(stderr, stdout).firstOrNull { it.isNotBlank() } ?: "exit status $exitStatus"

/**
 * The `op` reply, which may carry the record at the top level or nested under `op`.
 *
 * The contract says the record is "embedded under `op` in every mutating reply" and does not say
 * which of the two the `op` command itself uses. Both are read and the nested one wins, which costs
 * one nullable field and cannot be wrong either way.
 */
@kotlinx.serialization.Serializable
internal data class OpEnvelope(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val message: String? = null,
    val reasonCode: String? = null,
    val error: String? = null,
    val op: OpRecord? = null,
    val id: String? = null,
    val kind: String? = null,
    val service: String? = null,
    val target: String? = null,
    val actionId: String? = null,
    val mode: String? = null,
    val initiator: OpInitiator? = null,
    val state: String? = null,
    val phase: String? = null,
    val progress: OpProgress? = null,
    val requestedAt: String? = null,
    val startedAt: String? = null,
    val updatedAt: String? = null,
    val finishedAt: String? = null,
    val expiresAt: String? = null,
    val pid: Int? = null,
    val detached: Boolean? = null,
    val from: String? = null,
    val to: String? = null,
    val result: OpResult? = null,
    val log: List<OpLogEntry>? = null,
    val children: List<OpChild>? = null,
    val replaced: String? = null,
    val agentVersion: String? = null,
    val systemId: String? = null,
    val replayed: Boolean? = null,
) {
    fun record(): OpRecord {
        requireAcceptedRead(this)
        return op ?: OpRecord(
            id = id,
            kind = kind,
            service = service,
            target = target,
            actionId = actionId,
            mode = mode,
            initiator = initiator,
            state = state,
            phase = phase,
            progress = progress,
            requestedAt = requestedAt,
            startedAt = startedAt,
            updatedAt = updatedAt,
            finishedAt = finishedAt,
            expiresAt = expiresAt,
            pid = pid,
            detached = detached,
            from = from,
            to = to,
            result = result,
            log = log,
            children = children,
            replaced = replaced,
            agentVersion = agentVersion,
            systemId = systemId,
            replayed = replayed,
        )
    }
}

/** Refused reads cannot establish a route, capabilities, system identity or operation state. */
internal fun requireAcceptedRead(value: Any?) {
    when (value) {
        is AgentStatus -> if (value.ok == false) {
            throw AgentFailure.Reported(value.message ?: value.error ?: value.reasonCode ?: "Status request refused.")
        }
        is OpEnvelope -> {
            if (value.ok == false) {
                throw AgentFailure.Reported(value.message ?: value.error ?: value.reasonCode ?: "Operation read refused.")
            }
            if ((value.op?.id ?: value.id).isNullOrBlank()) {
                throw AgentFailure.BadOutput("The operation read contained no operation ID.")
            }
        }
    }
}

/**
 * The one JSON object out of a stream that may have other things in it.
 *
 * PowerShell puts a CLIXML banner in front of remote output and interleaves progress records with it,
 * so the reply cannot be parsed as a whole stream. Taking the slice from the first brace to the last
 * one is what the Mac app does and it is what the contract allows: exactly one object per invocation,
 * and everything else the agent has to say goes to stderr.
 */
internal fun sliceJsonObject(text: String): String? {
    val start = text.indexOf('{')
    val end = text.lastIndexOf('}')
    if (start < 0 || end <= start) return null
    return text.substring(start, end + 1)
}
