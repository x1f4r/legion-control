package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.LaunchGate
import com.x1f4r.legioncontrol.data.ConfigStore
import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.ControllerDocument
import kotlinx.coroutines.flow.StateFlow
import java.lang.reflect.Proxy

/**
 * The services the setup's models read and write, backed by a real store.
 *
 * Nothing else is reachable: a model that reaches for a machine, a key or the network fails the test
 * that made it, so what the test sees in the store is everything the model did.
 */
internal fun servicesOver(store: ConfigStore): ControlServices {
    val unused = Proxy.newProxyInstance(ControlServices::class.java.classLoader, arrayOf(ControlServices::class.java)) { _, method, _ ->
        throw UnsupportedOperationException("${method.name} is not part of the setup")
    } as ControlServices
    return object : ControlServices by unused {
        override val document: StateFlow<ControllerDocument?> get() = store.document
        override val config: StateFlow<ControllerConfig?> get() = store.config
        override val launch: StateFlow<LaunchGate.Effective> get() = store.launch
        override fun deviceName(): String = "Test phone"
        override fun applyEdit(document: ControllerDocument, deviceName: String?): String? = store.applyEdit(document, deviceName)
        override fun applyEnrolment(text: String, deviceName: String?): String? = store.applyEnrolment(text, deviceName)
    }
}
