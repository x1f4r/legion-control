package com.x1f4r.legion

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import org.json.JSONObject

/**
 * Non-exported receiver for Pause, Resume, and Cancel action buttons on the transfer notification.
 */
class TransferActionReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    val actionName = when (intent.action) {
      ACTION_PAUSE -> "pause"
      ACTION_RESUME -> "resume"
      ACTION_CANCEL -> "cancel"
      else -> return
    }
    val transferId = intent.getStringExtra(EXTRA_TRANSFER_ID).orEmpty()
    TransferBridge.sendEvent(JSONObject().apply {
      put("event", "notification_action")
      put("action", actionName)
      if (transferId.isNotEmpty()) {
        put("transferId", transferId)
      }
    })
  }

  companion object {
    const val ACTION_PAUSE = "com.x1f4r.legion.TRANSFER_PAUSE"
    const val ACTION_RESUME = "com.x1f4r.legion.TRANSFER_RESUME"
    const val ACTION_CANCEL = "com.x1f4r.legion.TRANSFER_CANCEL"
    const val EXTRA_TRANSFER_ID = "transfer_id"
  }
}
