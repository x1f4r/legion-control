package com.x1f4r.legioncontrol.agent

/**
 * What the agent on the far side can be asked to do.
 *
 * The contract settles this with one number. Every reply from a 3.x agent carries `"contract": 3`,
 * and a client gates every v3 feature on `contract >= 3`; absence means a 2.x or 1.x agent and the
 * old key-sniffing paths stay. That is deliberately not a capability list per verb, and it is a
 * better arrangement than one: an agent either implements contract 3 or it does not, and a partial
 * one would be a bug rather than a configuration. Nothing here asks `help` which verbs there are:
 * the one optional verb it was ever asked for, a wake proxy, was never part of the contract, and a
 * wake helper is an ordinary configured action (contract section 10.2).
 */
data class AgentAbilities(
    /**
     * The `contract` number from the last reply, or null when no reply has carried one.
     *
     * Null rather than zero, because "nothing said" is not a number to compare against. A zero that
     * stood for "not read yet" was below 3, and that was enough for the app to pick the legacy
     * bootstrap for a machine nobody had heard from. Only an authenticated status says what an agent
     * is; see [AgentDeploymentPath.decide].
     */
    val contract: Int? = null,
    val agentVersion: String? = null,
) {
    /** Whether every v3 command and flag in this app is safe to send. */
    val speaksV3: Boolean get() = (contract ?: 0) >= Contract.REQUIRED

    /**
     * Whether the far side is newer than this build understands.
     *
     * Worth saying rather than hiding: the app keeps working against the part of the contract it
     * knows, and the user is told that the machine has moved on.
     */
    val isNewerThanThisApp: Boolean get() = (contract ?: 0) > Contract.REQUIRED

    /** One line for the machine page. */
    fun summary(): String = when {
        contract == null || contract <= 0 -> "not read yet"
        !speaksV3 -> "contract $contract, older than this app needs"
        isNewerThanThisApp -> "contract $contract, newer than this app knows"
        else -> "contract $contract"
    }

    companion object {
        val unknown = AgentAbilities()

        fun of(status: AgentStatus): AgentAbilities = AgentAbilities(
            contract = status.contract ?: status.agent?.contract,
            agentVersion = status.version,
        )

        fun of(version: AgentVersion): AgentAbilities = AgentAbilities(
            contract = version.contract,
            agentVersion = version.agentVersion,
        )
    }
}
