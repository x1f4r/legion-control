package com.x1f4r.legioncontrol.agent

/**
 * Setup sync: what one status reply means for the copy of the setup a machine holds, and when this
 * device may do something about it (contract section 9.5).
 *
 * Every device decides on its own, from one status at a time, whether a machine is behind it, ahead
 * of it, holding another setup or holding a branch of this one. This app used to remember "already
 * handled" per machine hash for the whole session, retried a refused push every fifteen seconds and
 * pushed into a machine that was busy updating (C7). So there is one function per step, each a port
 * of the same function in `contract/tools/setup-sync.mjs`, held to
 * `contract/vectors/setup-reconcile.json`:
 *
 * - [decide], the eleven ordered rules, from one status;
 * - [admit], whether the bytes a fetch read may replace the applied document;
 * - [gate], the guardrails: one request at a time, a back-off, no push during a running mutation or
 *   after a refusal no retry can fix;
 * - [afterPush], what a push reply means for the next decision.
 *
 * Nothing here has a clock or does any I/O. A time is always an argument, so the vectors pin a
 * back-off to the millisecond, and the per-machine memory is [SetupPeerState].
 */

/** What to do about the copy one machine holds. */
enum class SetupDecision(val wire: String) {
    /** The agent cannot carry a setup. Left out, never a conflict. */
    UNSUPPORTED("unsupported"),

    /** This device has nothing to compare, or a document with no identity yet. */
    UNKNOWN("unknown"),

    /** The machine holds nothing, a copy nobody claims, or an ancestor of this one. */
    PUSH("push"),

    IN_SYNC("inSync"),

    /** Another setup entirely: a person replaces it, or follows it here. */
    DIFFERENT_SETUP("differentSetup"),

    /** One `config meta` round trip settles it. */
    READ_META("readMeta"),

    /** The machine's copy descends from this one. */
    FETCH("fetch"),

    /** Neither descends from the other. A person merges it. */
    DIVERGED("diverged"),
}

/** A decision, why, and for a fetch the hash the fetched bytes must have. */
data class SetupDecided(val decision: SetupDecision, val reason: String? = null, val expect: String? = null)

/** This device's applied document, as [decide] reads it. [id] is null on a document with no identity yet. */
data class SetupHead(val hash: String, val id: String?, val lineage: List<String>)

/** What this device filed about a hash a machine reported: the id and lineage of that document. */
data class SetupMeta(val id: String?, val lineage: List<String>)

/** How long nothing automatic goes to a machine for the same applied hash after a failed request. */
const val SETUP_BACKOFF_MS = 60_000L

/**
 * The first of the eleven rules that matches.
 *
 * [head] is this device's applied document, or null when it has none (or only a hand edit not yet
 * given a revision). [contract] and [mark] are the status reply's `contract` and `controller`.
 * [metaByHash] holds `config meta` answers and the `current` of push refusals, by the hash they
 * describe: a hash fixes a document, so an entry never goes stale.
 *
 * Rule 7 is checked before rule 8 on purpose. A machine that holds another setup whose hash happens
 * to be in this device's lineage is still another setup, and pushing over it would replace somebody
 * else's fleet.
 */
fun decide(head: SetupHead?, contract: Int?, mark: ControllerMark?, metaByHash: Map<String, SetupMeta>): SetupDecided {
    if (contract == null || contract < Contract.REQUIRED || mark == null) return SetupDecided(SetupDecision.UNSUPPORTED)
    if (head == null) return SetupDecided(SetupDecision.UNKNOWN, "no-local")
    if (head.id == null) return SetupDecided(SetupDecision.UNKNOWN, "local-unidentified")
    val hash = mark.hash ?: return SetupDecided(SetupDecision.PUSH, "machine-empty")
    if (hash == head.hash) return SetupDecided(SetupDecision.IN_SYNC)
    val meta = metaByHash[hash]
    val id = if (meta != null) meta.id else mark.id
    if (id == null) return SetupDecided(SetupDecision.PUSH, "machine-unidentified")
    if (id != head.id) return SetupDecided(SetupDecision.DIFFERENT_SETUP)
    if (hash in head.lineage) return SetupDecided(SetupDecision.PUSH, "machine-behind")
    if (meta == null) return SetupDecided(SetupDecision.READ_META)
    if (head.hash in meta.lineage) return SetupDecided(SetupDecision.FETCH, expect = hash)
    return SetupDecided(SetupDecision.DIVERGED)
}

/** Whether fetched bytes may be adopted, in the order [admit] checks. */
enum class SetupAdmission(val wire: String) {
    /** The applied document changed while the bytes were read. Nothing is written; decide again. */
    HEAD_MOVED("headMoved"),

    /** The machine changed since the decision. Decide on the next status. */
    MOVED("moved"),

    /** The bytes do not hash to what the machine reported: an agent before 3.1.0 that sends no `raw`. */
    UNVERIFIABLE("unverifiable"),

