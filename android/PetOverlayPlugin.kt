package ai.mcpagent.app

import android.app.Activity
import android.app.AlertDialog
import android.app.AppOpsManager
import android.app.Application
import android.content.Context
import android.graphics.Color
import android.graphics.PixelFormat
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class PetOverlayActionArgs {
  var userInitiated: Boolean = false
}

/**
 * Opt-in, app-scoped Pocket Bot. This passive 56dp touch-through View belongs
 * to the Activity only. No other-app inspection, notification, background
 * service, auto-start, saved activation state, or screen capture.
 */
@TauriPlugin
class PetOverlayPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    internal const val SIZE_DP = 56
    internal const val MIN_SDK = Build.VERSION_CODES.O
    internal const val OVERLAY_FLAGS =
      WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
      WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE

    internal fun safeFlags(flags: Int): Boolean =
      flags and OVERLAY_FLAGS == OVERLAY_FLAGS &&
      flags and WindowManager.LayoutParams.FLAG_WATCH_OUTSIDE_TOUCH == 0
  }

  private val main = Handler(Looper.getMainLooper())
  private val manager by lazy {
    activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
  }
  private val appOps by lazy {
    activity.getSystemService(Context.APP_OPS_SERVICE) as AppOpsManager
  }
  private var overlay: View? = null
  private var pendingShow: Invoke? = null
  private var confirmation: AlertDialog? = null
  private var watchingRevocation = false
  private var destroyed = false

  private fun supported() = Build.VERSION.SDK_INT >= MIN_SDK
  private fun granted() = supported() && Settings.canDrawOverlays(activity)
  private fun foreground() = !destroyed && !activity.isFinishing &&
    !activity.isDestroyed && activity.hasWindowFocus()

  private val permissionObserver = AppOpsManager.OnOpChangedListener { operation, packageName ->
    if (operation == AppOpsManager.OPSTR_SYSTEM_ALERT_WINDOW &&
      (packageName == null || packageName == activity.packageName)) {
      main.post {
        if (!granted()) {
          cancelPending("Pocket Bot permission was revoked")
          removeOverlay()
        }
      }
    }
  }

  private fun watchRevocation() {
    if (watchingRevocation) return
    appOps.startWatchingMode(
      AppOpsManager.OPSTR_SYSTEM_ALERT_WINDOW, activity.packageName, permissionObserver
    )
    watchingRevocation = true
  }

  private fun stopWatching() {
    if (!watchingRevocation) return
    watchingRevocation = false
    runCatching { appOps.stopWatchingMode(permissionObserver) }
  }

  private fun removeOverlay() {
    val old = overlay
    if (old == null) { stopWatching(); return }
    // Only remove the exact View owned by this plugin. Android may defer a
    // removeView() fallback; do not claim success while it is still attached.
    try {
      manager.removeViewImmediate(old)
    } catch (_: Exception) {
      runCatching { manager.removeView(old) }
    }
    if (old.parent == null && !old.isAttachedToWindow) {
      overlay = null
      stopWatching()
    }
  }

  private fun visible(): Boolean {
    val view = overlay ?: return false
    // Check actual Android attachment, independent of what we hope grant or
    // foreground state to be. The Rust bridge rejects inconsistent receipts.
    return view.isAttachedToWindow && view.isShown
  }

  private fun reconcile() {
    if (!granted() || destroyed || !foreground()) removeOverlay()
  }

  private fun reply(): JSObject = JSObject().apply {
    put("supported", supported())
    put("permissionGranted", granted())
    put("visible", visible())
    put("foreground", foreground())
    put("pendingConsent", pendingShow != null)
    put("sizeDp", SIZE_DP)
    put("mode", "app-scoped")
  }

  private fun completed(invoke: Invoke, shown: Boolean, cancelled: Boolean) {
    if (pendingShow !== invoke) return
    pendingShow = null
    confirmation = null
    // Never turn a lost window, revoked grant, or late lifecycle transition
    // into a nominally successful show with shown:false/cancelled:false.
    if (shown && (!visible() || !granted() || !foreground())) {
      removeOverlay()
      invoke.reject("Pocket Bot show failed: overlay is not visible or permitted")
      return
    }
    if (cancelled) {
      removeOverlay()
      if (overlay != null) {
        invoke.reject("Pocket Bot cancellation could not confirm overlay removal")
        return
      }
    }
    val state = reply()
    state.put("shown", shown)
    state.put("cancelled", cancelled)
    invoke.resolve(state)
  }

  private fun cancelPending(reason: String) {
    val request = pendingShow ?: return
    pendingShow = null
    val current = confirmation
    confirmation = null
    runCatching {
      current?.setOnDismissListener(null)
      current?.dismiss()
    }
    request.reject(reason)
  }

  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, savedInstanceState: Bundle?) = Unit
    override fun onActivityStarted(a: Activity) = Unit
    override fun onActivityResumed(a: Activity) = Unit
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) = Unit

    override fun onActivityPaused(a: Activity) {
      if (a === activity) {
        cancelPending("Pocket Bot cancelled when app left foreground")
        removeOverlay()
      }
    }

    override fun onActivityStopped(a: Activity) {
      if (a === activity) {
        cancelPending("Pocket Bot stopped with app")
        removeOverlay()
      }
    }

    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        destroyed = true
        cancelPending("Pocket Bot Activity destroyed")
        removeOverlay()
        // Framework window removal is normally synchronous. If Android
        // deferred it after a removeView fallback, retry on the next UI pass
        // before discarding this Activity (never a persistent polling loop).
        if (overlay != null) main.post { removeOverlay() }
        a.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }

  init { activity.application.registerActivityLifecycleCallbacks(lifecycle) }

  private fun parse(invoke: Invoke): PetOverlayActionArgs? = try {
    invoke.parseArgs(PetOverlayActionArgs::class.java)
  } catch (_: Exception) {
    invoke.reject("Invalid Pocket Bot request")
    null
  }

  private fun requireUser(invoke: Invoke): Boolean {
    val args = parse(invoke) ?: return false
    if (!args.userInitiated) {
      invoke.reject("Pocket Bot requires explicit user action")
      return false
    }
    return true
  }

  private fun onMain(invoke: Invoke, block: () -> Unit) {
    main.post {
      try { block() } catch (_: Exception) {
        val sameRequestPending = pendingShow === invoke
        if (sameRequestPending) cancelPending("Pocket Bot overlay operation was unsuccessful")
        removeOverlay()
        if (!sameRequestPending) invoke.reject("Pocket Bot overlay is unavailable")
      }
    }
  }

  @Command
  fun status(invoke: Invoke) = onMain(invoke) {
    reconcile()
    invoke.resolve(reply())
  }

  private fun createBot(): View =
    TextView(activity).apply {
      text = "✦\n●"
      textSize = 18f
      setTextColor(Color.WHITE)
      gravity = Gravity.CENTER
      includeFontPadding = false
      background = GradientDrawable().apply {
        shape = GradientDrawable.OVAL
        setColor(Color.rgb(25, 39, 61))
        setStroke((2 * activity.resources.displayMetrics.density + 0.5f).toInt(),
          Color.rgb(95, 207, 229))
      }
      isClickable = false
      isFocusable = false
      importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
      // removeView() can detach asynchronously. Release the owned reference
      // as soon as Android actually detaches it, even when a foreground
      // transition or permission change raced with removal.
      addOnAttachStateChangeListener(object : View.OnAttachStateChangeListener {
        override fun onViewAttachedToWindow(view: View) = Unit
        override fun onViewDetachedFromWindow(view: View) {
          if (this@PetOverlayPlugin.overlay === view) {
            this@PetOverlayPlugin.overlay = null
            stopWatching()
          }
        }
      })
    }

  private fun attachAfterConsent(request: Invoke) {
    if (pendingShow !== request) return
    if (!foreground() || !granted() || overlay != null) {
      cancelPending("Pocket Bot requires foreground app and overlay grant")
      return
    }
    val density = activity.resources.displayMetrics.density
    val size = (SIZE_DP * density + 0.5f).toInt()
    val margin = (12 * density + 0.5f).toInt()
    val params = WindowManager.LayoutParams(
      size, size, WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY, OVERLAY_FLAGS,
      PixelFormat.TRANSLUCENT
    ).apply {
      gravity = Gravity.TOP or Gravity.END
      x = margin
      y = margin + (72 * density + 0.5f).toInt()
      // Android 12+ rejects touch-through windows that obscure too much of the
      // underlying app. Keep the whole system overlay below 0.8 opacity.
      alpha = 0.65f
    }
    if (!safeFlags(params.flags)) {
      cancelPending("Unsafe Pocket Bot window flags")
      return
    }
    val view = createBot()
    try {
      watchRevocation()
      overlay = view
      manager.addView(view, params)
    } catch (_: Exception) {
      removeOverlay()
      cancelPending("Android refused Pocket Bot overlay")
      return
    }
    // One bounded confirmation after attach, never a recurring monitor.
    main.postDelayed({
      if (pendingShow !== request) return@postDelayed
      reconcile()
      if (!visible()) {
        removeOverlay()
        cancelPending("Pocket Bot was not attached or app left foreground")
      } else {
        completed(request, shown = true, cancelled = false)
      }
    }, 120L)
  }

  @Command
  fun show(invoke: Invoke) {
    if (!requireUser(invoke)) return
    onMain(invoke) {
      reconcile()
      if (!supported() || !granted() || !foreground()) {
        invoke.reject("Pocket Bot requires Android 8+, overlay grant and foreground app")
        return@onMain
      }
      if (pendingShow != null || overlay != null) {
        invoke.reject("Pocket Bot is already visible or awaiting consent")
        return@onMain
      }
      pendingShow = invoke
      var approved = false
      val prompt = AlertDialog.Builder(activity)
        .setTitle("Show Pocket Bot once?")
        .setMessage("Display one 56dp pass-through bot while this app is foreground? It cannot inspect other apps, receive touches, or remain after this app stops.")
        .setPositiveButton("Allow once") { _, _ -> approved = true }
        .setNegativeButton("Cancel") { _, _ -> }
        .create()
      confirmation = prompt
      prompt.setOnDismissListener {
        if (pendingShow !== invoke) return@setOnDismissListener
        confirmation = null
        if (!approved) completed(invoke, shown = false, cancelled = true)
        else main.postDelayed({ attachAfterConsent(invoke) }, 150L)
      }
      // A focus/lifecycle change after the first check must not leave a
      // native consent dialog queued for an app that is already background.
      if (!foreground() || !granted()) {
        cancelPending("Pocket Bot requires a foreground Activity and overlay grant")
        return@onMain
      }
      prompt.show()
    }
  }

  @Command
  fun hide(invoke: Invoke) {
    if (!requireUser(invoke)) return
    onMain(invoke) {
      reconcile()
      if (!supported() || !granted() || !foreground()) {
        invoke.reject("Pocket Bot hide requires foreground app and overlay grant")
        return@onMain
      }
      cancelPending("Pocket Bot show cancelled by user hide")
      removeOverlay()
      val state = reply()
      state.put("hidden", overlay == null && !state.optBoolean("visible"))
      invoke.resolve(state)
    }
  }
}
