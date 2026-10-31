package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.LaunchGate
import com.x1f4r.legioncontrol.agent.SetupProvenance
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.ControllerDocument
import com.x1f4r.legioncontrol.data.DocumentAcceptance
import com.x1f4r.legioncontrol.data.SetupDivergence
import com.x1f4r.legioncontrol.data.SetupIdentityClash
import com.x1f4r.legioncontrol.data.SetupOutcome
import kotlinx.coroutines.flow.StateFlow
import java.lang.reflect.Proxy

/**
 * The services the setup's models read and write, backed by a real store.
 *
 * Nothing else is reachable: a model that reaches for a machine, a key or the network fails the test
 * that made it, so what the test sees in the store is everything the model did.
 */
internal fun servicesOver(store: ConfigStore): ControlServices {
    val unused = unusable<ControlServices>("the setup")
    return object : ControlServices by unused {
        override val document: StateFlow<ControllerDocument?> get() = store.document
        override val config: StateFlow<ControllerConfig?> get() = store.config
        override val launch: StateFlow<LaunchGate.Effective> get() = store.launch
        override val provenance: StateFlow<SetupProvenance?> get() = store.provenance
        override val acceptance: StateFlow<DocumentAcceptance.Verdict?> get() = store.acceptance
        override val divergence: StateFlow<SetupDivergence?> get() = store.divergence
        override val identityClash: StateFlow<SetupIdentityClash?> get() = store.identityClash
        override fun deviceName(): String = "Test phone"
        override fun applyEdit(document: ControllerDocument, deviceName: String?): String? = store.applyEdit(document, deviceName)
        override fun applyEnrolment(text: String, servedHash: String?, expectedHead: String?, servedBy: String, deviceName: String?): SetupOutcome =
            store.applyEnrolment(text, servedHash, expectedHead, servedBy, deviceName)

        override fun appliedHash(): String? = store.appliedHash()
        override fun adoptFromMachine(text: String, expectedRemoteHash: String?, expectedLocalHash: String?, machineName: String): SetupOutcome =
            store.adoptFromMachine(text, expectedRemoteHash, expectedLocalHash, machineName)

        override fun recordDivergence(machineId: String, machineName: String, reason: String, theirText: String) =
            store.recordDivergence(machineId, machineName, reason, theirText)

        override fun recordIdentityClash(machineId: String, machineName: String, reason: String, theirText: String) =
            store.recordIdentityClash(machineId, machineName, reason, theirText)

        override fun clearDivergence() = store.clearDivergence()
        override fun clearIdentityClash() = store.clearIdentityClash()
        override fun applyMerge(mergedText: String, expectedHead: String?): SetupOutcome = store.applyMerge(mergedText, expectedHead)
        override fun revision(hash: String?): String? = store.revision(hash)
    }
}

/** An implementation of [T] every call of which fails, naming what the test is limited to. */
internal inline fun <reified T> unusable(limitedTo: String): T =
    Proxy.newProxyInstance(T::class.java.classLoader, arrayOf(T::class.java)) { _, method, _ ->
        throw UnsupportedOperationException("${method.name} is not part of $limitedTo")
    } as T