    /** The bytes fail section 9.7 under peer intake. */
    INVALID("invalid"),

    /** The bytes' own identity is another setup, or does not descend from this device's document. */
    INCONSISTENT("inconsistent"),

    ADOPT("adopt"),
}

/** What a decision to fetch was made from: the applied hash and setup id, and the remote hash that justified it. */
data class SetupExpectation(val localHash: String, val localId: String, val remoteHash: String)

/** The bytes a fetch read: their canonical hash, the reply's `hash`, and the id and lineage of their own controller block. */
data class SetupFetched(val hash: String, val replyHash: String, val id: String?, val lineage: List<String>)

/**
 * Whether the bytes a fetch read may be adopted.
 *
 * The head check is first because it is the one that protects a person: an edit saved while the read
 * was suspended must never be overwritten by bytes that were chosen against the document it replaced
 * (A15). [currentLocalHash] is the applied hash read again from storage now, after the read. A hash
 * that is not the expected one but is the reply's own means the machine moved on; one that is
 * neither means the bytes could not be reproduced (C1). [accepted] is section 9.7 under peer intake.
 *
 * A `config` reply without a `hash` never gets here: it describes no document and counts as a failed
 * request before anything is admitted.
 */
fun admit(expected: SetupExpectation, currentLocalHash: String?, fetched: SetupFetched, accepted: Boolean): SetupAdmission {
    if (currentLocalHash != expected.localHash) return SetupAdmission.HEAD_MOVED
    if (fetched.hash != expected.remoteHash) {
        return if (fetched.replyHash == fetched.hash) SetupAdmission.MOVED else SetupAdmission.UNVERIFIABLE
    }
    if (!accepted) return SetupAdmission.INVALID
    if (fetched.id != expected.localId || expected.localHash !in fetched.lineage) return SetupAdmission.INCONSISTENT
    return SetupAdmission.ADOPT
}

/**
 * The admission outcomes a device remembers, per [settledKey], so it does not read the same refused
 * document again on every status. Nothing else is remembered as handled.
 */
val REMEMBERED_ADMISSIONS: Set<SetupAdmission> =
    setOf(SetupAdmission.INVALID, SetupAdmission.UNVERIFIABLE, SetupAdmission.INCONSISTENT)

/** The key a remembered admission is filed under: it changes when either hash or the agent does. */
fun settledKey(localHash: String, remoteHash: String?, agentVersion: String?): String = "$localHash $remoteHash $agentVersion"

/** Whether the request a decision calls for goes out now. */
data class SetupGate(val action: Action, val reason: String? = null) {
    enum class Action(val wire: String) { ACT("act"), HOLD("hold"), NONE("none") }
}

/**
 * Whether the request [decision] calls for goes out now.
 *
 * Only push, readMeta and fetch send anything; every other decision is [SetupGate.Action.NONE]. One
 * request is in flight per machine, and a failure holds everything automatic for the same applied
 * hash for 60 seconds. The rest applies to pushes only, because reading a machine never changes it: a
 * device whose own document has a section 9.7 error publishes nothing, a push the machine refused
 * with `restricted` or `config-invalid` is not sent again for that hash, and nothing is pushed while
 * this device has an action in flight on the machine, while its last status lists a running
 * operation, or after a lock refusal until a status read after it says nothing is running.
 */
fun gate(
    decision: SetupDecision,
    localHash: String,
    remoteHash: String?,
    agentVersion: String?,
    now: Long,
    state: SetupPeerState,
): SetupGate {
    if (decision != SetupDecision.PUSH && decision != SetupDecision.READ_META && decision != SetupDecision.FETCH) {
        return SetupGate(SetupGate.Action.NONE)
    }
    if (state.inFlight) return SetupGate(SetupGate.Action.HOLD, "in-flight")
    val failed = state.failedAt[localHash]
    if (failed != null && now - failed < SETUP_BACKOFF_MS) return SetupGate(SetupGate.Action.HOLD, "backoff")
    if (decision == SetupDecision.PUSH) {
        if (state.localInvalid) return SetupGate(SetupGate.Action.HOLD, "local-invalid")
        if (state.held[localHash] != null) return SetupGate(SetupGate.Action.HOLD, "held")
        val lockedAt = state.lockedAt
        val stillLocked = lockedAt != null && (state.statusAt ?: Long.MIN_VALUE) <= lockedAt
        if (state.localMutation || state.runningOperations > 0 || stillLocked) {
            return SetupGate(SetupGate.Action.HOLD, "operation-running")
        }
    }
    if (decision == SetupDecision.FETCH && settledKey(localHash, remoteHash, agentVersion) in state.settled) {
        return SetupGate(SetupGate.Action.HOLD, "settled")
    }
    return SetupGate(SetupGate.Action.ACT)
}

