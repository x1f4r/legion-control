package com.x1f4r.legioncontrol.ui

import android.os.SystemClock
import com.x1f4r.legioncontrol.agent.AgentStatus
import com.x1f4r.legioncontrol.agent.ConfigSetReply
import com.x1f4r.legioncontrol.agent.ControllerFetch
import com.x1f4r.legioncontrol.agent.ControllerMark
import com.x1f4r.legioncontrol.agent.Outcome
import com.x1f4r.legioncontrol.agent.ReasonCode
import com.x1f4r.legioncontrol.agent.SetupAdmission
import com.x1f4r.legioncontrol.agent.SetupDecision
import com.x1f4r.legioncontrol.agent.SetupExpectation
import com.x1f4r.legioncontrol.agent.SetupFetched
import com.x1f4r.legioncontrol.agent.SetupGate
import com.x1f4r.legioncontrol.agent.SetupHead
import com.x1f4r.legioncontrol.agent.SetupMeta
import com.x1f4r.legioncontrol.agent.SetupPeerState
import com.x1f4r.legioncontrol.agent.SetupPushEffect
import com.x1f4r.legioncontrol.agent.REMEMBERED_ADMISSIONS
import com.x1f4r.legioncontrol.agent.SETUP_BACKOFF_MS
import com.x1f4r.legioncontrol.agent.admit
import com.x1f4r.legioncontrol.agent.afterPush
import com.x1f4r.legioncontrol.agent.decide
import com.x1f4r.legioncontrol.agent.gate
import com.x1f4r.legioncontrol.agent.settledKey
import com.x1f4r.legioncontrol.data.CanonicalSetup
import com.x1f4r.legioncontrol.data.DocumentAcceptance
import com.x1f4r.legioncontrol.data.Intake
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.SetupJson
import com.x1f4r.legioncontrol.data.SetupOutcome

/**
 * Setup sync for one machine at a time, on every successful status of it (contract section 9.5).
 *
 * Decide from the status, gate what that decision would send, send it, admit what came back and
 * commit it, in that order and nothing else. Two things happen without asking and both are along
 * strict descent: a push where the machine's copy is this device's ancestor, a fetch where this
 * device's copy is the machine's. Everything else either costs one `config meta` round trip or stops
 * at a person, and nothing is ever remembered as "handled": the decision is made again from the
 * hashes on every status, and only the guardrails and three admission refusals carry over.
 *
 * The state of each machine lives here rather than in its model, because applying a document or an
 * approval rebuilds every machine model, and a back-off or a hold must not end with the model that
 * earned it.
 *
 * [clock] only ever has its readings compared with each other, so it is one that counts up whatever
 * the wall clock does: a phone whose time is set back an hour must not hold a sixty-second back-off
 * or a lock for that hour.
 */
