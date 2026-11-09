package ai.mcpagent.app

import android.app.Activity
import android.app.Application
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.database.ContentObserver
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.app.KeyguardManager
import android.accessibilityservice.AccessibilityServiceInfo
import android.view.accessibility.AccessibilityManager
import android.os.PowerManager
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.provider.Settings
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicBoolean

@InvokeArg
class AccessibilityConsentArgs { var userInitiated: Boolean = false }

@InvokeArg
class AccessibilityAutomationStartArgs {
  var userInitiated: Boolean = false
  var sessionId: String = ""
  var targetPackage: String = ""
  var openTarget: Boolean = false
}
@InvokeArg
class AccessibilityAutomationResolveArgs { var query: String = "" }

@InvokeArg
class AccessibilityAutomationModeArgs { var selectedMode: String = "" }

@InvokeArg
class AccessibilityActionArgs {
  var approved: Boolean = false
  var sessionId: String = ""
  var targetPackage: String = ""
  var selectedMode: String = ""
  var snapshotId: String = ""
  var index: Int = 0
  var x: Int = -1
  var y: Int = -1
  var toX: Int = -1
  var toY: Int = -1
  var durationMs: Int = 0
  var kind: String = ""
  var text: String = ""
}

/** OS grant/connection and private, targeted automation bridge. All actions
 * are processed by the OS-bound AccessibilityService for an active target and
 * require a separate approved call from the trusted Rust orchestrator. */
