package ai.mcpagent.app

import android.Manifest
import android.app.Activity
import android.app.Application
import android.app.KeyguardManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.UUID

@InvokeArg
class PluginsApprovalPendingArgs {
  var requestId: String = ""
  var expiresAtMs: Long = 0L
}

@InvokeArg
class PluginsApprovalDismissArgs {
  var requestId: String = ""
}

/**
 * A passive notification and independent OS foreground/unlocked check for
 * inbound Plugins approvals. This plugin NEVER accepts, rejects or persists
 * a decision; Rust's pending request broker owns all approval authority.
 * No request arguments, URLs or target scopes are shown on the lock screen.
 */
@TauriPlugin
class PluginsApprovalAlertPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val CHANNEL = "mcp_agent_ai_plugin_approvals_v1"
    private const val NOTIFICATION_ID = 52991
    private const val MAX_FUTURE_MS = 60_000L
    private val uuidPattern = Regex(
      "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
    )
  }

  @Volatile private var resumed = false
  @Volatile private var lifecycleObserved = false
  @Volatile private var destroyed = false
  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, savedInstanceState: Bundle?) = Unit
    override fun onActivityStarted(a: Activity) = Unit
    override fun onActivityResumed(a: Activity) {
      if (a === activity) {
        lifecycleObserved = true
        resumed = true
      }
    }
    override fun onActivityPaused(a: Activity) {
      if (a === activity) {
        lifecycleObserved = true
        resumed = false
      }
    }
    override fun onActivityStopped(a: Activity) {
      if (a === activity) {
        lifecycleObserved = true
        resumed = false
      }
    }
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) = Unit
    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        resumed = false
        lifecycleObserved = true
        destroyed = true
        a.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }

  init { activity.application.registerActivityLifecycleCallbacks(lifecycle) }

  private fun validId(id: String): Boolean =
    uuidPattern.matches(id) && try { UUID.fromString(id); true } catch (_: IllegalArgumentException) { false }

  private fun visiblyForeground(): Boolean {
    if (destroyed || activity.isFinishing || activity.isDestroyed ||
        !activity.hasWindowFocus()) return false
    // Tauri may register the plugin after its first onResume. Only before any
    // lifecycle callback do we fall back to actual window focus. Once a pause
    // or stop is observed, stale focus alone can NEVER restore authority.
    return if (lifecycleObserved) resumed else true
  }

  /** Must be called on Android's UI thread; never trust process importance. */
  private fun isForegroundUnlocked(): Boolean {
    if (!visiblyForeground()) return false
    val power = activity.getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return false
    val keyguard = activity.getSystemService(Context.KEYGUARD_SERVICE) as? KeyguardManager ?: return false
    return power.isInteractive && !keyguard.isKeyguardLocked && !keyguard.isDeviceLocked
  }

  private fun result(key: String, value: Boolean): JSObject =
    JSObject().apply { put(key, value) }

  private fun manager(): NotificationManager? =
    activity.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager

  private fun notificationsAllowed(manager: NotificationManager): Boolean =
    manager.areNotificationsEnabled() &&
      (Build.VERSION.SDK_INT < 33 ||
        activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
        PackageManager.PERMISSION_GRANTED)

  @Command
  fun foregroundUnlocked(invoke: Invoke) {
    activity.runOnUiThread {
      val allowed = try { isForegroundUnlocked() } catch (_: Exception) { false }
      invoke.resolve(result("foregroundUnlocked", allowed))
    }
  }

  @Command
  fun notifyPending(invoke: Invoke) {
    val args = try { invoke.parseArgs(PluginsApprovalPendingArgs::class.java) }
      catch (_: Exception) { invoke.resolve(result("posted", false)); return }
    if (!validId(args.requestId)) {
      invoke.resolve(result("posted", false)); return
    }
    activity.runOnUiThread {
      try {
        val now = System.currentTimeMillis()
        val remaining = if (args.expiresAtMs > now &&
            args.expiresAtMs <= now + MAX_FUTURE_MS) args.expiresAtMs - now else 0L
        if (destroyed || activity.isFinishing || activity.isDestroyed ||
            isForegroundUnlocked() ||
            remaining <= 0 || remaining > MAX_FUTURE_MS) {
          invoke.resolve(result("posted", false)); return@runOnUiThread
        }
        val notificationManager = manager()
        if (notificationManager == null || !notificationsAllowed(notificationManager)) {
          invoke.resolve(result("posted", false)); return@runOnUiThread
        }
        if (Build.VERSION.SDK_INT >= 26) {
          notificationManager.createNotificationChannel(NotificationChannel(
            CHANNEL, "ChatGPT Plugins approvals", NotificationManager.IMPORTANCE_HIGH
          ).apply {
            description = "Open Mcp Agent Ai to review pending Plugins requests"
            setShowBadge(true)
          })
          if (notificationManager.getNotificationChannel(CHANNEL)?.importance ==
              NotificationManager.IMPORTANCE_NONE) {
            invoke.resolve(result("posted", false)); return@runOnUiThread
          }
        }
        val openApp = PendingIntent.getActivity(
          activity, args.requestId.hashCode(),
          Intent(activity, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
          }, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val notification = Notification.Builder(activity, CHANNEL)
          .setSmallIcon(android.R.drawable.ic_dialog_info)
          .setContentTitle("ChatGPT Plugins approval needed")
          .setContentText("Open Mcp Agent Ai to review a pending request.")
          .setPriority(Notification.PRIORITY_HIGH)
          .setCategory(Notification.CATEGORY_STATUS)
          .setVisibility(Notification.VISIBILITY_PRIVATE)
          .setContentIntent(openApp)
          .setAutoCancel(true)
          .setOnlyAlertOnce(true)
          .setTimeoutAfter(remaining)
          .build()
        notificationManager.notify(args.requestId, NOTIFICATION_ID, notification)
        invoke.resolve(result("posted", true))
      } catch (_: Exception) {
        invoke.resolve(result("posted", false))
      }
    }
  }

  @Command
  fun dismissPending(invoke: Invoke) {
    val args = try { invoke.parseArgs(PluginsApprovalDismissArgs::class.java) }
      catch (_: Exception) { invoke.resolve(result("dismissed", false)); return }
    if (!validId(args.requestId)) {
      invoke.resolve(result("dismissed", false)); return
    }
    activity.runOnUiThread {
      val dismissed = try {
        val notificationManager = manager()
        if (notificationManager == null) false else {
          notificationManager.cancel(args.requestId, NOTIFICATION_ID)
          true
        }
      } catch (_: Exception) { false }
      invoke.resolve(result("dismissed", dismissed))
    }
  }
}