internal class SetupPeerSync(
    private val services: ControlServices,
    private val clock: () -> Long = SystemClock::elapsedRealtime,
) {
    private val states = HashMap<String, SetupPeerState>()

    /** Why each machine's last push was held, by machine: the hash it was held for and the sentence. */
    private val holds = HashMap<String, Pair<String, String>>()

    /**
     * The questions a person left, as `head remote`, until either hash changes. Kept by the two
     * documents rather than by machine: every machine carrying the same copy is the same question.
     */
    private val left = HashSet<String>()

    /**
     * The question a Replace found out of date, left where it was pressed and saying why nothing was
     * sent, until its machine's next status closes it and asks again from what both hold then.
     */
    private var outdated: SetupIdentityClash? = null

    fun state(machineId: String): SetupPeerState = states.getOrPut(machineId) { SetupPeerState() }

    /**
     * Why pushes to a machine are held for the document in force, in words, or null when they are
     * not. Only a refusal no retry can cure holds a push; it resumes when the document changes or
     * [retry] is pressed.
     */
    fun hold(machineId: String): String? {
        val hash = services.document.value?.hash ?: return null
        val (heldFor, sentence) = holds[machineId] ?: return null
        return sentence.takeIf { heldFor == hash && states[machineId]?.held?.containsKey(hash) == true }
    }

    /** "Try again" on a held push: the next status of the machine sends it once more. */
    fun retry(machineId: String) {
        val hash = services.document.value?.hash ?: return
        states[machineId]?.retry(hash)
        holds.remove(machineId)
    }

    /**
     * "Leave it" on a divergence or a different setup: not asked again until either side changes,
     * on whichever machine carries [remoteHash]. Only questions about the document in force can
     * come up again, so the ones left about earlier documents are forgotten here.
     */
    fun leave(remoteHash: String?) {
        val head = services.document.value?.hash ?: return
        left.removeAll { !it.startsWith("$head ") }
        left += "$head $remoteHash"
    }

    /**
     * Reconciles one machine after a status. [localMutation] is whether this device has an action in
     * flight on it. Returns a line worth showing, or null.
     */
    suspend fun reconcile(client: MachineClient, status: AgentStatus, localMutation: Boolean, state: SetupPeerState): String? {
        state.statusAt = clock()
        state.runningOperations = status.operations?.running?.size ?: 0
        state.localMutation = localMutation
        // The status a question Replace found out of date was waiting for: it closes, and what
        // follows decides again from what both hold now.
        outdated?.takeIf { it.machineId == client.machine.id }?.let { stale ->
            outdated = null
            if (services.identityClash.value == stale) services.clearIdentityClash()
        }
        var mark = status.controller
        // Deciding again at once happens after a meta read, a filed refusal and a moved head, and
        // never more than this many times on one status: each of those learns something, and a
        // machine that keeps changing under the reads is decided on at its next status.
        repeat(MAX_ROUNDS) {
            val document = services.document.value
            state.localInvalid = services.acceptance.value?.errors?.isNotEmpty() == true
            val head = document?.let { SetupHead(it.hash, it.setupId, it.lineage) }
            val decided = decide(head, status.contract, mark, state.metaByHash)
            val remote = mark
            if (head == null || remote == null) return null
            when (decided.decision) {
                SetupDecision.UNSUPPORTED, SetupDecision.UNKNOWN, SetupDecision.IN_SYNC -> return null
                SetupDecision.DIFFERENT_SETUP, SetupDecision.DIVERGED -> return ask(client, decided.decision, head, remote, status, state)
                else -> Unit
            }
            val gated = gate(decided.decision, head.hash, remote.hash, status.version, clock(), state)
            if (gated.action != SetupGate.Action.ACT) return null
            val step = when (decided.decision) {
                SetupDecision.READ_META -> readMeta(client, head, remote, state)
                SetupDecision.FETCH -> fetch(client, head, decided.expect!!, status, state)
                else -> push(client, head, decided.reason, state)
            }
            when (step) {
                is Step.Done -> return step.line
                Step.Again -> Unit
                is Step.AgainWith -> mark = step.mark
            }
        }
        return null
    }

    /** What one request led to: a line and nothing more this status, or a decision made again. */
    private sealed interface Step {
        data class Done(val line: String?) : Step
        data object Again : Step
        data class AgainWith(val mark: ControllerMark) : Step
    }

    /**
     * `config meta`, once, for a hash this device has nothing filed about. An answer about another
     * hash means the machine changed in between: it is filed, and the next status decides.
     */
    private suspend fun readMeta(client: MachineClient, head: SetupHead, mark: ControllerMark, state: SetupPeerState): Step {
        state.inFlight = true
        val result = try {
            client.configMeta()
        } finally {
            state.inFlight = false
        }
        val reply = result.getOrNull()?.takeIf { it.ok != false }
        if (reply == null) {
            state.failed(head.hash, clock())
            return Step.Done(null)
        }
        val meta = reply.meta
        val hash = meta?.hash ?: reply.hash ?: return Step.Done(null)
        state.file(hash, SetupMeta(meta?.id?.takeIf { it.isNotBlank() }, meta?.lineage.orEmpty()))
        return if (hash == mark.hash) Step.Again else Step.Done(null)
    }

    /**
     * The machine's copy descends from this device's, so it is read and, if it is still what the
     * decision was made about, adopted over the head the decision was made from and nothing else.
     */
    private suspend fun fetch(client: MachineClient, head: SetupHead, expect: String, status: AgentStatus, state: SetupPeerState): Step {
        val name = client.machine.name
        val fetched = when (val read = read(client, head, state)) {
            is Read.Bytes -> read
            Read.Failed -> return Step.Done(null)
            Read.Empty -> return Step.Done(null)
        }
        val admission = admit(
            SetupExpectation(head.hash, head.id!!, expect),
            services.appliedHash(),
            SetupFetched(fetched.hash, fetched.replyHash, fetched.id, fetched.lineage),
            fetched.verdict.valid,
        )
        if (admission in REMEMBERED_ADMISSIONS) state.settled += settledKey(head.hash, expect, status.version)
        return when (admission) {
            SetupAdmission.HEAD_MOVED -> Step.Again
            SetupAdmission.MOVED -> Step.Done(null)
            SetupAdmission.UNVERIFIABLE -> Step.Done(unverifiable(name, "taken automatically"))

            SetupAdmission.INVALID -> Step.Done(
                "$name is carrying a setup this app cannot use. ${fetched.verdict.errors.firstOrNull()?.message.orEmpty()}".trim(),
            )

            SetupAdmission.INCONSISTENT -> Step.Done(
                "$name's copy of the setup does not say it was made from this device's copy, although its " +
                    "status did. Nothing was applied.",
            )

            SetupAdmission.ADOPT -> when (val outcome = services.adoptFromMachine(fetched.text, expect, head.hash, name)) {
                is SetupOutcome.Applied -> Step.Done("Setup updated from $name.")
                SetupOutcome.Unchanged -> Step.Done(null)
                SetupOutcome.HeadMoved -> Step.Again
                is SetupOutcome.Invalid -> {
                    state.settled += settledKey(head.hash, expect, status.version)
                    Step.Done("$name is carrying a setup this app cannot use. ${outcome.reason}")
                }

                // Nothing about the document: this device could not write it down. Tried again
                // after the back-off, like any request that did not go through.
                is SetupOutcome.NotSaved -> {
                    state.failed(head.hash, clock())
                    Step.Done(outcome.reason)
                }
            }
        }
    }

    /**
     * Publishes this device's copy, which the gate let through: nothing is in flight, nothing failed
     * for this head in the last minute, nothing is running on the machine, and the machine has not
     * refused this head for a reason no retry can cure.
     */
    private suspend fun push(client: MachineClient, head: SetupHead, reason: String?, state: SetupPeerState): Step {
        val document = services.document.value?.takeIf { it.hash == head.hash } ?: return Step.Again
        val identity = document.identity ?: return Step.Done(null)
        val name = client.machine.name
        state.inFlight = true
        val result = try {
            client.configSet(
                canonicalDocument = document.canonicalText,
                setupId = identity.id,
                revision = identity.revision,
                replace = false,
            )
        } finally {
            state.inFlight = false
        }
        val reply = result.getOrNull()
        val effect = afterPush(reply, head.hash)
        state.record(effect, reply, head.hash, clock())
        return when (effect.effect) {
            SetupPushEffect.Effect.CLEAR ->
                Step.Done(if (reply?.unchanged == true) null else "The setup was sent to $name.${because(reason)}")

            SetupPushEffect.Effect.FAIL -> Step.Done(
                when {
                    reply != null -> "$name would not take the setup: " + (reply.message ?: reply.reason?.sentence ?: "it refused it")
                    // A push whose reply was lost is byte-identical when it is tried again and the
                    // agent answers `noop`, so the next try after the back-off settles it.
                    result.exceptionOrNull()?.dispatchStage?.mayHaveRun == true ->
                        "The setup may or may not have reached $name. A later check settles it."

                    else -> null
                },
            )

            // What the machine holds, filed under its hash: decided on again at once, as if a
            // status had said it.
            SetupPushEffect.Effect.FILE_META -> Step.AgainWith(ControllerMark(hash = effect.metaHash, id = reply?.current?.id))
            SetupPushEffect.Effect.LOCKED -> Step.Done("$name was busy with a change, so the setup goes once it has finished.")
            SetupPushEffect.Effect.HOLD -> {
                val sentence = held(name, reply!!)
                holds[client.machine.id] = head.hash to sentence
                Step.Done(sentence)
            }
        }
    }

    /** What Replace on the different-setup question came to, and the line to show for it. */
    sealed interface Replaced {
        val line: String
        val isError: Boolean

        /** The machine holds this device's document now, and the question is closed. */
        data class Done(override val line: String) : Replaced {
            override val isError: Boolean get() = false
        }

        /**
         * Nothing was sent, because the question on screen is no longer the one either side would
         * ask. It stays where Replace was pressed, saying so, until the machine's next status
         * closes it and asks again from what both hold then.
         */
        data class Stale(override val line: String) : Replaced {
            override val isError: Boolean get() = false
        }

        /**
         * Not replaced, or not known to be, so the question stays open. Not an error for a push the
         * machine's lock held back, which is a later attempt rather than a refusal.
         */
        data class NotDone(override val line: String, override val isError: Boolean) : Replaced
    }

    /**
     * Sends this device's document over [clash]'s machine with `--replace`, the one command that
     * overwrites a different setup, and only while the question still stands: the document in force
     * is the one the question was built against, and [latestHash], the hash the machine's latest
     * status names, is still the document it showed. `--replace` takes whatever the machine holds by
     * the time it arrives, so a newer copy nobody here has seen is never overwritten on a question
     * about an older one.
     *
     * Decided on the reply's outcome (contract section 5.6), never on its being a reply: a refusal,
     * a push the lock held back and a write the agent failed part way through all come back as
     * replies, and only the first of them replaced nothing for certain.
     */
    suspend fun replace(client: MachineClient, clash: SetupIdentityClash, latestHash: String?): Replaced =
        sendReplace(client, clash, latestHash).also { replaced ->
            // Only if it is still the question on screen: another may have come up while the
            // request was out.
            if (services.identityClash.value != clash) return@also
            when (replaced) {
                is Replaced.Done -> services.clearIdentityClash()
                // Closing it at once took the page away with the sentence saying nothing was sent.
                is Replaced.Stale -> outdated = clash
                is Replaced.NotDone -> Unit
            }
        }

    private suspend fun sendReplace(client: MachineClient, clash: SetupIdentityClash, latestHash: String?): Replaced {
        val name = clash.machineName
        val document = services.document.value?.takeIf { it.hash == clash.mineProvenance?.hash }
            ?: return Replaced.Stale("The setup here changed while this was open; comparing again.")
        val identity = document.identity ?: return Replaced.NotDone("This device's setup has no identity to send yet.", isError = true)
        if (latestHash != null && latestHash != clash.theirProvenance.hash) {
            return Replaced.Stale(
                if (latestHash == document.hash) {
                    "$name carries this setup already."
                } else {
                    "$name holds another document now, not the one shown. Nothing was sent; it is compared again at its next check."
                },
            )
        }
        val state = state(client.machine.id)
        // One setup request to a machine at a time (section 9.5), and a Replace is one: a second
        // press while the first is out sent a second `--replace`, and whichever reply came back
        // last was the one shown, over what the other had done.
        if (state.inFlight) {
            return Replaced.NotDone(
                "A request about the setup is still out to $name, so nothing more was sent. Try again once it has answered.",
                isError = false,
            )
        }
        state.inFlight = true
        val result = try {
            client.configSet(
                canonicalDocument = document.canonicalText,
                setupId = identity.id,
                revision = identity.revision,
                replace = true,
            )
        } finally {
            state.inFlight = false
        }
        val reply = result.getOrNull()
        // Kept as a push's reply is, so the guardrails see it: a refusal no retry can cure holds
        // this hash for that machine, and a request that did not go through backs off. A hold is
        // said as a push's is, with Try again beside it: the automatic push of this hash that the
        // hold now stops would otherwise be stopped without a word.
        val effect = afterPush(reply, document.hash)
        state.record(effect, reply, document.hash, clock())
        if (effect.effect == SetupPushEffect.Effect.HOLD) holds[client.machine.id] = document.hash to held(name, reply!!)

        if (reply == null) {
            val failure = result.exceptionOrNull()
            val sentence = failure?.sentence ?: "$name did not answer."
            return if (failure?.dispatchStage?.mayHaveRun != false) {
                Replaced.NotDone("The setup on $name may or may not have been replaced: $sentence", isError = true)
            } else {
                Replaced.NotDone("The setup on $name was not replaced: $sentence", isError = true)
            }
        }
        val said = reply.message ?: reply.reason?.sentence
        return when (reply.effectiveOutcome) {
            // Done is what every client counts as done, and what the line above recorded as one: a
            // success naming the document sent. One naming none was filed there as a failure to
            // back off from, and taken here for a replacement all the same.
            Outcome.SUCCEEDED -> when (reply.hash) {
                document.hash -> Replaced.Done("$name now carries this setup.")
                null -> Replaced.NotDone(
                    "The setup on $name may or may not have been replaced: its agent did not say which document it holds now.",
                    isError = true,
                )

                else -> Replaced.NotDone(
                    "The setup on $name may or may not have been replaced: its agent answered with a document other than the one sent.",
                    isError = true,
                )
            }

            Outcome.DEFERRED -> Replaced.NotDone(
                "The setup on $name was not replaced yet: ${ending(said ?: "it was busy with a change")} Try again once that has finished.",
                isError = false,
            )

            Outcome.FAILED -> Replaced.NotDone("The setup on $name was not replaced: ${refusal(reply)}", isError = true)

            Outcome.UNKNOWN, Outcome.PENDING -> Replaced.NotDone(
                "The setup on $name may or may not have been replaced: ${ending(said ?: "its agent could not say")}",
                isError = true,
            )
        }
    }

    /** Why a machine refused a document, as the end of a sentence. */
    private fun refusal(reply: ConfigSetReply): String {
        val problem = reply.problems?.firstOrNull()?.let { listOfNotNull(it.path, it.message).joinToString(": ") }
        val why = when {
            reply.reasonCode == ReasonCode.RESTRICTED.wire ->
                "this device's key is restricted there, and a restricted key may not publish commands that machine does not hold" +
                    (reply.launchChanges?.takeIf { it.isNotEmpty() }?.let { ": ${it.take(3).joinToString(", ")}" } ?: "")
            problem != null -> problem
            else -> reply.message ?: reply.reason?.sentence ?: "it refused the document"
        }
        return ending(why)
    }

    /** [text] closed as a sentence. */
    private fun ending(text: String): String = if (text.endsWith(".") || text.endsWith("!") || text.endsWith("?")) text else "$text."

    /**
     * Another setup, or a branch of this one: a person decides, from the machine's own bytes, so
     * they are read and verified first. A read is never held for a running operation, but it is one
     * request in flight and it backs off.
     *
     * A question is about two documents, not about a machine, and there is one of each kind on
     * screen. While one built against this head is open, no machine is read for it: the same two
     * hashes are the same question whichever machine serves them, and a machine carrying another
     * copy would replace the screen while somebody is choosing on it. Each machine is decided on
     * again at its next status once the open question is settled.
     */
    private suspend fun ask(
        client: MachineClient,
        decision: SetupDecision,
        head: SetupHead,
        mark: ControllerMark,
        status: AgentStatus,
        state: SetupPeerState,
    ): String? {
        val name = client.machine.name
        val remote = mark.hash ?: return null
        if (!mayAsk(decision, head.hash, remote)) return null
        if (state.inFlight || state.backingOff(head.hash, clock())) return null
        val key = settledKey(head.hash, remote, status.version)
        if (key in state.settled) return null

        val fetched = read(client, head, state) as? Read.Bytes ?: return null
        if (fetched.hash != remote) {
            // Bytes that are not what the status described cannot be shown as that document. If
            // they are their own reply's hash the machine moved on, and its next status says to
            // what; if not, an agent before 3.1.0 rebuilt them, and only an updated one can say.
            if (fetched.replyHash == fetched.hash) return null
            state.settled += key
            return unverifiable(name, if (decision == SetupDecision.DIVERGED) "merged" else "weighed against this one")
        }
        if (!fetched.verdict.valid) {
            state.settled += key
            return "$name is carrying a setup this app cannot use. ${fetched.verdict.errors.firstOrNull()?.message.orEmpty()}".trim()
        }
        // The read was suspended: the document in force may have moved, and another machine may
        // have put a question up meanwhile. Either way the next status decides.
        if (services.document.value?.hash != head.hash || !mayAsk(decision, head.hash, remote)) return null
        return if (decision == SetupDecision.DIVERGED) {
            services.recordDivergence(
                client.machine.id,
                name,
                "This device and $name each have a copy of the setup that was edited separately, and neither was made from the other.",
                fetched.text,
            )
            "$name has a copy of the setup that was edited separately. It is waiting to be merged."
        } else {
            services.recordIdentityClash(
                client.machine.id,
                name,
                "This device has the setup \"${head.id}\" and $name is carrying \"${fetched.id ?: "one with no id"}\". " +
                    "Two setups cannot be merged; one of them has to replace the other.",
                fetched.text,
            )
            "$name is carrying a different setup. It is waiting for a decision."
        }
    }

    /**
     * Whether the question about [remote] over [head] may be put to a person now: it was not left,
     * and no question of its kind built against [head] is open, whichever machine it came from and
     * whatever copy it is about. One built against an earlier head is out of date and gives way.
     */
    private fun mayAsk(decision: SetupDecision, head: String, remote: String): Boolean {
        if ("$head $remote" in left) return false
        val open = if (decision == SetupDecision.DIVERGED) {
            services.divergence.value?.mineProvenance
        } else {
            services.identityClash.value?.mineProvenance
        }
        return open?.hash != head
    }

    /** Why a copy an agent before 3.1.0 rebuilt cannot be [used]: its bytes are not the ones it hashed. */
    private fun unverifiable(name: String, used: String): String =
        "$name's control agent has to be updated before its setup can be $used: what it sent does not match the " +
            "hash it reported."

    /** What reading `config` gave. */
    private sealed interface Read {
        data class Bytes(
            val text: String,
            val hash: String,
            val replyHash: String,
            val id: String?,
            val lineage: List<String>,
            val verdict: DocumentAcceptance.Verdict,
        ) : Read

        /** The machine holds nothing now: it changed since the status. */
        data object Empty : Read

        data object Failed : Read
    }

    /**
     * One `config` read, counted as a failed request when it gets no answer, an answer that is no
     * document, or a document without a `hash`, which section 9.5 never adopts.
     */
    private suspend fun read(client: MachineClient, head: SetupHead, state: SetupPeerState): Read {
        state.inFlight = true
        val result = try {
            client.fetchSetup()
        } finally {
            state.inFlight = false
        }
        val fetched = result.getOrNull()
        if (fetched == ControllerFetch.NothingStored) return Read.Empty
        val document = fetched as? ControllerFetch.Document
        val replyHash = document?.replyHash
        if (document == null || replyHash == null) {
            state.failed(head.hash, clock())
            return Read.Failed
        }
        val verdict = DocumentAcceptance.check(document.text, Intake.PEER)
        val block = verdict.document?.get("controller") as? SetupJson.Object
        return Read.Bytes(
            text = document.text,
            hash = CanonicalSetup.hashOf(document.text),
            replyHash = replyHash,
            id = (block?.get("id") as? SetupJson.Text)?.value,
            lineage = (block?.get("lineage") as? SetupJson.Array)?.items?.mapNotNull { (it as? SetupJson.Text)?.value }.orEmpty(),
            verdict = verdict,
        )
    }

    /** Why a push was held, as the person reads it (contract section 9.5). */
    private fun held(name: String, reply: ConfigSetReply): String = when (reply.reasonCode) {
        ReasonCode.RESTRICTED.wire ->
            "$name takes this revision only from a device whose key is not restricted. A device with full " +
                "access sends it there on its next check."

        else -> {
            val problem = reply.problems?.firstOrNull()?.let { listOfNotNull(it.path, it.message).joinToString(": ") }
            "$name refused this revision of the setup" + (problem?.let { ": $it." } ?: ".")
        }
    }

    private fun because(reason: String?): String = when (reason) {
        "machine-empty" -> " It was not carrying a setup yet."
        "machine-unidentified" -> " It was carrying a copy that did not say which setup it belongs to."
        "machine-behind" -> " It was carrying a copy this one was made from."
        else -> ""
    }

    private fun SetupPeerState.backingOff(hash: String, now: Long): Boolean =
        failedAt[hash]?.let { now - it < SETUP_BACKOFF_MS } == true

    private companion object {
        const val MAX_ROUNDS = 4
    }
}
