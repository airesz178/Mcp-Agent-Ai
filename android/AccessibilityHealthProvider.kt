package ai.mcpagent.app

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.Bundle
import android.os.Process
import android.util.Log

/**
 * Private same-package IPC probe for the OS-bound AccessibilityService.
 *
 * This provider runs in :accessibility, alongside the service. In addition
 * to the binding probe it handles the same-UID start/stop/status control for
 * a visible, user-authorized app automation indicator. Only same-UID
 * explicitly approved action requests can cross to the OS-bound service.
 * Exported=false in AndroidManifest; other applications cannot call it.
 */
class AccessibilityHealthProvider : ContentProvider() {
  companion object {
    private val packagePattern = Regex("^[a-zA-Z_][a-zA-Z0-9_]*(\\.[a-zA-Z_][a-zA-Z0-9_]*)+$")
    private val snapshotPattern = Regex("^[0-9a-fA-F-]{36}$")
    private fun sessionTarget(extras: Bundle?): Boolean {
      val session = extras?.getString("sessionId").orEmpty()
      val target = extras?.getString("targetPackage").orEmpty()
      return session.length in 8..128 &&
        session.all { it.isLetterOrDigit() || it == '-' || it == '_' } &&
        target.length in 3..200 && packagePattern.matches(target) &&
        target != "ai.mcpagent.app" && target != "android" &&
        !target.startsWith("com.android.") &&
        !target.startsWith("com.google.android.permissioncontroller")
    }
    private fun approved(extras: Bundle?): Boolean =
      extras?.getBoolean("approved", false) == true && sessionTarget(extras)
    private fun error(reason: String): Bundle = Bundle().apply {
      putBoolean("ok", false); putString("reason", reason)
    }
  }
  override fun onCreate(): Boolean = true

  override fun call(method: String, arg: String?, extras: Bundle?): Bundle? {
    // The provider is also exported=false. Refuse a different caller UID even
    // if an OEM/provider framework accidentally makes the authority visible.
    if (Binder.getCallingUid() != Process.myUid()) return null
    return when (method) {
      "status" -> {
        val boundSince = AgentAccessibilityService.connectedSinceElapsedMs()
        val connected = AgentAccessibilityService.isConnected() && boundSince > 0
        Log.i("McpAutoIndicator", "health_provider_status connected=$connected boundSincePresent=${boundSince > 0}")
        Bundle().apply {
          putBoolean("connected", connected)
          putLong("connectedSinceElapsedMs", if (connected) boundSince else 0L)
        }
      }
      "automationStart" -> {
        val sessionId = extras?.getString("sessionId").orEmpty()
        val targetPackage = extras?.getString("targetPackage").orEmpty()
        if (sessionId.length !in 8..128 ||
            !sessionId.all { it.isLetterOrDigit() || it == '-' || it == '_' } ||
            targetPackage.length !in 3..200 || !packagePattern.matches(targetPackage) ||
            targetPackage == context?.packageName || targetPackage == "android" ||
            targetPackage.startsWith("com.android.") ||
            targetPackage.startsWith("com.google.android.permissioncontroller")) {
          Bundle().apply {
            putBoolean("active", false)
            putBoolean("visible", false)
            putString("reason", "invalid_target_or_session")
          }
        } else {
          AgentAccessibilityService.automationStart(sessionId, targetPackage)
        }
      }
      "automationStop" -> AgentAccessibilityService.automationStop()
      "automationStatus" -> AgentAccessibilityService.automationStatus()
      "automationCapabilities" -> AgentAccessibilityService.automationCapabilities()
      "automationSnapshot" -> {
        val mode = extras?.getString("selectedMode").orEmpty()
        if (!approved(extras) || mode !in listOf("realtime", "screenshot_click"))
          error("approval_or_target_invalid")
        else AgentAccessibilityService.automationSnapshot(
          extras!!.getString("sessionId").orEmpty(),
          extras.getString("targetPackage").orEmpty(), mode)
      }
      "automationClick" -> {
        val mode = extras?.getString("selectedMode").orEmpty()
        if (!approved(extras) || !snapshotPattern.matches(extras!!.getString("snapshotId").orEmpty()) ||
            mode !in listOf("realtime", "screenshot_click") ||
            extras.getInt("index", 0) !in 0..48)
          error("approval_or_target_invalid")
        else AgentAccessibilityService.automationClick(extras)
      }
      "automationFill" -> {
        val content = extras?.getString("text").orEmpty()
        if (!approved(extras) || !snapshotPattern.matches(extras!!.getString("snapshotId").orEmpty()) ||
            extras.getInt("index", 0) !in 1..48 ||
            content.length !in 1..512 || content.any { it.code == 0 || Character.isISOControl(it) })
          error("approval_or_target_invalid")
        else AgentAccessibilityService.automationFill(extras)
      }
      "automationGesture" -> {
        val mode = extras?.getString("selectedMode").orEmpty()
        val kind = extras?.getString("kind").orEmpty()
        if (!approved(extras) || !snapshotPattern.matches(extras!!.getString("snapshotId").orEmpty()) ||
            mode !in listOf("realtime", "screenshot_click") ||
            kind !in listOf("tap", "swipe"))
          error("approval_or_target_invalid")
        else AgentAccessibilityService.automationGesture(extras)
      }
      else -> null
    }
  }

  override fun query(
    uri: Uri,
    projection: Array<out String>?,
    selection: String?,
    selectionArgs: Array<out String>?,
    sortOrder: String?
  ): Cursor? = null
  override fun getType(uri: Uri): String? = null
  override fun insert(uri: Uri, values: ContentValues?): Uri? = null
  override fun update(uri: Uri, values: ContentValues?, selection: String?,
    selectionArgs: Array<out String>?): Int = 0
  override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = 0
}