@TauriPlugin
class AccessibilityToolsPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    // System API action (not an SDK-public Settings constant). Some Android
    // builds permit the target service page; Android 11 realme guards it with
    // OPEN_ACCESSIBILITY_DETAILS_SETTINGS, so SecurityException is expected.
    private const val DETAILS_SETTINGS_ACTION = "android.settings.ACCESSIBILITY_DETAILS_SETTINGS"
    // Keep checking while the Activity is foreground, rather than stopping
    // after 16 ticks (~2 minutes). The accessibility process may be rebound
    // by Android much later. No background service, wake lock, OS consent
    // change or attempt to manually bind a system-owned service is made.
    private const val WATCH_RECOVERING_MS = 8_000L
    private const val WATCH_CONNECTED_MS = 30_000L
    private const val WATCH_USER_ACTION_MS = 30_000L
    private const val MIN_PROBE_GAP_MS = 5_000L
    // Android frequently rebinds a granted service after a short process
    // interruption. Do not show Reconnect for that normal transient interval.
    private const val REBIND_GRACE_MS = 45_000L
  }

  private val statusWorker = ThreadPoolExecutor(
    1, 1, 0L, TimeUnit.MILLISECONDS, ArrayBlockingQueue<Runnable>(2),
    { work -> Thread(work, "mcp-accessibility-status").apply { isDaemon = true } },
    ThreadPoolExecutor.AbortPolicy()
  )

  private data class LiveStatus(
    val supported: Boolean,
    val enabled: Boolean,
    val connected: Boolean,
    val needsReconnect: Boolean
  )
  private val main = Handler(Looper.getMainLooper())
  private val watchdogInFlight = AtomicBoolean(false)
  @Volatile private var lastObservedAtElapsedMs = 0L
  @Volatile private var lastObserved: LiveStatus? = null
  @Volatile private var foreground = false
  @Volatile private var destroyed = false
  @Volatile private var unboundSinceElapsedMs = 0L
  private var observerRegistered = false

  private val osSettingsObserver = object : ContentObserver(main) {
    override fun onChange(selfChange: Boolean) {
      // A platform grant/switch change is a reason to recheck, not proof
      // that an OS binding exists. The separate service owns that proof.
      unboundSinceElapsedMs = 0L
      if (foreground) armForegroundWatch()
    }
  }

  private val watchTick = object : Runnable {
    override fun run() {
      if (!foreground || destroyed) return
      // The normal status() request is itself a live read. A watchdog probe
      // must not queue behind it or repeatedly start an unbound provider.
      val elapsed = SystemClock.elapsedRealtime()
      if (elapsed - lastObservedAtElapsedMs >= MIN_PROBE_GAP_MS &&
          statusWorker.activeCount == 0 && statusWorker.queue.isEmpty() &&
          watchdogInFlight.compareAndSet(false, true)) {
        try {
          statusWorker.execute {
            try {
              // The Activity may be paused before a queued probe can start.
              if (foreground && !destroyed) observeLiveStatus()
            } catch (_: Exception) {
              lastObserved = null // Failure is unknown, never a grant.
            } finally {
              watchdogInFlight.set(false)
            }
          }
        } catch (_: Exception) {
          watchdogInFlight.set(false)
        }
      }
      if (foreground && !destroyed) {
        // A connected service needs less polling. Recovery is watched more
        // closely, and a system-blocked service is not hammered continuously.
        val previous = lastObserved
        val delay = when {
          previous?.connected == true -> WATCH_CONNECTED_MS
          previous?.needsReconnect == true -> WATCH_USER_ACTION_MS
          else -> WATCH_RECOVERING_MS
        }
        main.postDelayed(this, delay)
      }
    }
  }

  private fun armForegroundWatch() {
    if (!foreground || destroyed) return
    main.removeCallbacks(watchTick)
    main.post(watchTick)
  }

  private fun startForegroundWatch() {
    if (destroyed || foreground) return
    foreground = true
    try {
      activity.contentResolver.registerContentObserver(
        Settings.Secure.getUriFor(Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES),
        false, osSettingsObserver)
      activity.contentResolver.registerContentObserver(
        Settings.Secure.getUriFor(Settings.Secure.ACCESSIBILITY_ENABLED),
        false, osSettingsObserver)
      observerRegistered = true
    } catch (_: Exception) {
      // The periodic checks are still available on OEMs denying observation.
      runCatching { activity.contentResolver.unregisterContentObserver(osSettingsObserver) }
    }
    armForegroundWatch()
  }

  private fun stopForegroundWatch() {
    foreground = false
    main.removeCallbacks(watchTick)
    if (observerRegistered) {
      runCatching { activity.contentResolver.unregisterContentObserver(osSettingsObserver) }
      observerRegistered = false
    }
    lastObserved = null
    lastObservedAtElapsedMs = 0L
  }

  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, state: Bundle?) = Unit
    override fun onActivityStarted(a: Activity) = Unit
    override fun onActivityResumed(a: Activity) {
      if (a === activity) startForegroundWatch()
    }
    override fun onActivityPaused(a: Activity) {
      if (a === activity) stopForegroundWatch()
    }
    override fun onActivityStopped(a: Activity) {
      if (a === activity) stopForegroundWatch()
    }
    override fun onActivitySaveInstanceState(a: Activity, outState: Bundle) = Unit
    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        destroyed = true
        stopForegroundWatch()
        statusWorker.shutdownNow()
        a.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }

  init {
    activity.application.registerActivityLifecycleCallbacks(lifecycle)
    // Tauri may register a plugin after the Activity has already resumed.
    // Start supervision for that case instead of waiting for another
    // foreground transition. Window focus is read on the main thread.
    main.post {
      if (!destroyed && activity.hasWindowFocus()) startForegroundWatch()
    }
  }

  private fun serviceComponent(): ComponentName =
    ComponentName(activity.packageName, AgentAccessibilityService::class.java.name)

  private fun enabledBySystem(): Boolean {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return false
    val expected = serviceComponent()
    // Some OEMs return an empty Secure allowlist after reopening an Activity,
    // even though AccessibilityManager still enumerates the enabled service.
    // Use both live OS sources, never a saved in-app "Allowed" preference.
    val secureMatch = try {
      Settings.Secure.getString(activity.contentResolver,
        Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES)
        .orEmpty().split(':').any {
          ComponentName.unflattenFromString(it.trim()) == expected
        }
    } catch (_: Exception) { false }
    if (secureMatch) return true
    return try {
      val manager = activity.getSystemService(Context.ACCESSIBILITY_SERVICE) as? AccessibilityManager
      manager?.getEnabledAccessibilityServiceList(
        AccessibilityServiceInfo.FEEDBACK_ALL_MASK
      )?.any { service ->
        service.resolveInfo?.serviceInfo?.let { info ->
          ComponentName(info.packageName, info.name) == expected
        } == true
      } == true
    } catch (_: Exception) { false }
  }

  private fun connectedByServiceProcess(): Boolean = try {
    // The private health provider belongs to the accessibility process.
    // Only its current onServiceConnected state is proof of a live binding.
    val uri = Uri.parse("content://${activity.packageName}.accessibilityhealth")
    val response = activity.contentResolver.call(uri, "status", null, null)
    val since = response?.getLong("connectedSinceElapsedMs", 0L) ?: 0L
    Log.i("McpAutoIndicator", "health_probe present=${response != null} connected=${response?.getBoolean("connected", false)} sinceValid=${since > 0 && since <= SystemClock.elapsedRealtime()}")
    response?.getBoolean("connected", false) == true &&
      since > 0 && since <= SystemClock.elapsedRealtime()
  } catch (error: Exception) {
    Log.w("McpAutoIndicator", "health_probe_failed class=${error.javaClass.simpleName}")
    false
  }

  private fun accessibilityGloballyDisabled(): Boolean = try {
    Settings.Secure.getInt(activity.contentResolver,
      Settings.Secure.ACCESSIBILITY_ENABLED, -1) == 0
  } catch (_: Exception) { false }

  private fun observeLiveStatus(): LiveStatus {
    val supported = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
    val enabled = supported && enabledBySystem()
    val globallyDisabled = enabled && accessibilityGloballyDisabled()
    // Querying an unbound health provider can start its isolated process.
    // When Android's master switch is off, there cannot be a live OS bind:
    // avoid waking the provider and do not turn this into false liveness.
    val connected = enabled && !globallyDisabled && connectedByServiceProcess()
    val now = SystemClock.elapsedRealtime().coerceAtLeast(1L)
    val needsReconnect = if (!enabled || connected) {
      unboundSinceElapsedMs = 0L
      false
    } else if (globallyDisabled) {
      // A real OS-disabled/crashed service cannot be started by a normal app.
      unboundSinceElapsedMs = now
      true
    } else {
      if (unboundSinceElapsedMs <= 0L || unboundSinceElapsedMs > now)
        unboundSinceElapsedMs = now
      // Give the system a bounded opportunity to rebind automatically. The
      // next live status clears this as soon as onServiceConnected fires.
      now - unboundSinceElapsedMs >= REBIND_GRACE_MS
    }
    val status = LiveStatus(
      supported = supported,
      enabled = enabled,
      connected = connected,
      needsReconnect = needsReconnect
    )
    lastObserved = status
    lastObservedAtElapsedMs = SystemClock.elapsedRealtime()
    return status
  }

  private fun launchSettings(): String {
    val component = serviceComponent().flattenToString()
    try {
      // EXTRA_COMPONENT_NAME is a String containing the flattened service
      // component, not a Parcelable ComponentName. Only report this target
      // if Android accepted the startActivity request.
      activity.startActivity(Intent(DETAILS_SETTINGS_ACTION)
        .putExtra(Intent.EXTRA_COMPONENT_NAME, component))
      return "service_details"
    } catch (_: ActivityNotFoundException) {
      // OEM does not provide an activity for the system-only details action.
    } catch (_: SecurityException) {
      // Android 11 realme refuses this with OPEN_ACCESSIBILITY_DETAILS_SETTINGS.
    } catch (_: IllegalArgumentException) {
      // Some OEM Settings packages reject unexpected detail intent extras.
    }
    // Public, supported Settings action. The component is a harmless optional
    // hint for OEMs; standard Android may show only the accessibility list.
    activity.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
      .putExtra(Intent.EXTRA_COMPONENT_NAME, component))
    return "accessibility_settings"
  }

  private fun run(invoke: Invoke, block: () -> JSObject) {
    activity.runOnUiThread {
      try { invoke.resolve(block()) }
      catch (_: Exception) { invoke.reject("Android Accessibility status or Settings unavailable") }
    }
  }

  @Command
  fun status(invoke: Invoke) {
    try {
      statusWorker.execute {
        try {
          // Always use a fresh native read, never a cached watchdog value:
          // the OS grant or service binding may have changed since polling.
          val status = observeLiveStatus()
          invoke.resolve(JSObject().apply {
            put("supported", status.supported)
            put("enabled", status.enabled)
            put("connected", status.connected)
            put("needsReconnect", status.needsReconnect)
          })
        } catch (_: Exception) {
          invoke.reject("Android Accessibility status unavailable")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android Accessibility status is busy")
    }
  }

  @Command
  fun requestSettings(invoke: Invoke) {
    val args = try { invoke.parseArgs(AccessibilityConsentArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid Accessibility settings request"); return }
    if (!args.userInitiated || Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
      invoke.reject("Explicit user action and Android 8+ required")
      return
    }
    run(invoke) {
      val destination = launchSettings()
      JSObject().apply {
        put("opened", true)
        put("destination", destination)
        put("enabled", enabledBySystem())
      }
    }
  }

  /** Calls the non-exported provider in :accessibility. This acknowledges an
   * actual bound service, never a presumed UI state in the Tauri process. */
  private fun automationCall(method: String, arguments: Bundle? = null): Bundle? =
    try {
      val uri = Uri.parse("content://${activity.packageName}.accessibilityhealth")
      activity.contentResolver.call(uri, method, null, arguments)
    } catch (_: RuntimeException) { null }

  private fun automationReceipt(
    system: LiveStatus, response: Bundle?, missingReason: String
  ): JSObject = JSObject().apply {
    val connected = system.connected
    val active = connected && response?.getBoolean("active", false) == true
    put("supported", system.supported)
    put("enabled", system.enabled)
    put("connected", connected)
    put("needsReconnect", system.needsReconnect)
    put("active", active)
    put("visible", active && response?.getBoolean("visible", false) == true)
    // Private trusted-host readiness; the public MCP status is sanitized by
    // Rust. Overlay visibility alone never attests foreground control.
    put("screenUnlocked", connected && response?.getBoolean("screenUnlocked", false) == true)
    put("targetFocused", active && response?.getBoolean("targetFocused", false) == true &&
      response?.getBoolean("screenUnlocked", false) == true)
    put("stopRequested", response?.getBoolean("stopRequested", false) == true)
    put("windowEventsSeen", response?.getInt("windowEventsSeen", 0) ?: 0)
    put("targetEventsSeen", response?.getInt("targetEventsSeen", 0) ?: 0)
    response?.getString("sessionId")?.let { put("sessionId", it) }
    response?.getString("targetPackage")?.let { put("targetPackage", it) }
    response?.getString("stoppedSessionId")?.let { put("stoppedSessionId", it) }
    val reason = response?.getString("reason") ?: when {
      !connected -> "accessibility_service_not_connected"
      response == null -> missingReason
      else -> null
    }
    if (reason != null) put("reason", reason)
  }

  /** True Android runtime capabilities from the currently OS-bound service;
   * enabled/connected is NOT by itself a screenshot or gesture permission. */
  private fun automationModeReceipt(system: LiveStatus, selected: String): JSObject {
    val capability = if (system.connected) automationCall("automationCapabilities") else null
    val semantic = system.connected && capability?.getBoolean("nodeContent", false) == true
    val gesture = system.connected && capability?.getBoolean("gesture", false) == true
    val screenshot = system.connected && capability?.getBoolean("screenshot", false) == true
    val screenshotClick = screenshot && gesture
    val sessionReady = capability?.run {
      getBoolean("sessionActive", false) &&
        getBoolean("targetVisible", false) &&
        getBoolean("screenUnlocked", false)
    } == true
    val mode = when {
      !sessionReady -> "unavailable"
      selected == "realtime" && semantic -> "realtime"
      screenshotClick -> "screenshot_click"
      else -> "unavailable"
    }
    val reason = when {
      !system.supported -> "android_unsupported"
      !system.enabled -> "accessibility_permission_missing"
      !system.connected -> "accessibility_service_disconnected"
      capability == null -> "capability_probe_failed"
      !sessionReady -> "target_session_required"
      mode == "realtime" -> "semantic_ready"
      selected == "realtime" && mode == "screenshot_click" -> "semantic_fallback_to_screenshot_click"
      mode == "screenshot_click" -> "screenshot_click_ready"
      selected == "realtime" -> "semantic_action_and_screenshot_tap_unavailable"
      else -> "screenshot_tap_unavailable"
    }
    return JSObject().apply {
      put("selectedMode", selected)
      put("effectiveMode", mode)
      put("fallbackUsed", selected == "realtime" && mode == "screenshot_click")
      put("reason", reason)
      put("supported", system.supported)
      put("enabled", system.enabled)
      put("connected", system.connected)
      put("needsReconnect", system.needsReconnect)
      put("semanticActionAvailable", semantic)
      put("screenCaptureAvailable", screenshot)
      put("tapAvailable", gesture)
      put("realtimeAvailable", semantic)
      put("screenshotClickAvailable", screenshotClick)
    }
  }

  private fun automationWork(invoke: Invoke, action: (LiveStatus) -> JSObject) {
    try {
      statusWorker.execute {
        try { invoke.resolve(action(observeLiveStatus())) }
        catch (_: Exception) { invoke.reject("Android app automation indicator unavailable") }
      }
    } catch (_: Exception) {
      invoke.reject("Android app automation indicator is busy")
    }
  }

  /** Only opens the explicit, user-approved installed target app. The private
   * AccessibilityService has already armed the matching indicator session.
   * No background-launch loophole, exported component guessing or shell. */
  private fun launchAutomationTarget(targetPackage: String): Boolean {
    val intent = activity.packageManager.getLaunchIntentForPackage(targetPackage)
      ?: return false
    val packageName = intent.component?.packageName ?: intent.`package`
    if (packageName != targetPackage) return false
    val finished = CountDownLatch(1)
    val pending = AtomicBoolean(true)
    val launched = AtomicBoolean(false)
    val runnable = Runnable {
      try {
        if (pending.compareAndSet(true, false) && !activity.isFinishing &&
            !activity.isDestroyed && activity.hasWindowFocus()) {
          intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
          activity.startActivity(intent)
          launched.set(true)
        }
      } catch (_: Exception) {
        launched.set(false)
      } finally { finished.countDown() }
    }
    if (!main.post(runnable)) return false
    if (!finished.await(2, TimeUnit.SECONDS)) {
      pending.compareAndSet(true, false)
      main.removeCallbacks(runnable)
      return false
    }
    return launched.get()
  }

  /** Resolve one user-written installed app label to its actual launcher
   * package, without hard-coding examples or exposing the installed app list.
   * Ambiguous and system-app matches fail closed. */
  @Command
  fun automationResolveTarget(invoke: Invoke) {
    val args = try { invoke.parseArgs(AccessibilityAutomationResolveArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid app name"); return }
    val query = args.query.trim()
    if (query.length !in 2..80 || query.any { Character.isISOControl(it) }) {
      invoke.reject("Enter one app name, at most 80 characters")
      return
    }
    try {
      statusWorker.execute {
        try {
          val pm = activity.packageManager
          val intent = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
          val matches = pm.queryIntentActivities(intent, 0)
            .asSequence()
            .filter { it.activityInfo?.packageName != null }
            .mapNotNull {
              val pkg = it.activityInfo.packageName
              val label = it.loadLabel(pm)?.toString()?.trim().orEmpty()
              if (label.isEmpty() || pkg == "ai.mcpagent.app" || pkg.startsWith("com.android.") ||
                  pkg.startsWith("com.google.android.permissioncontroller") ||
                  pkg.split('.').size < 2) null else pkg to label
            }
            .filter { (pkg, label) -> pkg.equals(query, true) || label.equals(query, true) }
            .distinctBy { it.first }
            .take(2).toList()
          invoke.resolve(JSObject().apply {
            put("resolved", matches.size == 1)
            if (matches.size == 1) {
              put("targetPackage", matches[0].first)
              put("displayName", matches[0].second.take(80))
            } else put("reason", if (matches.isEmpty()) "app_not_found" else "app_name_ambiguous")
          })
        } catch (_: Exception) { invoke.reject("Unable to resolve installed Android app") }
      }
    } catch (_: Exception) { invoke.reject("App name lookup is busy") }
  }

  @Command
  fun automationStatus(invoke: Invoke) {
    automationWork(invoke) { status ->
      automationReceipt(status,
        if (status.connected) automationCall("automationStatus") else null,
        "automation_status_unavailable")
    }
  }

  /** OS-derived mode and fallback readiness for the user's saved choice.
   * No probe changes that saved choice or performs an action. */
  @Command
  fun automationModeStatus(invoke: Invoke) {
    val selected = try { invoke.parseArgs(AccessibilityAutomationModeArgs::class.java).selectedMode }
      catch (_: Exception) { invoke.reject("Invalid app automation mode request"); return }
    if (selected != "realtime" && selected != "screenshot_click") {
      invoke.reject("Unknown app automation mode")
      return
    }
    automationWork(invoke) { system -> automationModeReceipt(system, selected) }
  }

  /** Trusted Rust sends approved=true only after the *exact call* passed its
   * per-call decision gate; no action command is exported as Tauri JS invoke. */
  private fun actionArguments(args: AccessibilityActionArgs): Bundle? {
    val pkg = args.targetPackage
    val session = args.sessionId
    if (!args.approved || session.length !in 8..128 ||
        !session.all { it.isLetterOrDigit() || it == '-' || it == '_' } ||
        pkg.length !in 3..200 || pkg == activity.packageName ||
        pkg == "android" || pkg.startsWith("com.android.") ||
        pkg.startsWith("com.google.android.permissioncontroller") ||
        !Regex("^[a-zA-Z_][a-zA-Z0-9_]*(\\.[a-zA-Z_][a-zA-Z0-9_]*)+$").matches(pkg))
      return null
    return Bundle().apply {
      putBoolean("approved", true)
      putString("sessionId", session)
      putString("targetPackage", pkg)
      putString("selectedMode", args.selectedMode)
      putString("snapshotId", args.snapshotId)
      putInt("index", args.index)
      putInt("x", args.x)
      putInt("y", args.y)
      putInt("toX", args.toX)
      putInt("toY", args.toY)
      putInt("durationMs", args.durationMs)
      putString("kind", args.kind)
      putString("text", args.text)
    }
  }

  private fun actionReceipt(receipt: Bundle?): JSObject = JSObject().apply {
    val safe = receipt?.takeIf { it.getBoolean("ok", false) }
    put("ok", safe != null)
    if (safe == null) {
      put("reason", receipt?.getString("reason")?.take(90) ?: "service_unavailable")
      return@apply
    }
    for (key in listOf("selectedMode", "effectiveMode", "snapshotId", "mimeType")) {
      safe.getString(key)?.let { put(key, it) }
    }
    for (key in listOf("fallbackUsed", "dispatched", "completed")) {
      if (safe.containsKey(key)) put(key, safe.getBoolean(key))
    }
    for (key in listOf("screenWidth", "screenHeight", "imageWidth",
                       "imageHeight", "imageBytes")) {
      if (safe.containsKey(key)) put(key, safe.getInt(key))
    }
    val image = safe.getString("imageBase64")
    if (image != null && safe.getInt("imageBytes", 0) in 1..(128 * 1024) &&
        image.length <= ((128 * 1024 + 2) / 3) * 4 &&
        image.all { it.isLetterOrDigit() || it == '+' || it == '/' || it == '=' })
      put("imageBase64", image)
    val elements = safe.getParcelableArrayList<Bundle>("elements")
    if (elements != null) {
      val output = JSONArray()
      for (node in elements.take(48)) {
        output.put(JSONObject().apply {
          put("index", node.getInt("index"))
          put("className", node.getString("className").orEmpty().take(96))
          put("text", node.getString("text").orEmpty().take(96))
          put("description", node.getString("description").orEmpty().take(96))
          put("clickable", node.getBoolean("clickable"))
          put("editable", node.getBoolean("editable"))
          put("protected", node.getBoolean("protected"))
          put("left", node.getInt("left")); put("top", node.getInt("top"))
          put("right", node.getInt("right")); put("bottom", node.getInt("bottom"))
        })
      }
      put("elements", output)
    }
  }

  private fun action(invoke: Invoke, command: String) {
    val args = try { invoke.parseArgs(AccessibilityActionArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid app automation action"); return }
    val bundle = actionArguments(args)
    if (bundle == null) { invoke.reject("Approved targeted session required"); return }
    if (command != "automationFill" && args.selectedMode !in listOf("realtime", "screenshot_click")) {
      invoke.reject("Unknown automation mode"); return
    }
    if (command == "automationFill" &&
        (args.text.length !in 1..512 || args.text.any { it.code == 0 || Character.isISOControl(it) })) {
      invoke.reject("Invalid input text"); return
    }
    automationWork(invoke) { status ->
      actionReceipt(if (status.connected) automationCall(command, bundle) else null)
    }
  }

  @Command fun automationSnapshot(invoke: Invoke) = action(invoke, "automationSnapshot")
  @Command fun automationClick(invoke: Invoke) = action(invoke, "automationClick")
  @Command fun automationFill(invoke: Invoke) = action(invoke, "automationFill")
  @Command fun automationGesture(invoke: Invoke) = action(invoke, "automationGesture")

  /** Internal trusted-host continuation after the user approves an exact
   * screen action in Mcp Agent Ai. Opening the approval UI hides the target
   * Activity, so merely checking 'visible' would make every action fail.
   * This NEVER arms a new session, changes the approved target, or grants
   * permission. It only returns to the SAME still-active third-party app,
   * then waits for the OS-bound service to attest its foreground window. */
  @Command
  fun automationResume(invoke: Invoke) {
    val args = try { invoke.parseArgs(AccessibilityActionArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid approved app resume request"); return }
    if (!args.approved || args.sessionId.length !in 8..128 ||
        args.sessionId.any { !(it.isLetterOrDigit() || it == '-' || it == '_') } ||
        args.targetPackage.length !in 3..160 ||
        !Regex("^[A-Za-z_][A-Za-z0-9_]*(\\.[A-Za-z_][A-Za-z0-9_]*)+$").matches(args.targetPackage) ||
        args.targetPackage == "ai.mcpagent.app" || args.targetPackage.startsWith("com.android.")) {
      invoke.reject("Approved session and exact third-party target required")
      return
    }
    automationWork(invoke) { system ->
      val before = if (system.connected) automationCall("automationStatus") else null
      val matching = before?.getBoolean("active", false) == true &&
        before.getBoolean("stopRequested", true) == false &&
        before.getBoolean("screenUnlocked", false) &&
        before.getString("sessionId") == args.sessionId &&
        before.getString("targetPackage") == args.targetPackage
      val power = activity.getSystemService(Context.POWER_SERVICE) as? PowerManager
      val keyguard = activity.getSystemService(Context.KEYGUARD_SERVICE) as? KeyguardManager
      val unlocked = power?.isInteractive == true && keyguard != null &&
        !keyguard.isKeyguardLocked && !keyguard.isDeviceLocked
      if (!matching || !unlocked) {
        return@automationWork JSObject().apply {
          put("resumed", false)
          put("reason", if (!matching) "approved_session_changed" else "device_locked")
        }
      }
      var latest = before
      if (before?.getBoolean("visible", false) != true ||
          before?.getBoolean("targetFocused", false) != true) {
        // The approval decision may unblock the tunnel just before the
        // Mcp Agent Ai dialog loses focus. Allow a short bounded handoff;
        // never launch from a locked screen or a hidden Activity.
        var launched = false
        for (attempt in 0 until 8) {
          if (activity.hasWindowFocus() && power?.isInteractive == true &&
              keyguard != null && !keyguard.isKeyguardLocked && !keyguard.isDeviceLocked) {
            launched = launchAutomationTarget(args.targetPackage)
            if (launched) break
          }
          try { Thread.sleep(150) } catch (_: InterruptedException) { break }
        }
        if (!launched) {
          return@automationWork JSObject().apply {
            put("resumed", false)
            put("reason", "approved_target_launch_failed")
          }
        }
        for (attempt in 0 until 10) {
          try { Thread.sleep(180) } catch (_: InterruptedException) { break }
          val observed = automationCall("automationStatus") ?: break
          latest = observed
          if (observed.getString("sessionId") != args.sessionId ||
              observed.getString("targetPackage") != args.targetPackage ||
              !observed.getBoolean("active", false) ||
              observed.getBoolean("stopRequested", true)) break
          if (observed.getBoolean("visible", false) &&
              observed.getBoolean("targetFocused", false) &&
              observed.getBoolean("screenUnlocked", false)) break
        }
      }
      val verified = latest != null && latest.getBoolean("active", false) &&
        latest.getBoolean("visible", false) &&
        latest.getBoolean("targetFocused", false) &&
        latest.getBoolean("screenUnlocked", false) &&
        !latest.getBoolean("stopRequested", true) &&
        latest.getString("sessionId") == args.sessionId &&
        latest.getString("targetPackage") == args.targetPackage
      JSObject().apply {
        put("resumed", verified)
        put("active", verified)
        put("visible", verified)
        put("connected", verified && system.connected)
        put("stopRequested", latest?.getBoolean("stopRequested", true) ?: true)
        if (verified) {
          put("sessionId", args.sessionId)
          put("targetPackage", args.targetPackage)
        }
        if (!verified) put("reason", "approved_target_not_visible")
      }
    }
  }

  @Command
  fun automationStart(invoke: Invoke) {
    val args = try { invoke.parseArgs(AccessibilityAutomationStartArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid app automation request"); return }
    if (!args.userInitiated || args.sessionId.length !in 8..128 ||
        args.targetPackage.length !in 3..200) {
      invoke.reject("Explicit user action, session ID and target package required")
      return
    }
    automationWork(invoke) { status ->
      val response = if (status.connected) automationCall("automationStart", Bundle().apply {
        putString("sessionId", args.sessionId)
        putString("targetPackage", args.targetPackage)
      }) else null
      val armed = response?.getBoolean("active", false) == true
      if (args.openTarget && armed) {
        val launched = launchAutomationTarget(args.targetPackage)
        if (!launched) {
          // Never leave an invisible automation session armed if the requested
          // app could not be opened. A late launch is prevented by pending=false.
          automationCall("automationStop")
          automationReceipt(status, null, "target_app_launch_failed").apply {
            put("launched", false)
          }
        } else {
          // startActivity only acknowledges that Android accepted the Intent.
          // The blue indicator must be reported as successful ONLY after the
          // bound AccessibilityService attests the target window and overlay.
          var latest = response
          var confirmedVisible = false
          for (attempt in 0 until 14) {
            try { Thread.sleep(250) } catch (_: InterruptedException) { break }
            latest = automationCall("automationStatus") ?: break
            if (latest.getBoolean("active", false) &&
                latest.getString("sessionId") == args.sessionId &&
                latest.getString("targetPackage") == args.targetPackage &&
                latest.getBoolean("visible", false) &&
                latest.getBoolean("targetFocused", false) &&
                latest.getBoolean("screenUnlocked", false)) {
              confirmedVisible = true
              break
            }
            if (!latest.getBoolean("active", false)) break
          }
          if (!confirmedVisible) {
            automationCall("automationStop")
            automationReceipt(status, latest, "indicator_not_visible").apply {
              put("launched", false)
              put("reason", "indicator_not_visible")
            }
          } else automationReceipt(status, latest, "automation_start_unavailable").apply {
            put("launched", true)
          }
        }
      } else automationReceipt(status, response, "automation_start_unavailable").apply {
        put("launched", false)
      }
    }
  }

  @Command
  fun automationStop(invoke: Invoke) {
    automationWork(invoke) { status ->
      automationReceipt(status,
        if (status.connected) automationCall("automationStop") else null,
        "automation_stop_unavailable")
    }
  }
}
