package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.Settings
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.SshTransport
import com.x1f4r.legioncontrol.net.StdinSource
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonObject
import java.io.File

/** An answer, plus how it was obtained, because the UI is expected to show the route it used. */
data class AgentReply<T>(
    val value: T,
    val route: Endpoint,
    /** Straight out of the agent's own reply. Never inferred from the address. */
    val systemId: String?,
)

/**
 * Whether a command changes anything, which decides what may be done when it fails.
 *
 * A status that fails on one address is retried on the next, because reading twice is reading once.
 * A restart that fails on one address is only retried when it is known not to have started, and
 * never on the strength of "the connection went away", because that is exactly what a restart looks
 * like from here.
 */
private enum class CommandNature { READ, MUTATION }

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
 * One JSON object per invocation, one command per connection. What has changed since 1.2 is that the
 * command line is built rather than pasted together: an argv is not a string, and joining one with
 * spaces works right up to the first path with a space in it. See [buildRemoteCommand]. Every value
 * this app puts on a command line is also checked against the contract's token grammar first, so an
 * id out of a document can never become a second command whatever the quoting does.
 *
 * Everything added in contract 3 is gated on the far side saying `"contract": 3`. A 2.x agent
 * refuses a flag it does not recognise rather than ignoring it, so one unknown flag fails the whole
 * command, and every v3 flag below is therefore behind [abilities].
 */
