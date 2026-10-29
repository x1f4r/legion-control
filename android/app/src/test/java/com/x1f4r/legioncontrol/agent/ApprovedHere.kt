package com.x1f4r.legioncontrol.agent

import com.x1f4r.legioncontrol.data.ControllerConfig
import com.x1f4r.legioncontrol.data.LaunchValues
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject

/**
 * [this] as the launch gate gives it to a device that approved every command in it, which is what an
 * upgraded device holding this document seeds itself with.
 *
 * For tests about something other than approval. Machines are only ever built from the gate's
 * configuration, so a test that wants a machine whose systems are driven has to say whose approval
 * that is, and this is the device that approved all of them.
 */
internal fun ControllerConfig.approvedHere(): LaunchGate.Effective {
    val document = Json.encodeToJsonElement(ControllerConfig.serializer(), this).jsonObject
    return LaunchGate.effective(this, document, LaunchValues.approvalsFrom(document))
}
