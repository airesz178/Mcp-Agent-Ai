package ai.mcpagent.app

import android.app.Activity
import android.app.Application
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class ForegroundRuntimeUserActionArgs {
  var userInitiated: Boolean = false
}

/**
 * As with Termux, the current in-app native session moves into a foreground
 * service when the Activity is paused. There is no extra manual Start button.
 * Notification Exit is the user's deliberate decision to end that process.
 */
@TauriPlugin
class ForegroundRuntimePlugin(private val activity: Activity) : Plugin(activity) {
  private val main = Handler(Looper.getMainLooper())
  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, state: Bundle?) {}
    override fun onActivityStarted(a: Activity) {}
    override fun onActivityResumed(a: Activity) {
      if (a !== activity) return
      ForegroundRuntimeService.setActivityVisible(true)
      if (!ForegroundRuntimeService.isRunning()) return
      // The session stays alive in the app; remove its background-only
      // notification as soon as the Activity returns to the screen.
      try {
        activity.startService(
          Intent(activity, ForegroundRuntimeService::class.java)
            .setAction(ForegroundRuntimeService.ACTION_HIDE))
      } catch (_: RuntimeException) {
        // A delayed ACTION_START also checks current Activity visibility.
      }
    }
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) {}
    override fun onActivityStopped(a: Activity) {}
    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) activity.application.unregisterActivityLifecycleCallbacks(this)
    }
    override fun onActivityPaused(a: Activity) {
      if (a !== activity) return
      ForegroundRuntimeService.setActivityVisible(false)
      if (Build.VERSION.SDK_INT < 26 ||
          !ForegroundRuntimeService.notificationEnabled(activity)) return
      // onPause is still part of the user-visible Activity transition on
      // Android 12+, allowing FGS start before the app becomes backgrounded.
      // Always enqueue START: a pending HIDE from a quick resume must not
      // leave the session unprotected after the Activity pauses again.
      try {
        activity.startForegroundService(
          Intent(activity, ForegroundRuntimeService::class.java)
            .setAction(ForegroundRuntimeService.ACTION_START))
      } catch (_: RuntimeException) {
        // No false active receipt: status() always reads the service itself.
      }
    }
  }

  init {
    ForegroundRuntimeService.setActivityVisible(true)
    activity.application.registerActivityLifecycleCallbacks(lifecycle)
  }

  // Service commands are queued on Android's main thread. Wait for the
  // actual service lifecycle receipt instead of claiming it started/stopped
  // synchronously merely because startService() accepted our Intent.
  private fun resolveWhenObserved(invoke: Invoke, running: Boolean, attempts: Int = 20) {
    if (ForegroundRuntimeService.isRunning() == running) {
      invoke.resolve(snapshot())
    } else if (attempts <= 0) {
      invoke.reject(if (running) "Android did not confirm the foreground runtime"
        else "Android did not confirm background mode stopped")
    } else {
      main.postDelayed({ resolveWhenObserved(invoke, running, attempts - 1) }, 150L)
    }
  }

  private fun snapshot(): JSObject = JSObject().apply {
    val running = ForegroundRuntimeService.isRunning()
    val visible = running && ForegroundRuntimeService.notificationEnabled(activity)
    put("supported", Build.VERSION.SDK_INT >= 26)
    put("running", running)
    put("notificationVisible", visible)
    put("stopAvailable", visible)
  }

  @Command
  fun status(invoke: Invoke) {
    invoke.resolve(snapshot())
  }

  @Command
  fun start(invoke: Invoke) {
    val approved = try {
      invoke.parseArgs(ForegroundRuntimeUserActionArgs::class.java).userInitiated
    } catch (_: Exception) { false }
    if (!approved) {
      invoke.reject("Starting background mode requires an explicit user action")
      return
    }
    if (Build.VERSION.SDK_INT < 26 || !activity.hasWindowFocus()) {
      invoke.reject("Start background mode only while Mcp Agent Ai is open")
      return
    }
    if (!ForegroundRuntimeService.notificationEnabled(activity)) {
      invoke.reject("Enable Mcp Agent Ai notifications in Android Settings before starting background mode")
      return
    }
    try {
      if (ForegroundRuntimeService.isRunning()) {
        invoke.resolve(snapshot())
      } else {
        activity.startForegroundService(Intent(activity, ForegroundRuntimeService::class.java)
          .setAction(ForegroundRuntimeService.ACTION_START))
        resolveWhenObserved(invoke, true)
      }
    } catch (_: RuntimeException) {
      invoke.reject("Android blocked the foreground runtime start")
    }
  }

  @Command
  fun stop(invoke: Invoke) {
    val approved = try {
      invoke.parseArgs(ForegroundRuntimeUserActionArgs::class.java).userInitiated
    } catch (_: Exception) { false }
    if (!approved) {
      invoke.reject("Stopping background mode requires an explicit user action")
      return
    }
    try {
      if (ForegroundRuntimeService.isRunning()) {
        activity.startService(Intent(activity, ForegroundRuntimeService::class.java)
          .setAction(ForegroundRuntimeService.ACTION_STOP))
      }
      resolveWhenObserved(invoke, false)
    } catch (_: RuntimeException) {
      invoke.reject("Android could not stop the foreground runtime")
    }
  }
}
