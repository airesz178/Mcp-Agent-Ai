package ai.mcpagent.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * Internal target of the per-request, mutable, one-shot PendingIntent sent
 * ONLY to com.termux.app.RunCommandService. The manifest marks this receiver
 * exported=false; a random URI nonce and request UUID are checked by the
 * in-process coordinator. Results arriving after an app restart/timeout are
 * discarded because there is no longer a pending request.
 */
class TermuxRunResultReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent?) {
    AppTerminalPlugin.deliverTermuxResult(intent)
  }
}