class AgentClient(
    private val machine: Machine,
    private val transport: SshTransport,
    private val routes: RouteSelector,
    private val settings: Settings,
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

    // MARK: - Reads

    suspend fun status(budgetMillis: Long? = null): AgentReply<AgentStatus> = call(
        buildList {
            add("status")
            if (budgetMillis != null && abilities.speaksV3) {
                add("--budget-ms")
                add(budgetMillis.toString())
            }
        },
        STATUS_TIMEOUT_MILLIS,
        AgentStatus.serializer(),
        CommandNature.READ,
    ).also { abilities = AgentAbilities.of(it.value).copy(verbs = abilities.verbs) }

    suspend fun busy(): AgentReply<BusyStatus> =
        call(listOf("busy"), STATUS_TIMEOUT_MILLIS, BusyStatus.serializer(), CommandNature.READ)

    suspend fun version(): AgentReply<AgentVersion> =
        call(listOf("version"), SHORT_TIMEOUT_MILLIS, AgentVersion.serializer(), CommandNature.READ)
            .also { abilities = AgentAbilities.of(it.value).copy(verbs = abilities.verbs) }

    /**
     * The verb list, read once per session and only because of one optional command.
     *
     * The wake proxy is an optional verb rather than part of contract 3 proper, so this app finds
     * out whether it exists instead of assuming either way.
     */
    suspend fun readVerbs(): Set<String> {
        if (abilities.verbs.isNotEmpty()) return abilities.verbs
        val reply = call(listOf("help"), SHORT_TIMEOUT_MILLIS, HelpReply.serializer(), CommandNature.READ)
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
        CommandNature.READ,
    )

    suspend fun serviceConfigGet(): AgentReply<JsonObject> =
        call(listOf("service-config", "get"), SHORT_TIMEOUT_MILLIS, JsonObject.serializer(), CommandNature.READ)

    suspend fun serviceConfigWrite(action: String, payload: JsonObject): AgentReply<JsonObject> {
        require(action == "validate" || action == "set")
        return call(listOf("service-config", action, "--stdin"), DOCTOR_TIMEOUT_MILLIS,
            JsonObject.serializer(), if (action == "set") CommandNature.MUTATION else CommandNature.READ,
            stdin = StdinSource.Bytes(payload.toString().toByteArray(Charsets.UTF_8)))
    }

    /** Doctor, status, history, logs and a redacted config, for the export action. */
    suspend fun bundle(): AgentReply<JsonObject> =
        call(listOf("bundle"), DEEP_DOCTOR_TIMEOUT_MILLIS, JsonObject.serializer(), CommandNature.READ)

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
        val reply = call(arguments, budget, OpEnvelope.serializer(), CommandNature.READ)
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
        CommandNature.READ,
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
        CommandNature.READ,
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
        call(listOf("config"), SHORT_TIMEOUT_MILLIS, JsonObject.serializer(), CommandNature.READ)

    /** Identity, revision, ancestry and hash without the document, for deciding descent. */
    suspend fun configMeta(): AgentReply<ConfigMetaReply> =
        call(listOf("config", "meta"), SHORT_TIMEOUT_MILLIS, ConfigMetaReply.serializer(), CommandNature.READ)

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
        require(revision >= 0) { "a revision counts up from zero" }
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
            CommandNature.MUTATION,
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
        CommandNature.READ,
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
        CommandNature.MUTATION,
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
        CommandNature.MUTATION,
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
        CommandNature.MUTATION,
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
        CommandNature.MUTATION,
    )

    /** `auto-update pause DUR` and `auto-update resume`. */
    suspend fun pauseUpdates(
        duration: String,
        serviceId: String? = null,
        operationId: String? = null,
    ): AgentReply<AgentActionResult> {
        require(Contract.isDuration(duration)) { "a pause is written like 30m, 4h or 2d" }
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
            CommandNature.MUTATION,
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
        CommandNature.MUTATION,
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
            CommandNature.MUTATION,
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
        CommandNature.MUTATION,
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
            CommandNature.MUTATION,
        )
    }

    /** Withdraws a queued operation that has not started. A running one is not killable. */
    suspend fun cancel(operationId: String): AgentReply<AgentActionResult> {
        Contract.requireToken(operationId, "operation id")
        return call(
            listOf("cancel", operationId),
            SHORT_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
            CommandNature.MUTATION,
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
            CommandNature.MUTATION,
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
        CommandNature.MUTATION,
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
        CommandNature.MUTATION,
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
        val system = machine.systems.firstOrNull { it.isConfigured }
            ?: throw AgentFailure.AgentMissing(null, "no system on this machine names an interpreter")

        // tar is on Windows 10 and later, on macOS and on every Linux, so this is one command
        // everywhere rather than three.
        val unpack = buildRemoteCommand(
            system.shell,
            listOf("tar", "-xzf", tarballPath, "-C", stagingDirectory),
        )
        val endpoint = routes.candidates().firstOrNull()
            ?: throw AgentFailure.Unreachable(machine.name, routes.probeSummary())
        val unpacked = transport.run(endpoint, unpack, SELF_UPDATE_TIMEOUT_MILLIS)
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
            CommandNature.MUTATION,
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
            CommandNature.MUTATION,
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
     * authenticated readiness check: it answers what a bare TCP probe answered and four more
     * questions besides, and because it only happens when a person presses a button it cannot become
     * the background traffic that gets a source penalised.
     */
    suspend fun diagnose(endpoint: Endpoint): EndpointDiagnosis {
        val started = System.currentTimeMillis()
        fun took() = System.currentTimeMillis() - started
        val failures = Failures()
        return try {
            val reply = attempt(
                endpoint,
                listOf("version"),
                DIAGNOSE_TIMEOUT_MILLIS,
                AgentVersion.serializer(),
                failures,
                CommandNature.READ,
                null,
                null,
            )
            when {
                reply != null -> EndpointDiagnosis(
                    endpoint = endpoint,
                    status = DiagnosisStatus.ANSWERED,
                    detail = reply.value.agentVersion?.let {
                        "control agent $it, contract ${reply.value.contract ?: 2}"
                    },
                    millis = took(),
                )

                failures.hostKeyChanged != null -> EndpointDiagnosis(
                    endpoint,
                    DiagnosisStatus.HOST_KEY_CHANGED,
                    failures.hostKeyChanged?.detail,
                    took(),
                )

                failures.scriptMissing != null || failures.wrongShape != null -> EndpointDiagnosis(
                    endpoint,
                    DiagnosisStatus.NO_AGENT,
                    (failures.scriptMissing ?: failures.wrongShape)?.detail,
                    took(),
                )

                failures.unreadable != null -> EndpointDiagnosis(
                    endpoint,
                    DiagnosisStatus.UNREADABLE,
                    failures.unreadable?.detail,
                    took(),
                )

                else -> EndpointDiagnosis(
                    endpoint,
                    DiagnosisStatus.UNREACHABLE,
                    failures.unreachable?.detail,
                    took(),
                )
            }
        } catch (failure: AgentFailure.NotAuthorised) {
            EndpointDiagnosis(endpoint, DiagnosisStatus.REFUSED_KEY, failure.detail, took())
        } catch (failure: AgentFailure.TimedOut) {
            EndpointDiagnosis(endpoint, DiagnosisStatus.TIMED_OUT, failure.detail, took())
        } catch (failure: AgentFailure.HostKeyChanged) {
            EndpointDiagnosis(endpoint, DiagnosisStatus.HOST_KEY_CHANGED, failure.detail, took())
        } catch (failure: AgentFailure) {
            EndpointDiagnosis(endpoint, DiagnosisStatus.UNREACHABLE, failure.detail, took())
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
        require(Contract.isOpId(id)) { "an operation id is lowercase, 8 to 64 of [a-z0-9-]" }
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
     * One command, across the addresses worth trying.
     *
     * The route that worked last time leads and is dialled directly, with no probe in front of it.
     * That probe used to be the first thing every call did, and it is the traffic OpenSSH counts
     * against a source when it never completes authentication.
     *
     * [nature] decides what may happen after a failure. A read moves on to the next address. A
     * mutation moves on only while the failure says nothing reached the far side; the moment one
     * says it may have, the search stops and the caller is told it is ambiguous, because trying the
     * next address is how one press becomes two restarts.
     */
    private suspend fun <T : Any> call(
        arguments: List<String>,
        timeoutMillis: Long,
        deserializer: KSerializer<T>,
        nature: CommandNature,
        stdin: StdinSource? = null,
        /**
         * The argv to run instead of the system's configured one.
         *
         * Used by exactly one caller: the bootstrap, which has to run the tree it has just uploaded
         * rather than the one that is already installed. Everything else uses what the setup names,
         * because a machine this app has not been told about must not be reachable by it at all.
         */
        overrideArgv: List<String>? = null,
    ): AgentReply<T> {
        val failures = Failures()
        val candidates = routes.candidates()

        for (endpoint in candidates) {
            val outcome =
                attempt(endpoint, arguments, timeoutMillis, deserializer, failures, nature, stdin, overrideArgv)
            if (outcome != null) return outcome
            if (failures.stopSearching) break
        }

        if (candidates.isEmpty()) throw AgentFailure.Unreachable(machine.name, routes.probeSummary())
        throw failures.worthReporting() ?: AgentFailure.Unreachable(machine.name, routes.probeSummary())
    }

    /**
     * One address, every command shape. Returns the reply, or null once each shape has failed and
     * the reason has been recorded.
     *
     * An unauthorised key is thrown rather than recorded: it does not get better by trying another
     * address, and it would otherwise fill the machine's auth log with one refused login per address
     * per refresh. A command that ran out of time is thrown for the opposite reason, that repeating
     * it against the other addresses would spend the same budget again on each of them.
     */
    private suspend fun <T : Any> attempt(
        endpoint: Endpoint,
        arguments: List<String>,
        timeoutMillis: Long,
        deserializer: KSerializer<T>,
        failures: Failures,
        nature: CommandNature,
        stdin: StdinSource?,
        overrideArgv: List<String>? = null,
    ): AgentReply<T>? {
        for (system in shapeOrder(endpoint)) {
            val line = try {
                buildRemoteCommand(system.shell, (overrideArgv ?: system.agent) + arguments)
            } catch (failure: UnquotableArgument) {
                // Nothing was sent, and nothing on another address would be either: it is the same
                // configuration everywhere. Thrown, because there is nothing here to retry and the
                // fix is a field in the document.
                throw AgentFailure.Unquotable(failure.argument, failure.reason)
            }

            val outcome = try {
                transport.run(endpoint, line, timeoutMillis, stdin)
            } catch (failure: AgentFailure.NotAuthorised) {
                routes.note(endpoint, "refused this phone's key")
                throw failure
            } catch (failure: AgentFailure.TimedOut) {
                routes.note(endpoint, "did not finish in time")
                throw failure
            } catch (failure: AgentFailure.HostKeyChanged) {
                // Recorded rather than thrown, because the pin is per address and the other addresses
                // are not affected by it. A LAN address legitimately changes host key every time the
                // machine boots into another system, and letting that one address stop the call would
                // strand the app on a question the user has to answer, while a remote route that
                // still matches its pin sits there working. It is reported first if nothing answers.
                routes.note(endpoint, "offered an untrusted host key")
                failures.hostKeyChanged = failures.hostKeyChanged ?: failure
                return null
            } catch (failure: AgentFailure.Unreachable) {
                if (failure.dispatch == DispatchStage.AMBIGUOUS) {
                    routes.note(endpoint, "dropped the link with the command running")
                    // The far side has the command. Whether it ran is not knowable from here, and
                    // the one thing that must not happen next is sending it somewhere else.
                    if (nature == CommandNature.MUTATION) throw failure
                    failures.ambiguous = failures.ambiguous ?: failure
                    return null
                }
                routes.penalise(endpoint, "did not answer")
                failures.unreachable = failures.unreachable ?: failure
                return null
            }

            val json = sliceJsonObject(outcome.stdout)
            if (json != null) {
                val value = try {
                    AgentJson.decodeFromString(deserializer, json)
                } catch (failure: Exception) {
                    // Something ran and printed something. For a mutation that is ambiguous by
                    // definition, and BadOutput says so.
                    throw AgentFailure.BadOutput(
                        "${failure.message} Output: ${condense(outcome.stdout)}",
                        failure,
                    )
                }
                if (nature == CommandNature.READ) requireAcceptedRead(value)
                routes.remember(endpoint)
                val reported = reportedSystem(value)?.also { settings.rememberSystem(machine.id, it) }
                return AgentReply(value = value, route = endpoint, systemId = reported)
            }

            when (classify(outcome)) {
                Failure.ScriptMissing -> {
                    routes.note(endpoint, "answered without the control agent installed")
                    failures.scriptMissing = failures.scriptMissing
                        ?: AgentFailure.AgentMissing(system, condense(outcome.failureText))
                }

                Failure.InterpreterMissing ->
                    failures.wrongShape = failures.wrongShape
                        ?: AgentFailure.AgentMissing(system, condense(outcome.failureText))

                Failure.Other -> {
                    routes.note(endpoint, "answered with something unreadable")
                    failures.unreadable = failures.unreadable
                        ?: AgentFailure.BadOutput(condense(outcome.failureText))
                    // A shell that ran the command and printed something that is not JSON has run
                    // it. Trying the next shape would run it again.
                    if (nature == CommandNature.MUTATION) {
                        failures.stopSearching = true
                        return null
                    }
                }
            }
        }
        return null
    }

    /**
     * The failures collected across every address and command shape, and which one to report.
     *
     * The order is not arbitrary. On a machine with more than one system, every attempt but one is
     * aimed at a system that is not running and is guaranteed to fail, so the loudest failure is
     * usually the least informative. An interpreter that was not found means the command shape was
     * wrong, which says nothing. An interpreter that ran and could not find the script means the
     * shape was right and the agent really is missing, which is the thing worth telling the user.
     *
     * A host key that does not match its pin comes first of all, because it is the only one of these
     * that is a question rather than a report, and it is the only one with an action attached.
     */
    private class Failures {
        var hostKeyChanged: AgentFailure? = null
        var scriptMissing: AgentFailure? = null
        var wrongShape: AgentFailure? = null
        var unreadable: AgentFailure? = null
        var unreachable: AgentFailure? = null
        var ambiguous: AgentFailure? = null

        /** Set when carrying on would risk running a change a second time. */
        var stopSearching: Boolean = false

        fun worthReporting(): AgentFailure? =
            hostKeyChanged ?: scriptMissing ?: unreadable ?: ambiguous ?: wrongShape ?: unreachable
    }

    /**
     * Which interpreter to try first.
     *
     * An address that belongs to one system says so in the configuration, and that answer is free.
     * A shared address cannot say, so the last system that answered is a better guess than nothing.
     * Everything else follows, and a wrong guess only ever costs one extra round trip.
     */
    private fun shapeOrder(endpoint: Endpoint): List<MachineSystem> {
        val usable = machine.systems.filter { it.isConfigured }
        val hinted = machine.system(endpoint.systemHint)
            ?: machine.system(settings.lastKnownSystem(machine.id).value)
        val first = hinted?.takeIf { it in usable }
        return (listOfNotNull(first) + usable).distinct().flatMap { system ->
            listOf(system) + listOfNotNull(stableLauncherArgv(system.agent)?.let { stable ->
                system.copy(agent = stable)
            })
        }.distinct()
    }

    private fun <T> reportedSystem(value: T): String? = when (value) {
        is AgentStatus -> value.systemId
        else -> null
    }

    private enum class Failure { InterpreterMissing, ScriptMissing, Other }

    private fun classify(outcome: CommandOutcome): Failure {
        val text = outcome.failureText.lowercase()
        // The interpreter itself starting and then failing to find the script. That is the right
        // shape on the wrong install, and it is the message worth showing.
        val script = listOf("cannot find module", "module_not_found", "err_module_not_found")
        if (script.any { text.contains(it) }) return Failure.ScriptMissing

        val interpreter = listOf(
            "command not found", "unknown command", "not recognized as the name",
            "is not recognized as an internal", "commandnotfoundexception",
            "no such file or directory",
        )
        if (interpreter.any { text.contains(it) }) return Failure.InterpreterMissing

        return Failure.Other
    }

    private fun condense(text: String): String {
        val trimmed = text.trim()
        return if (trimmed.length <= 400) trimmed else trimmed.take(400) + "..."
    }

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

/**
 * The stable launcher survives replacement of the active `agent/` directory. Keep the configured
 * Node executable and replace only the conventional entry path, so shell and PATH assumptions do
 * not change. A staged 2.x bootstrap supplies an explicit argv and does not use this fallback.
 */
internal fun stableLauncherArgv(argv: List<String>): List<String>? {
    val index = argv.indexOfFirst { value ->
        value.endsWith("/agent/src/index.mjs") || value.endsWith("\\agent\\src\\index.mjs")
    }
    if (index < 0) return null
    val original = argv[index]
    val suffix = if (original.endsWith("/agent/src/index.mjs")) {
        "/agent/src/index.mjs"
    } else {
        "\\agent\\src\\index.mjs"
    }
    val separator = if (suffix.startsWith('/')) "/" else "\\"
    return argv.toMutableList().also {
        it[index] = original.removeSuffix(suffix) + separator + "bin" + separator + "launcher.mjs"
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
