package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.DEFAULT_GITHUB_REPO
import com.x1f4r.legioncontrol.data.LaunchReviewEntry
import com.x1f4r.legioncontrol.data.LaunchScope
import com.x1f4r.legioncontrol.data.LaunchValues
import kotlinx.serialization.json.JsonObject

/**
 * The one place this app decides what it runs (contract section 9.9).
 *
 * Adopting and running are separate. Every document is adopted exactly as section 9.5 says, so the
 * hash and the lineage advance and this device keeps publishing what it holds. What a machine is
 * then sent is decided here and nowhere else: every transport, the legacy bootstrap and the update
 * check read the configuration this builds, never the document's own commands.
 *
 * It runs on every load, every change to the approval record and every adoption, which on this
 * device are all [com.x1f4r.legioncontrol.data.ConfigStore] writing the document and the record in
 * one go.
 */
object LaunchGate {

    /** What the gate decided for one document and one record. */
    data class Effective(
        /**
         * The configuration to drive machines from, or null when there is no setup: the document's
         * own, with each system's command and grammar, and the update source, as this device
         * approved them.
         */
        val config: ControllerConfig?,
        /** Each launch value of the document that waits for this device's approval. */
        val review: List<LaunchReviewEntry>,
    ) {
        companion object {
            val NONE = Effective(null, emptyList())
        }
    }

    /**
     * [config], with every launch value replaced by what this device runs.
     *
     * [document] is the parsed document [config] was read from: the projection never reads the
     * typed model, which a lenient decoder fills in. A system is matched to its tuple by its machine
     * and system ids, and one the projection does not have, such as an id that is not a string in
     * the document, is not driven. A system that runs gets the approved argv and grammar; any other
     * gets no argv at all, so no command line can be built for it.
     */
    fun effective(config: ControllerConfig?, document: JsonObject?, record: JsonObject): Effective {
        val projection = LaunchValues.projection(document)
        val review = if (document == null) emptyList() else LaunchValues.review(record, projection)
        if (config == null) return Effective(null, review)

        val decided = LaunchValues.effective(record, projection)
        val runs = decided.filter { it.scope == LaunchScope.SYSTEM }.associateBy { it.machineId to it.systemId }
        val waiting = review.filter { it.scope == LaunchScope.SYSTEM }.map { it.machineId to it.systemId }.toSet()
        val machines = config.machines.map { machine ->
            machine.copy(
                systems = machine.systems.map { system ->
                    val place = machine.id to system.id
                    val tuple = runs[place]?.tuple
                    val agent = tuple?.agent
                    val grammar = RemoteShell.fromGrammar(tuple?.grammar)
                    if (agent == null || grammar == null) {
                        system.copy(agent = emptyList(), grammar = null, launchState = LaunchState.NOT_DRIVEN)
                    } else {
                        system.copy(
                            agent = agent,
                            grammar = grammar,
                            launchState = if (place in waiting) {
                                LaunchState.APPROVED_WITH_CHANGE_WAITING
                            } else {
                                LaunchState.APPROVED
                            },
                        )
                    }
                },
            )
        }
        // Android has no identityFile and no local agent to run: it signs in with its own key, and
        // only the Mac runs `local`. The update source is the one other launch value it acts on.
        val repo = decided.firstOrNull { it.scope == LaunchScope.REPO }?.text ?: DEFAULT_GITHUB_REPO
        return Effective(
            config = config.copy(machines = machines, appUpdates = config.appUpdates.copy(githubRepo = repo)),
            review = review,
        )
    }
}
