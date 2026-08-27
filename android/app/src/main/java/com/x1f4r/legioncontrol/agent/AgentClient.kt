package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.Settings
import com.x1f4r.legioncontrol.net.CommandOutcome
import com.x1f4r.legioncontrol.net.Endpoint
import com.x1f4r.legioncontrol.net.RouteSelector
import com.x1f4r.legioncontrol.net.SshTransport
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonObject

/** An answer, plus how it was obtained, because the UI is expected to show the route it used. */
data class AgentReply<T>(
    val value: T,
    val route: Endpoint,
    /** Straight out of the agent's own reply. Never inferred from the address. */
    val systemId: String?,
)

/**
 * The agent contract, spoken over ssh.
 *
 * This is a port of what the Mac app does and not a new protocol: the same script, the same
 * arguments, the same one JSON object per invocation. Nothing here is allowed to invent a command or
 * a field, because the far side is shared with the Mac and only ever answers what it already answers.
 *
 * One of these per machine. Which interpreter to hand the command to comes from that machine's
 * configuration and from nowhere else, so a machine this app has never been told about cannot be
 * reached by it at all.
 */
class AgentClient(
    private val machine: Machine,
    private val transport: SshTransport,
    private val routes: RouteSelector,
    private val settings: Settings,
) {
    suspend fun status(): AgentReply<AgentStatus> =
        call(listOf("status"), STATUS_TIMEOUT_MILLIS, AgentStatus.serializer())

    suspend fun busy(): AgentReply<BusyStatus> =
        call(listOf("busy"), STATUS_TIMEOUT_MILLIS, BusyStatus.serializer())

    /**
     * One update cycle for one service.
     *
     * [serviceId] is left out for an agent that predates services, and that is not tidiness: the
     * first version refuses any flag it does not know by name, so `--service` sent to it fails the
     * command outright rather than being ignored. The caller passes an id only when the status it
     * is working from actually listed services.
     */
    suspend fun update(serviceId: String? = null, force: Boolean = false): AgentReply<AgentActionResult> =
        call(
            command("update", serviceId, force),
            UPDATE_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
        )

    suspend fun restart(serviceId: String? = null, force: Boolean = false): AgentReply<AgentActionResult> =
        call(
            command("restart", serviceId, force),
            RESTART_TIMEOUT_MILLIS,
            AgentActionResult.serializer(),
        )

    suspend fun setAutoUpdate(enabled: Boolean): AgentReply<AgentActionResult> = call(
        listOf("auto-update", if (enabled) "on" else "off"),
        SHORT_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
    )

    /**
     * Arms the next boot and reboots. The reboot cuts the connection while we are still reading, so
     * the timeout is short and a dropped link after a good reply is the expected ending, not a fault.
     */
    suspend fun boot(targetId: String, force: Boolean = false): AgentReply<AgentActionResult> {
        require(targetId.isNotBlank()) { "boot takes the id of a system" }
        val arguments = buildList {
            add("boot")
            add(targetId)
            if (force) add("--force")
        }
        return call(arguments, BOOT_TIMEOUT_MILLIS, AgentActionResult.serializer())
    }

    /**
     * Suspends the machine, once the agent has checked that nothing is running.
     *
     * The same shape as boot, and the same ending: the machine stops answering while the reply is
     * still being read, so the timeout is short and a link that drops after the command was accepted
     * is the expected outcome rather than a fault.
     */
    suspend fun sleep(force: Boolean = false): AgentReply<AgentActionResult> = call(
        if (force) listOf("sleep", "--force") else listOf("sleep"),
        SLEEP_TIMEOUT_MILLIS,
        AgentActionResult.serializer(),
    )

    /** Runs one of the actions the agent offered. Busy gated by the agent, exactly as the rest are. */
    suspend fun run(actionId: String, force: Boolean = false): AgentReply<AgentActionResult> {
        require(actionId.isNotBlank()) { "run takes the id of an action" }
        val arguments = buildList {
            add("run")
            add(actionId)
            if (force) add("--force")
        }
        return call(arguments, ACTION_TIMEOUT_MILLIS, AgentActionResult.serializer())
    }

    suspend fun version(): AgentReply<AgentVersion> =
        call(listOf("version"), SHORT_TIMEOUT_MILLIS, AgentVersion.serializer())

    /**
     * The setup this machine carries, as the raw reply.
     *
     * Raw rather than decoded, because the three things this reply can say are told apart by which
     * keys are there rather than by their values: an agent too old to know the command answers with
     * an object of its own, and a class with a nullable field cannot tell that apart from a machine
     * that is carrying nothing. [readControllerReply] does the reading.
     */
    suspend fun config(): AgentReply<JsonObject> =
        call(listOf("config"), SHORT_TIMEOUT_MILLIS, JsonObject.serializer())

    private fun command(name: String, serviceId: String?, force: Boolean): List<String> = buildList {
        add(name)
        serviceId?.takeIf { it.isNotBlank() }?.let {
            add("--service")
            add(it)
        }
        if (force) add("--force")
    }

    // MARK: - Transport

    private suspend fun <T : Any> call(
        arguments: List<String>,
        timeoutMillis: Long,
        deserializer: KSerializer<T>,
    ): AgentReply<T> {
        val failures = Failures()
        val tried = mutableSetOf<String>()

        // Round one is the route that worked last time, on its own. That is the whole point of
        // remembering it: the normal call costs one TCP probe and one ssh connection.
        routes.remembered()?.let { endpoint ->
            attempt(endpoint, arguments, timeoutMillis, deserializer, failures)?.let { return it }
            tried += endpoint.id
        }

        // Round two only happens when round one produced nothing, so it can afford to be thorough.
        // Probing every address at once costs one timeout rather than one each, and it is the only
        // way to find out that the machine has rebooted into another system since the last call.
        val reachable = routes.reachable()
        for (endpoint in reachable) {
            if (!tried.add(endpoint.id)) continue
            attempt(endpoint, arguments, timeoutMillis, deserializer, failures)?.let { return it }
        }

        if (tried.isEmpty()) throw AgentFailure.Unreachable(machine.name, routes.probeSummary())
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
    ): AgentReply<T>? {
        for (system in shapeOrder(endpoint)) {
            val outcome = try {
                transport.run(endpoint, remoteCommand(system, arguments), timeoutMillis)
            } catch (failure: AgentFailure.Unreachable) {
                // This address answered a TCP probe a moment ago and has stopped since. The command
                // shape is not the problem, so move on to the next address rather than the next shape.
                failures.unreachable = failures.unreachable ?: failure
                return null
            } catch (failure: AgentFailure.NotAuthorised) {
                throw failure
            } catch (failure: AgentFailure.TimedOut) {
                throw failure
            } catch (failure: AgentFailure.HostKeyChanged) {
                // Recorded rather than thrown, because the pin is per address and the other addresses
                // are not affected by it. A LAN address legitimately changes host key every time the
                // machine boots into another system, and letting that one address stop the call would
                // strand the app on a question the user has to answer, while a remote route that
                // still matches its pin sits there working. It is reported first if nothing answers.
                failures.hostKeyChanged = failures.hostKeyChanged ?: failure
                return null
            }

            val json = sliceJsonObject(outcome.stdout)
            if (json != null) {
                val value = try {
                    AgentJson.decodeFromString(deserializer, json)
                } catch (failure: Exception) {
                    throw AgentFailure.BadOutput(
                        "${failure.message} Output: ${condense(outcome.stdout)}",
                        failure,
                    )
                }
                routes.remember(endpoint)
                val reported = reportedSystem(value)?.also { settings.rememberSystem(machine.id, it) }
                return AgentReply(value = value, route = endpoint, systemId = reported)
            }

            when (classify(outcome)) {
                Failure.ScriptMissing ->
                    failures.scriptMissing = failures.scriptMissing
                        ?: AgentFailure.AgentMissing(system, condense(outcome.failureText))

                Failure.InterpreterMissing ->
                    failures.wrongShape = failures.wrongShape
                        ?: AgentFailure.AgentMissing(system, condense(outcome.failureText))

                Failure.Other ->
                    failures.unreadable = failures.unreadable
                        ?: AgentFailure.BadOutput(condense(outcome.failureText))
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

        fun worthReporting(): AgentFailure? =
            hostKeyChanged ?: scriptMissing ?: unreadable ?: wrongShape ?: unreachable
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
        return (listOfNotNull(first) + usable).distinct()
    }

    private fun remoteCommand(system: MachineSystem, arguments: List<String>): String =
        (system.agent + arguments).joinToString(" ")

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
        const val STATUS_TIMEOUT_MILLIS = 30_000L
        const val BOOT_TIMEOUT_MILLIS = 60_000L
        const val SLEEP_TIMEOUT_MILLIS = 60_000L
        const val RESTART_TIMEOUT_MILLIS = 240_000L

        /** A configured action is somebody else's script, and the agent gives it its own timeout. */
        const val ACTION_TIMEOUT_MILLIS = 180_000L

        /** An update pulls a build down the machine's own connection and then installs it. */
        const val UPDATE_TIMEOUT_MILLIS = 420_000L
    }
}

private val CommandOutcome.failureText: String
    get() = listOf(stderr, stdout).firstOrNull { it.isNotBlank() } ?: "exit status $exitStatus"

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
