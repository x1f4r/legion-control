package com.x1f4r.legioncontrol.data

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * What is true about this phone in particular, and is never published.
 *
 * The shared document describes the fleet: which machines exist, how to reach them, which systems
 * they boot. None of that is about the device holding it. A key path, an ssh alias, which machine
 * "I" am, which site I am standing on: those are per device, and putting them in the document was
 * what forced one device to be the authority in the first place, because publishing them would
 * overwrite everybody else's.
 *
 * So they live here instead, in ordinary app-private preferences, and they are never sent anywhere.
 * That is also why the phone can now be a peer: it can edit the shared half without touching
 * anybody's private half.
 *
 * The phone's binding is deliberately small. It has no key path (the key is generated on the device
 * and never leaves it), no ssh alias (there is no ~/.ssh/config here), and no local agent (a phone
 * cannot run one), which leaves the device name and the site the user says they are on.
 */
data class DeviceBindings(
    /**
     * What to call this device in a setup revision, so a divergence screen can say who wrote what.
     *
     * Free text, and free of anything identifying by default: it starts empty and the user fills it
     * in. It is the one thing here that does leave the device, inside `controller.device`.
     */
    val deviceName: String = "",

    /**
     * Which site the user says this device is standing on.
     *
     * A hint for routing and waking, and never a proof. Two houses behind the same router model have
     * the same private subnet, so an address that starts with 192.168.178. cannot tell them apart;
     * where several sites match, the app says it is not sure rather than picking the first. This is
     * how somebody settles that, and it is still only a hint: what authenticates a machine is its
     * pinned host key, never its address.
     */
    val currentSite: String? = null,
)

class BindingsStore(context: Context) {
    private val prefs = context.applicationContext
        .getSharedPreferences("legion-bindings", Context.MODE_PRIVATE)

    private val _bindings = MutableStateFlow(read())

    val bindings: StateFlow<DeviceBindings> = _bindings.asStateFlow()

    fun setDeviceName(name: String) {
        prefs.edit().putString(KEY_DEVICE_NAME, name.trim()).apply()
        _bindings.value = read()
    }

    /** Null clears it, which puts the app back to working the site out from the addresses. */
    fun setCurrentSite(siteId: String?) {
        prefs.edit().apply {
            if (siteId.isNullOrBlank()) remove(KEY_CURRENT_SITE) else putString(KEY_CURRENT_SITE, siteId)
        }.apply()
        _bindings.value = read()
    }

    private fun read() = DeviceBindings(
        deviceName = prefs.getString(KEY_DEVICE_NAME, null).orEmpty(),
        currentSite = prefs.getString(KEY_CURRENT_SITE, null)?.takeIf { it.isNotBlank() },
    )

    private companion object {
        const val KEY_DEVICE_NAME = "device-name"
        const val KEY_CURRENT_SITE = "current-site"
    }
}
