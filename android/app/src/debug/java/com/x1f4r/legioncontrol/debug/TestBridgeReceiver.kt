package com.x1f4r.legioncontrol.debug

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import com.x1f4r.legioncontrol.agent.LegionControl
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

/**
 * The one way to drive this app from a test machine, and it exists only in a debug build.
 *
 * Setting a phone up by hand means reading a public key off a screen and typing a document into a
 * text field, which is fine once and impossible in a test loop. This receiver does those two things
 * over adb and nothing else: it prints the key, and it applies a setup.
 *
 * It is deliberately absent from a release build. It lives in `src/debug`, so the class and its
 * manifest entry are not merged into a release APK at all, and there is no flag anywhere that could
 * turn it back on. The debug manifest exports it because Android applies component export rules to
 * explicit broadcasts from uid 2000 too, then protects it with `android.permission.DUMP`. The adb
 * shell holds that privileged permission and an ordinary installed app cannot request it.
 *
 * No test key, host or address is compiled into any build. Everything comes in as an extra.
 */
class TestBridgeReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        val app = context.applicationContext
        when (intent.action) {
            ACTION_SHOW_KEY -> {
                val pending = goAsync()
                CoroutineScope(Dispatchers.IO).launch {
                    try {
                        val identity = LegionControl(app).identity.identity()
                        Log.i(TAG, "authorized_keys line: ${identity.authorizedKeysLine}")
                        Log.i(TAG, "fingerprint: ${identity.fingerprint}")
                    } catch (failure: Exception) {
                        Log.e(TAG, "could not read this device's key: ${failure.message}")
                    } finally {
                        pending.finish()
                    }
                }
            }

            ACTION_APPLY_CONFIG -> {
                val document = intent.getStringExtra(EXTRA_CONFIG)
                if (document.isNullOrBlank()) {
                    Log.e(TAG, "no --es config given, nothing applied")
                    return
                }
                val control = LegionControl(app)
                val failure = control.config.applyPasted(document, control.deviceName())
                if (failure == null) {
                    val provenance = control.config.provenance.value
                    Log.i(
                        TAG,
                        "applied: setup=${provenance?.authority} revision=${provenance?.revision} " +
                            "hash=${provenance?.hash}",
                    )
                } else {
                    Log.e(TAG, "refused: $failure")
                }
            }

            ACTION_SHOW_SETUP -> {
                val control = LegionControl(app)
                val provenance = control.config.provenance.value
                Log.i(
                    TAG,
                    "setup=${provenance?.authority} revision=${provenance?.revision} " +
                        "hash=${provenance?.hash} lineage=${provenance?.lineage?.size ?: 0}",
                )
            }

            else -> Log.w(TAG, "unknown action ${intent.action}")
        }
    }

    companion object {
        const val TAG = "LegionControlTest"
        const val ACTION_SHOW_KEY = "com.x1f4r.legioncontrol.debug.SHOW_KEY"
        const val ACTION_APPLY_CONFIG = "com.x1f4r.legioncontrol.debug.APPLY_CONFIG"
        const val ACTION_SHOW_SETUP = "com.x1f4r.legioncontrol.debug.SHOW_SETUP"
        const val EXTRA_CONFIG = "config"
    }
}
