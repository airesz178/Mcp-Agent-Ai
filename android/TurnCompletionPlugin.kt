package ai.mcpagent.app

import android.Manifest
import android.app.Activity
import android.app.Application
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.concurrent.atomic.AtomicInteger

@InvokeArg
class TurnCompletionArgs {
  var mode: String = ""
  var message: String = ""
  var notificationPriority: String = ""
  var userInitiated: Boolean = false
}

/**
 * Optional, one-shot notification for a verified completed Android chat turn.
 *
 * The renderer may request this ONLY after the user has chosen a notification
 * preference and a real reply has completed. userInitiated is the caller's
 * assertion of that prior authorization, not a new permission grant. Android
 * always checks the current OS/channel permission and never opens a prompt.
 *
 * This notification uses its own channel, ID range and tag. It does not touch
 * ForegroundRuntimeService, the Termux-style session notification, wake locks
 * or the background session service. It observes Activity resume/pause solely
 * to decide whether a finished-reply alert matches the user's delivery choice.
 */
@TauriPlugin
class TurnCompletionPlugin(private val activity: Activity) : Plugin(activity) {
  // Lifecycle callbacks are authoritative even when a foreground SERVICE
  // keeps the app process alive after the Activity goes into the background.
  // Do not infer Activity visibility from process importance.
  private var activityResumed = false
  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, state: Bundle?) {}
    override fun onActivityStarted(a: Activity) {}
    override fun onActivityResumed(a: Activity) {
      if (a === activity) activityResumed = true
    }
    override fun onActivityPaused(a: Activity) {
      if (a === activity) activityResumed = false
    }
    override fun onActivityStopped(a: Activity) {}
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) {}
    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        activityResumed = false
        activity.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }
  init { activity.application.registerActivityLifecycleCallbacks(lifecycle) }

  companion object {
    // Existing v1 channels are immutable at IMPORTANCE_DEFAULT (no heads-up).
    // A new channel is needed for the requested pop-up alert. Do not delete
    // v1: Android retains the user's previous notification settings/history.
    private const val LEGACY_CHANNEL_ID = "mcp_agent_ai_turn_completions_v1"
    private const val CHANNEL_ID = "mcp_agent_ai_turn_completions_popup_v2"
    private const val NOTIFICATION_TAG = "mcp_agent_ai_turn_completion"
    private const val MAX_MESSAGE_CODEPOINTS = 2048
    private const val MAX_INPUT_CHARS = 65536
    private val nextNotificationId =
      AtomicInteger(60000 + (System.currentTimeMillis() % 1_000_000).toInt())

    // The same families guarded by the existing Android Review sharing path.
    // Reject sensitive text instead of potentially publishing a partial token.
    private val secrets = listOf(
      Regex("""(?i)\b(?:sk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_]{16,}|github_pat_[A-Za-z0-9_]{16,}|glpat-[A-Za-z0-9_-]{16,}|xox[baprs]-[A-Za-z0-9-]{12,}|AIza[0-9A-Za-z_-]{20,})\b"""),
      Regex("""(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}"""),
      Regex("""(?i)\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|password|authorization)\b["']?\s*[:=]\s*["']?[^\s"';,`]{6,}""")
    )

    private fun blockedFormat(codePoint: Int): Boolean =
      Character.getType(codePoint) == Character.FORMAT.toInt() ||
        codePoint in 0x202A..0x202E || codePoint in 0x2066..0x2069

    /** Collapse benign line breaks, reject control/format characters and
     * unpaired UTF-16 surrogates, then bound the visible Unicode characters. */
    internal fun prepareMessage(raw: String): String? {
      if (raw.length > MAX_INPUT_CHARS) return null
      val clean = StringBuilder()
      var index = 0
      var previousSpace = false
      while (index < raw.length) {
        val ch = raw[index]
        if (Character.isLowSurrogate(ch)) return null
        if (Character.isHighSurrogate(ch) &&
          (index + 1 >= raw.length || !Character.isLowSurrogate(raw[index + 1]))) return null
        val point = Character.codePointAt(raw, index)
        index += Character.charCount(point)
        val space = point == '\n'.code || point == '\r'.code || point == '\t'.code ||
          Character.isWhitespace(point)
        if (blockedFormat(point) || (Character.isISOControl(point) && !space)) return null
        if (space) {
          if (!previousSpace && clean.isNotEmpty()) {
            clean.append(' ')
            previousSpace = true
          }
        } else {
          clean.appendCodePoint(point)
          previousSpace = false
        }
      }
      val text = clean.toString().trim()
      if (text.isEmpty() || secrets.any { it.containsMatchIn(text) }) return null
      if (text.codePointCount(0, text.length) <= MAX_MESSAGE_CODEPOINTS) return text
      val end = text.offsetByCodePoints(0, MAX_MESSAGE_CODEPOINTS - 1)
      return text.substring(0, end).trimEnd() + "…"
    }
  }

  private fun receipt(posted: Boolean, reason: String? = null): JSObject =
    JSObject().apply {
      put("posted", posted)
      if (reason != null) put("reason", reason)
    }

  @Command
  fun notifyTurn(invoke: Invoke) {
    val args = try { invoke.parseArgs(TurnCompletionArgs::class.java) }
      catch (_: Exception) {
        invoke.resolve(receipt(false, "invalid_request"))
        return
      }
    if (!args.userInitiated) {
      invoke.resolve(receipt(false, "not_user_initiated"))
      return
    }
    if (args.mode != "full_message" && args.mode != "task_finished") {
      invoke.resolve(receipt(false, "invalid_mode"))
      return
    }
    if (args.notificationPriority != "foreground" &&
        args.notificationPriority != "background" &&
        args.notificationPriority != "both") {
      invoke.resolve(receipt(false, "invalid_priority"))
      return
    }
    // task_finished deliberately never reads or forwards a reply body.
    val body = if (args.mode == "task_finished") "Your task is finished."
      else prepareMessage(args.message)
    if (body == null) {
      invoke.resolve(receipt(false, "unsafe_message"))
      return
    }
    activity.runOnUiThread {
      try {
        if (activity.isFinishing || activity.isDestroyed) {
          invoke.resolve(receipt(false, "activity_unavailable"))
          return@runOnUiThread
        }
        // Android may initialize this Tauri plugin after the Activity's first
        // onResume event. Window focus provides the missing initial snapshot;
        // later lifecycle callbacks continue to track pause/resume changes.
        val appForeground = activityResumed ||
          (activity.window?.decorView?.hasWindowFocus() == true)
        val shouldNotify = args.notificationPriority == "both" ||
          (args.notificationPriority == "foreground" && appForeground) ||
          (args.notificationPriority == "background" && !appForeground)
        if (!shouldNotify) {
          invoke.resolve(receipt(false, "priority_filtered"))
          return@runOnUiThread
        }
        val manager = activity.getSystemService(Context.NOTIFICATION_SERVICE)
          as? NotificationManager
        if (manager == null) {
          invoke.resolve(receipt(false, "notification_unavailable"))
          return@runOnUiThread
        }
        if ((Build.VERSION.SDK_INT >= 33 &&
            activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
              PackageManager.PERMISSION_GRANTED) || !manager.areNotificationsEnabled()) {
          invoke.resolve(receipt(false, "permission_denied"))
          return@runOnUiThread
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
          // Never bypass a legacy channel that the user explicitly blocked.
          if (manager.getNotificationChannel(LEGACY_CHANNEL_ID)?.importance ==
              NotificationManager.IMPORTANCE_NONE) {
            invoke.resolve(receipt(false, "channel_disabled"))
            return@runOnUiThread
          }
          manager.createNotificationChannel(NotificationChannel(
            CHANNEL_ID, "Completed ChatGPT turns · Pop-up", NotificationManager.IMPORTANCE_HIGH
          ).apply {
            description = "Pop-up alerts for finished Mcp Agent Ai replies and tasks"
            setShowBadge(true)
          })
          if (manager.getNotificationChannel(CHANNEL_ID)?.importance ==
              NotificationManager.IMPORTANCE_NONE) {
            invoke.resolve(receipt(false, "channel_disabled"))
            return@runOnUiThread
          }
        }

        val id = nextNotificationId.getAndIncrement()
        val openApp = PendingIntent.getActivity(activity, id,
          Intent(activity, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
          }, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        val isFull = args.mode == "full_message"
        val title = if (isFull) "Mcp Agent Ai · Reply complete"
          else "Mcp Agent Ai · Task finished"
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
          Notification.Builder(activity, CHANNEL_ID) else Notification.Builder(activity)
        val notification = builder
          .setSmallIcon(android.R.drawable.ic_dialog_info)
          // API 26+ uses channel importance; keep high priority as a fallback.
          .setPriority(Notification.PRIORITY_HIGH)
          .setContentTitle(title)
          .setContentText(body)
          .setContentIntent(openApp)
          .setCategory(Notification.CATEGORY_STATUS)
          .setVisibility(Notification.VISIBILITY_PRIVATE)
          .setAutoCancel(true)
          .setOnlyAlertOnce(false)
          .setStyle(Notification.BigTextStyle().bigText(body))
          .build()
        manager.notify(NOTIFICATION_TAG, id, notification)
        invoke.resolve(receipt(true))
      } catch (_: Exception) {
        invoke.resolve(receipt(false, "notification_unavailable"))
      }
    }
  }
}
