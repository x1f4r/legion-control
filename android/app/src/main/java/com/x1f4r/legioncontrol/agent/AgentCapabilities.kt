package com.x1f4r.legioncontrol.agent

import kotlinx.serialization.Serializable

/**
 * What the agent on the far side can be asked to do.
 *
 * The contract settles this with one number. Every reply from a 3.x agent carries `"contract": 3`,
 * and a client gates every v3 feature on `contract >= 3`; absence means a 2.x or 1.x agent and the
 * old key-sniffing paths stay. That is deliberately not a capability list per verb, and it is a
 * better arrangement than one: an agent either implements contract 3 or it does not, and a partial
 * one would be a bug rather than a configuration.
 *
 * The one thing this does hold beyond the number is the verb list from `help`, and it exists for
 * exactly one command: the wake proxy, which is a proposed contract addition rather than part of
 * contract 3 proper. Nothing else consults it.
 */
data class AgentAbilities(
    /** The `contract` number from the last reply. Zero until something has answered. */
    val contract: Int = 0,
    val agentVersion: String? = null,
    /** Verb names from `help`, when it has been read. Empty otherwise, which means "do not assume". */
    val verbs: Set<String> = emptySet(),
) {
    /** Whether every v3 command and flag in this app is safe to send. */
    val speaksV3: Boolean get() = contract >= Contract.REQUIRED

    /**
     * Whether the far side is newer than this build understands.
     *
     * Worth saying rather than hiding: the app keeps working against the part of the contract it
     * knows, and the user is told that the machine has moved on.
     */
    val isNewerThanThisApp: Boolean get() = contract > Contract.REQUIRED

    /** The optional wake proxy, only ever used when `help` actually listed it. */
    val hasWakeProxy: Boolean get() = speaksV3 && "wake" in verbs

    /** One line for the machine page. */
    fun summary(): String = when {
        contract <= 0 -> "not read yet"
        !speaksV3 -> "contract $contract, older than this app needs"
        isNewerThanThisApp -> "contract $contract, newer than this app knows"
        else -> "contract $contract"
    }

    companion object {
        val unknown = AgentAbilities()

        fun of(status: AgentStatus): AgentAbilities = AgentAbilities(
            contract = status.contract ?: status.agent?.contract ?: 0,
            agentVersion = status.version,
        )

        fun of(version: AgentVersion): AgentAbilities = AgentAbilities(
            contract = version.contract ?: 0,
            agentVersion = version.agentVersion,
        )
    }
}

/** One `help` reply, read only to find out whether an optional verb exists. */
@Serializable
data class HelpReply(
    val ok: Boolean? = null,
    val contract: Int? = null,
    val agentVersion: String? = null,
    val usage: String? = null,
    val commands: List<HelpCommand>? = null,
) {
    val verbs: Set<String>
        get() = commands.orEmpty().mapNotNull { it.name?.trim()?.takeIf(String::isNotEmpty) }.toSet()
}

@Serializable
data class HelpCommand(
    val name: String? = null,
    val flags: List<String>? = null,
    val summary: String? = null,
)