/** What a push reply means for the next decision. */
data class SetupPushEffect(val effect: Effect, val metaHash: String? = null) {
    enum class Effect(val wire: String) { CLEAR("clear"), FAIL("fail"), FILE_META("fileMeta"), LOCKED("locked"), HOLD("hold") }
}

/**
 * What a push reply means.
 *
 * [reply] is the decoded reply, or null when none was read: no answer, a dropped link, a timeout,
 * output that is not a reply. That and every refusal not named here is a failure to back off from.
 * `stale-revision` and `controller-conflict` are not failures: they say what the machine holds, and
 * `current` is filed and decided on at once. A lock refusal waits for the operation to finish.
 * `restricted` and `config-invalid` cannot be cured by trying again, so the push is held for that
 * hash until it changes or a person asks. A reply that stored something other than the applied hash
 * is a failure too: the machine now holds a document this device did not send.
 *
 * Only a reply with `ok: false` is a refusal, and a `current` without a hash has nothing to be filed
 * under, so both of those readings are failures on purpose.
 */
fun afterPush(reply: ConfigSetReply?, localHash: String): SetupPushEffect {
    if (reply == null) return SetupPushEffect(SetupPushEffect.Effect.FAIL)
    if (reply.ok == true) {
        return SetupPushEffect(if (reply.hash == localHash) SetupPushEffect.Effect.CLEAR else SetupPushEffect.Effect.FAIL)
    }
    if (reply.ok != false) return SetupPushEffect(SetupPushEffect.Effect.FAIL)
    val code = reply.reasonCode
    val current = reply.current?.hash
    if ((code == ReasonCode.STALE_REVISION.wire || code == ReasonCode.CONTROLLER_CONFLICT.wire) && current != null) {
        return SetupPushEffect(SetupPushEffect.Effect.FILE_META, current)
    }
    if (code == ReasonCode.OPERATION_IN_PROGRESS.wire || code == ReasonCode.LOCK_HELD.wire) {
        return SetupPushEffect(SetupPushEffect.Effect.LOCKED)
    }
    if (code == ReasonCode.RESTRICTED.wire || code == ReasonCode.CONFIG_INVALID.wire) {
        return SetupPushEffect(SetupPushEffect.Effect.HOLD)
    }
    return SetupPushEffect(SetupPushEffect.Effect.FAIL)
}

/**
 * What this device remembers about one machine's copy of the setup, in memory only.
 *
 * [metaByHash] is what [decide] reads, the cache section 9.5 calls `meta`; the rest is what [gate]
 * reads. Times are milliseconds on this device's own clock and are only ever compared with each
 * other. It is touched from the thread the polls run on and nowhere else.
 */
class SetupPeerState {
    /** At most this many hashes are kept: a machine moves through few, and an old one is cheap to ask about again. */
    private val metas = object : LinkedHashMap<String, SetupMeta>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, SetupMeta>?): Boolean = size > META_LIMIT
    }

    val metaByHash: Map<String, SetupMeta> get() = metas

    var inFlight: Boolean = false
    val failedAt: MutableMap<String, Long> = mutableMapOf()

    /** The refusal code a push was held for, by the applied hash it was pushed from. */
    val held: MutableMap<String, String> = mutableMapOf()
    var localInvalid: Boolean = false
    var localMutation: Boolean = false
    var runningOperations: Int = 0
    var lockedAt: Long? = null
    var statusAt: Long? = null
    val settled: MutableSet<String> = linkedSetOf()

    /** Files what a hash describes. A hash fixes a document, so filing it twice changes nothing. */
    fun file(hash: String, meta: SetupMeta) {
        metas[hash] = meta
    }

    /** A request failed: nothing automatic goes to the machine for [localHash] for a while. */
    fun failed(localHash: String, now: Long) {
        failedAt[localHash] = now
    }

    /**
     * The state after a push reply: [SetupPushEffect.Effect.CLEAR] forgets the failure, FAIL starts
     * the back-off, FILE_META files `current` under its hash, LOCKED waits for a later status and
     * HOLD remembers the refusal code for the applied hash.
     */
    fun record(effect: SetupPushEffect, reply: ConfigSetReply?, localHash: String, now: Long) {
        inFlight = false
        when (effect.effect) {
            SetupPushEffect.Effect.CLEAR -> failedAt.remove(localHash)
            SetupPushEffect.Effect.FAIL -> failedAt[localHash] = now
            SetupPushEffect.Effect.FILE_META -> reply?.current?.let { current ->
                file(effect.metaHash!!, SetupMeta(current.id?.takeIf { it.isNotBlank() }, current.lineage.orEmpty()))
            }

            SetupPushEffect.Effect.LOCKED -> lockedAt = now
            SetupPushEffect.Effect.HOLD -> held[localHash] = reply?.reasonCode.orEmpty()
        }
    }

    /** "Try again": the one thing besides a new applied hash that lifts a hold. */
    fun retry(localHash: String) {
        held.remove(localHash)
    }

    private companion object {
        const val META_LIMIT = 16
    }
}
