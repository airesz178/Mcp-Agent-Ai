package ai.mcpagent.app

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.AccessibilityServiceInfo
import android.accessibilityservice.GestureDescription
import android.app.KeyguardManager
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.Path
import android.graphics.PixelFormat
import android.graphics.Point
import android.graphics.Rect
import android.graphics.drawable.GradientDrawable
import android.hardware.display.DisplayManager
import android.os.Bundle
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.provider.Settings
import android.util.Base64
import android.util.Log
import android.view.Display
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.widget.LinearLayout
import android.widget.TextView
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import java.io.ByteArrayOutputStream
import java.util.UUID
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicInteger

/**
 * Runs in :accessibility. The system, not our app, owns this service's binding.
 * An explicit, targeted automation session may show a non-obscuring blue frame
 * over the named third-party app. OS-granted node actions and screenshot /
 * gestures are possible ONLY during a separately approved, targeted session.
 * Every call checks the unlocked display, live service and current target.
 */
class AgentAccessibilityService : AccessibilityService() {
  companion object {
    private const val DEBUG_TAG = "McpAutoIndicator"
    private const val MAX_NODES = 48
    private const val MAX_LABEL_CHARS = 96
    // A separately approved action can spend up to ~22s waiting for the user,
    // plus ChatGPT/tool dispatch time. 12s made legitimate click approvals
    // expire before they could execute. Keep a bounded window while binding
    // every use to the same session, target, foreground window and screen.
    private const val SNAPSHOT_LIFETIME_MS = 90_000L
    private const val MAX_IMAGE_BYTES = 128 * 1024
    private const val MAX_IMAGE_SIDE = 900
    @Volatile private var boundToSystem = false
    @Volatile private var boundSinceElapsedMs = 0L
    @Volatile private var current: AgentAccessibilityService? = null

    fun isConnected(): Boolean = boundToSystem
    fun connectedSinceElapsedMs(): Long =
      if (boundToSystem) boundSinceElapsedMs else 0L

    private fun unavailable(reason: String): Bundle = Bundle().apply {
      putBoolean("active", false)
      putBoolean("visible", false)
      putBoolean("stopRequested", false)
      putString("reason", reason)
    }

    /** Private same-UID ContentProvider calls enter on a Binder thread. Only the
     * service's main thread may touch WindowManager or the overlay session. */
    private fun withBoundService(action: (AgentAccessibilityService) -> Bundle): Bundle {
      val service = current ?: return unavailable("service_not_connected")
      if (!boundToSystem) return unavailable("service_not_connected")
      if (Looper.myLooper() == Looper.getMainLooper()) return action(service)

      val result = AtomicReference<Bundle>()
      val pending = AtomicBoolean(true)
      val completed = CountDownLatch(1)
      val runnable = Runnable {
        try {
          if (pending.compareAndSet(true, false))
            result.set(if (boundToSystem && current === service) action(service)
              else unavailable("service_not_connected"))
        } finally {
          completed.countDown()
        }
      }
      if (!service.main.post(runnable)) return unavailable("service_unavailable")
      if (!completed.await(2, TimeUnit.SECONDS)) {
        pending.compareAndSet(true, false)
        service.main.removeCallbacks(runnable)
        return unavailable("service_timeout")
      }
      return result.get() ?: unavailable("service_unavailable")
    }

    fun automationStart(sessionId: String, targetPackage: String): Bundle =
      withBoundService { it.startSession(sessionId, targetPackage) }

    fun automationStop(): Bundle = withBoundService { service ->
      service.endSession(false)
      service.snapshot()
    }

    fun automationStatus(): Bundle = withBoundService { it.snapshot() }
    fun automationCapabilities(): Bundle = withBoundService { it.capabilities() }
    fun automationSnapshot(sessionId: String, targetPackage: String, mode: String): Bundle {
      val semantic = withBoundService { it.semanticSnapshot(sessionId, targetPackage, mode) }
      if (semantic.getBoolean("ok", false)) return semantic
      if (mode == "realtime" && semantic.getString("reason") != "semantic_unavailable") return semantic
      if (mode != "realtime" && mode != "screenshot_click") return semantic
      // Only absence of actual semantic capability permits screenshot fallback.
      // Missing focus/session, password fields or locked screen must fail closed.
      return captureScreenshot(sessionId, targetPackage, mode)
    }
    fun automationClick(args: Bundle): Bundle = withBoundService { it.click(args) }
    fun automationFill(args: Bundle): Bundle = withBoundService { it.fill(args) }
    fun automationGesture(args: Bundle): Bundle = withBoundService { it.gesture(args) }

    private fun captureScreenshot(sessionId: String, targetPackage: String, mode: String): Bundle {
      val service = current ?: return unavailable("service_not_connected")
      if (!boundToSystem) return unavailable("service_not_connected")
      if (Looper.myLooper() == Looper.getMainLooper()) return unavailable("capture_requires_worker")
      val completed = CountDownLatch(1)
      val waiting = AtomicBoolean(true)
      val result = AtomicReference<Bundle>()
      if (!service.main.post {
          if (waiting.get() && boundToSystem && current === service)
            service.captureSnapshot(sessionId, targetPackage, mode) { reply ->
              if (waiting.compareAndSet(true, false)) { result.set(reply); completed.countDown() }
            }
          else if (waiting.compareAndSet(true, false)) {
            result.set(unavailable("service_not_connected")); completed.countDown()
          }
        }) return unavailable("service_unavailable")
      if (!completed.await(5, TimeUnit.SECONDS)) {
        waiting.set(false)
        return unavailable("screenshot_timeout")
      }
      return result.get() ?: unavailable("screenshot_unavailable")
    }
  }

  private val main = Handler(Looper.getMainLooper())
  private val imageWorker = Executors.newSingleThreadExecutor { work ->
    Thread(work, "mcp-accessibility-frame").apply { isDaemon = true }
  }
  private val screenshotInFlight = AtomicBoolean(false)
  private data class NodeRecord(val fingerprint: String, val clickable: Boolean, val editable: Boolean,
                                val password: Boolean, val bounds: Rect)
  private data class ActionSnapshot(val id: String, val session: String, val target: String,
                                    val mode: String, val created: Long, val generation: Int,
                                    val nodes: List<NodeRecord>, val screenWidth: Int,
                                    val screenHeight: Int)
  private var lastActionSnapshot: ActionSnapshot? = null
  private var windowGeneration = 0
  private var activeSessionId: String? = null
  private var targetPackageName: String? = null
  private var targetEverFocused = false
  private var lastEndedSessionId: String? = null
  private var overlayVisible = false
  private var stopRequested = false
  private var stoppedSessionId: String? = null
  private var lastFailure: String? = null
  private var windowEventsSeen = 0
  private var targetEventsSeen = 0
  private val overlays = mutableListOf<View>()
  private val blue = Color.rgb(16, 103, 246)
  private var screenOffReceiverRegistered = false
  private val screenOffReceiver = object : BroadcastReceiver() {
    override fun onReceive(context: Context?, intent: Intent?) {
      if (intent?.action == Intent.ACTION_SCREEN_OFF) revokeSession("screen_locked")
    }
  }

  private fun registerScreenOffWatch() {
    if (screenOffReceiverRegistered) return
    try {
      val filter = IntentFilter(Intent.ACTION_SCREEN_OFF)
      if (Build.VERSION.SDK_INT >= 33)
        registerReceiver(screenOffReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
      else registerReceiver(screenOffReceiver, filter)
      screenOffReceiverRegistered = true
    } catch (error: RuntimeException) {
      Log.w(DEBUG_TAG, "screen_off_watch_unavailable class=${error.javaClass.simpleName}")
    }
  }

  private fun unregisterScreenOffWatch() {
    if (!screenOffReceiverRegistered) return
    screenOffReceiverRegistered = false
    try { unregisterReceiver(screenOffReceiver) } catch (_: RuntimeException) {}
  }

  private fun revokeSession(reason: String) {
    if (activeSessionId == null) return
    endSession(false)
    lastFailure = reason
    Log.i(DEBUG_TAG, "session_revoked reason=$reason")
  }

  private fun targetFocusPackage(): String? = try {
    if (hasNodePermission()) rootInActiveWindow?.packageName?.toString() else null
  } catch (_: RuntimeException) { null }

  /** A keyboard or system shade is temporary; any other foreground app
   * (including our own Activity) ends the target-bound session. */
  private fun isDifferentApp(pkg: String?): Boolean {
    if (pkg == null || pkg == targetPackageName || pkg == "android" ||
        pkg == "com.android.systemui" ||
        pkg.startsWith("com.google.android.permissioncontroller") ||
        pkg.startsWith("com.android.permissioncontroller")) return false
    val ime = try {
      ComponentName.unflattenFromString(Settings.Secure.getString(
        contentResolver, Settings.Secure.DEFAULT_INPUT_METHOD).orEmpty())?.packageName
    } catch (_: RuntimeException) { null }
    return pkg != ime
  }

  private fun refreshSessionState() {
    if (activeSessionId == null) return
    if (locked()) { revokeSession("screen_locked"); return }
    if (targetEverFocused && isDifferentApp(targetFocusPackage()))
      revokeSession("app_switched")
  }

  override fun onServiceConnected() {
    super.onServiceConnected()
    registerScreenOffWatch()
    boundSinceElapsedMs = SystemClock.elapsedRealtime().coerceAtLeast(1L)
    current = this
    boundToSystem = true
    Log.i(DEBUG_TAG, "service_connected")
  }

  /** No window content, events or action history is copied to the main app. */
  override fun onAccessibilityEvent(event: AccessibilityEvent?) {
    if (activeSessionId == null || (event?.eventType != AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED &&
        event?.eventType != AccessibilityEvent.TYPE_WINDOWS_CHANGED)) return
    windowEventsSeen++
    refreshSessionState()
    if (activeSessionId == null) return
    val observedPackage = event.packageName?.toString() ?: return
    if (event.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED &&
        targetEverFocused && isDifferentApp(observedPackage) &&
        targetFocusPackage() == observedPackage) {
      revokeSession("app_switched")
      return
    }
    if (event.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) {
      // Navigation within the same app preserves the session, but cannot
      // preserve a prior indexed node identity or screenshot coordinates.
      windowGeneration++
      lastActionSnapshot = null
    }
    if (observedPackage == targetPackageName) {
      targetEventsSeen++
      Log.i(DEBUG_TAG, "target_window_event type=${event.eventType} visible=$overlayVisible")
      if (!overlayVisible) attachOverlay()
      if (overlayVisible && targetFocusPackage() == targetPackageName)
        targetEverFocused = true
    } else if (event.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) {
      // Transient OS panels and input methods do not revoke the session.
      // Screen actions still require the exact target foreground root.
      if (overlayVisible) {
        Log.i(DEBUG_TAG, "transient_window_ignored package=${observedPackage.take(90)} " +
          "class=${event.className?.toString()?.take(100)}")
      }
    }
  }

  override fun onInterrupt() {
    Log.i(DEBUG_TAG, "service_interrupted")
    endSession(false)
    lastFailure = "accessibility_interrupted"
  }

  private fun startSession(sessionId: String, targetPackage: String): Bundle {
    if (!boundToSystem || current !== this) return unavailable("service_not_connected")
    if (!screenOffReceiverRegistered) return unavailable("screen_lock_watch_unavailable")
    if (locked()) return unavailable("screen_locked")
    if (activeSessionId == null && sessionId == lastEndedSessionId)
      return unavailable("session_already_ended")
    if (activeSessionId != null && (activeSessionId != sessionId ||
        targetPackageName != targetPackage)) {
      revokeSession("session_or_target_mismatch")
      return unavailable("session_or_target_mismatch")
    }
    if (activeSessionId == null) {
      lastActionSnapshot = null
      removeOverlay()
      activeSessionId = sessionId
      targetPackageName = targetPackage
      targetEverFocused = false
      stopRequested = false
      stoppedSessionId = null
      lastFailure = null
      windowEventsSeen = 0
      targetEventsSeen = 0
      Log.i(DEBUG_TAG, "session_armed")
    }
    // Armed is different from visible. An OS window event must attest that
    // the named third-party app is actually in the foreground before drawing.
    return snapshot(if (overlayVisible) null else "waiting_for_target_window")
  }

  private fun snapshot(reason: String? = null): Bundle {
    refreshSessionState()
    val connected = boundToSystem && current === this
    val active = connected && activeSessionId != null
    val screenUnlocked = connected && !locked()
    val targetFocused = active && screenUnlocked && overlayVisible &&
      targetFocusPackage() == targetPackageName
    if (targetFocused) targetEverFocused = true
    return Bundle().apply {
      putBoolean("active", active)
      putBoolean("visible", active && overlayVisible)
      putBoolean("targetFocused", targetFocused)
      putBoolean("screenUnlocked", screenUnlocked)
      putBoolean("stopRequested", stopRequested)
      putInt("windowEventsSeen", windowEventsSeen)
      putInt("targetEventsSeen", targetEventsSeen)
      putString("sessionId", activeSessionId)
      putString("targetPackage", targetPackageName)
      putString("stoppedSessionId", stoppedSessionId)
      (reason ?: lastFailure)?.let { putString("reason", it) }
    }
  }

  private fun locked(): Boolean {
    val power = getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return true
    val keyguard = getSystemService(Context.KEYGUARD_SERVICE) as? KeyguardManager ?: return true
    return !power.isInteractive || keyguard.isKeyguardLocked || keyguard.isDeviceLocked
  }

  private fun hasNodePermission(): Boolean =
    (serviceInfo?.capabilities ?: 0) and
      AccessibilityServiceInfo.CAPABILITY_CAN_RETRIEVE_WINDOW_CONTENT != 0

  private fun hasGesturePermission(): Boolean =
    (serviceInfo?.capabilities ?: 0) and
      AccessibilityServiceInfo.CAPABILITY_CAN_PERFORM_GESTURES != 0

  private fun hasScreenshotPermission(): Boolean =
    Build.VERSION.SDK_INT >= 30 && ((serviceInfo?.capabilities ?: 0) and
      AccessibilityServiceInfo.CAPABILITY_CAN_TAKE_SCREENSHOT != 0)

  private fun capabilities(): Bundle {
    refreshSessionState()
    return Bundle().apply {
      val connected = boundToSystem && current === this@AgentAccessibilityService
      putBoolean("connected", connected)
      putBoolean("nodeContent", connected && hasNodePermission())
      putBoolean("gesture", connected && hasGesturePermission())
      putBoolean("screenshot", connected && hasScreenshotPermission())
      putBoolean("sessionActive", connected && activeSessionId != null)
      putBoolean("targetVisible", connected && activeSessionId != null && overlayVisible &&
        targetFocusPackage() == targetPackageName)
      putBoolean("screenUnlocked", connected && !locked())
    }
  }

  /** Never act outside a live and visibly established third-party target. */
  private fun actionGuard(session: String, target: String): String? {
    if (!boundToSystem || current !== this) return "service_not_connected"
    if (locked()) { revokeSession("screen_locked"); return "screen_locked" }
    refreshSessionState()
    if (session != activeSessionId || target != targetPackageName || session.isEmpty()) {
      revokeSession("session_or_target_mismatch")
      return "session_or_target_mismatch"
    }
    if (!overlayVisible || targetEventsSeen <= 0) return "target_not_visible"
    if (targetFocusPackage() != target) return "target_not_focused"
    targetEverFocused = true
    return null
  }

  private fun error(reason: String): Bundle = Bundle().apply {
    putBoolean("ok", false)
    putString("reason", reason)
  }

  private fun screenSize(): Point = Point().also {
    @Suppress("DEPRECATION")
    (getSystemService(Context.WINDOW_SERVICE) as WindowManager)
      .defaultDisplay.getRealSize(it)
  }

  private val privateLabel = Regex(
    "(?i)(?:[A-Z0-9._%+-]+@[A-Z0-9.-]+\\.[A-Z]{2,}|\\b[0-9]{4,}\\b|" +
      "password|passcode|one.time|otp|security.code|api.key|access.token|" +
      "authorization|bearer|private.key|recovery.code|seed.phrase)"
  )

  private fun label(raw: CharSequence?): String {
    val cleaned = raw?.toString()
      ?.filterNot { Character.isISOControl(it) ||
        Character.getType(it) == Character.FORMAT.toInt() }
      ?.take(MAX_LABEL_CHARS) ?: ""
    return if (privateLabel.containsMatchIn(cleaned)) "[redacted]" else cleaned
  }

  private fun fingerprint(node: AccessibilityNodeInfo, rect: Rect): String =
    listOf(node.packageName, node.className, node.viewIdResourceName,
      rect.left, rect.top, rect.right, rect.bottom, node.text?.hashCode(),
      node.contentDescription?.hashCode(), node.isClickable, node.isEditable,
      node.isPassword).joinToString("|")

  /** Recompute on every action; index and fingerprint must still match the
   * indexed snapshot. Never keep stale AccessibilityNodeInfo across calls. */
  private fun nodes(): Pair<List<NodeRecord>, ArrayList<Bundle>> {
    val records = ArrayList<NodeRecord>()
    val output = ArrayList<Bundle>()
    val root = rootInActiveWindow ?: return records to output
    val pending = java.util.ArrayDeque<AccessibilityNodeInfo>()
    pending.add(root)
    var visited = 0
    while (pending.isNotEmpty() && records.size < MAX_NODES && visited < 256) {
      val node = pending.removeFirst()
      visited++
      try {
        if (!node.isVisibleToUser) continue
        val rect = Rect().also { node.getBoundsInScreen(it) }
        val protected = node.isPassword ||
          privateLabel.containsMatchIn(node.text?.toString().orEmpty()) ||
          privateLabel.containsMatchIn(node.contentDescription?.toString().orEmpty())
        val clickable = !protected && node.isClickable && node.isEnabled
        val editable = !protected && node.isEditable && node.isEnabled
        if (rect.width() > 0 && rect.height() > 0) {
          val index = records.size + 1
          records.add(NodeRecord(fingerprint(node, rect), clickable, editable,
            protected, Rect(rect)))
          output.add(Bundle().apply {
            putInt("index", index)
            putString("className", label(node.className))
            // Never return values of editable fields: non-password inputs may
            // still contain an email, OTP, secret or personal message.
            putString("text", if (protected || editable) "" else label(node.text))
            putString("description", if (protected || editable) "" else label(node.contentDescription))
            putBoolean("clickable", clickable)
            putBoolean("editable", editable)
            putBoolean("protected", protected)
            putInt("left", rect.left)
            putInt("top", rect.top)
            putInt("right", rect.right)
            putInt("bottom", rect.bottom)
          })
        }
        for (i in 0 until minOf(node.childCount, 32)) {
          node.getChild(i)?.let { pending.addLast(it) }
        }
      } finally {
        @Suppress("DEPRECATION")
        node.recycle()
      }
    }
    while (pending.isNotEmpty()) {
      @Suppress("DEPRECATION")
      pending.removeFirst().recycle()
    }
    return records to output
  }

  private fun freshSnapshot(session: String, target: String, snapshotId: String): Pair<ActionSnapshot?,String?> {
    val guard = actionGuard(session, target)
    if (guard != null) return null to guard
    val stored = lastActionSnapshot ?: return null to "snapshot_required"
    if (stored.id != snapshotId || stored.session != session || stored.target != target ||
        stored.generation != windowGeneration ||
        SystemClock.elapsedRealtime() - stored.created !in 0L..SNAPSHOT_LIFETIME_MS)
      return null to "snapshot_expired"
    return stored to null
  }

  private fun semanticSnapshot(session: String, target: String, mode: String): Bundle {
    actionGuard(session, target)?.let { return error(it) }
    if (mode != "realtime") return error("semantic_unavailable")
    if (!hasNodePermission()) return error("semantic_unavailable")
    val root = rootInActiveWindow ?: return error("semantic_unavailable")
    if (root.packageName?.toString() != target) return error("target_not_focused")
    val (records, elements) = nodes()
    if (records.isEmpty()) return error("semantic_unavailable")
    val size = screenSize()
    val id = UUID.randomUUID().toString()
    lastActionSnapshot = ActionSnapshot(id, session, target, "realtime",
      SystemClock.elapsedRealtime(), windowGeneration, records, size.x, size.y)
    return Bundle().apply {
      putBoolean("ok", true)
      putString("selectedMode", mode)
      putString("effectiveMode", "realtime")
      putBoolean("fallbackUsed", false)
      putString("snapshotId", id)
      putString("mimeType", "none")
      putInt("screenWidth", size.x)
      putInt("screenHeight", size.y)
      putParcelableArrayList("elements", elements)
    }
  }

  /** A screenshot is not safe when the currently visible tree includes an
   * editable value, password, OTP, email, key-like label, or cannot be scanned
   * within a bounded budget. Android FLAG_SECURE is independently respected by
   * takeScreenshot, which returns an OS error. When the OS does not grant
   * content retrieval we cannot infer any text, so display-level permission
   * and explicit per-call approval remain mandatory for screenshot use. */
  private fun hasSensitiveContent(): Boolean {
    if (!hasNodePermission()) return false
    val root = rootInActiveWindow ?: return true
    val queue = java.util.ArrayDeque<AccessibilityNodeInfo>()
    queue.add(root)
    var scanned = 0
    var sensitive = false
    while (queue.isNotEmpty() && scanned < 256) {
      val node = queue.removeFirst()
      scanned++
      try {
        if (!node.isVisibleToUser) continue
        val rawText = node.text?.toString().orEmpty()
        val rawDescription = node.contentDescription?.toString().orEmpty()
        if (node.isPassword || node.isEditable && rawText.isNotBlank() ||
            privateLabel.containsMatchIn(rawText) ||
            privateLabel.containsMatchIn(rawDescription)) {
          sensitive = true
          break
        }
        for (i in 0 until minOf(node.childCount, 32)) {
          node.getChild(i)?.let { queue.addLast(it) }
        }
      } finally {
        @Suppress("DEPRECATION")
        node.recycle()
      }
    }
    val truncated = queue.isNotEmpty()
    while (queue.isNotEmpty()) {
      @Suppress("DEPRECATION")
      queue.removeFirst().recycle()
    }
    return sensitive || truncated
  }

  private fun captureSnapshot(session: String, target: String, selectedMode: String,
                              complete: (Bundle) -> Unit) {
    actionGuard(session, target)?.let { complete(error(it)); return }
    if (!hasScreenshotPermission() || !hasGesturePermission()) {
      complete(error("screenshot_or_gesture_permission_missing")); return
    }
    if (hasSensitiveContent()) { complete(error("sensitive_content_visible")); return }
    if (!screenshotInFlight.compareAndSet(false, true)) {
      complete(error("screenshot_busy")); return
    }
    val generation = windowGeneration
    val size = screenSize()
    val finished = AtomicBoolean(false)
    fun finish(result: Bundle) {
      if (!finished.compareAndSet(false, true)) return
      screenshotInFlight.set(false)
      complete(result)
    }
    try {
      takeScreenshot(Display.DEFAULT_DISPLAY, imageWorker, object : TakeScreenshotCallback {
        override fun onFailure(errorCode: Int) {
          main.post { finish(error("screenshot_os_denied")) }
        }
        override fun onSuccess(result: ScreenshotResult) {
          val encoded: Pair<ByteArray,Pair<Int,Int>>? = try {
            val buffer = result.hardwareBuffer
            val original = try { Bitmap.wrapHardwareBuffer(buffer, result.colorSpace) }
              finally { buffer.close() }
            if (original == null || original.width < 1 || original.height < 1 ||
                original.width.toLong() * original.height > 8_000_000) null else {
              val scale = minOf(1.0, MAX_IMAGE_SIDE.toDouble() /
                maxOf(original.width, original.height))
              val width = maxOf(1, (original.width * scale).toInt())
              val height = maxOf(1, (original.height * scale).toInt())
              val scaled = Bitmap.createScaledBitmap(original, width, height, true)
              var bytes: ByteArray? = null
              for (quality in listOf(65, 45, 25)) {
                val stream = ByteArrayOutputStream()
                scaled.compress(Bitmap.CompressFormat.JPEG, quality, stream)
                if (stream.size() in 1..MAX_IMAGE_BYTES) { bytes = stream.toByteArray(); break }
              }
              if (scaled !== original) scaled.recycle()
              original.recycle()
              bytes?.let { it to (width to height) }
            }
          } catch (_: Exception) { null }
          main.post {
            if (actionGuard(session, target) != null ||
                generation != windowGeneration || hasSensitiveContent()) {
              finish(error("target_or_screen_changed"))
            } else if (encoded == null) {
              finish(error("screenshot_size_or_encoding_failed"))
            } else {
              val id = UUID.randomUUID().toString()
              lastActionSnapshot = ActionSnapshot(id, session, target, "screenshot_click",
                SystemClock.elapsedRealtime(), generation, emptyList(), size.x, size.y)
              finish(Bundle().apply {
                putBoolean("ok", true)
                putString("selectedMode", selectedMode)
                putString("effectiveMode", "screenshot_click")
                putBoolean("fallbackUsed", selectedMode == "realtime")
                putString("snapshotId", id)
                putString("mimeType", "image/jpeg")
                putInt("screenWidth", size.x)
                putInt("screenHeight", size.y)
                putInt("imageWidth", encoded.second.first)
                putInt("imageHeight", encoded.second.second)
                putInt("imageBytes", encoded.first.size)
                putString("imageBase64", Base64.encodeToString(encoded.first, Base64.NO_WRAP))
              })
            }
          }
        }
      })
    } catch (_: Exception) { finish(error("screenshot_os_unavailable")) }
  }

  /** Execute against a fresh tree only when the specific indexed node still
   * has its original bounds/identity. Never select by raw cross-app coordinates
   * or permit a protected field to become an actionable target. */
  private fun withIndexedNode(index: Int, stored: ActionSnapshot,
                              action: (AccessibilityNodeInfo, NodeRecord) -> Bundle): Bundle {
    if (!hasNodePermission() || stored.mode != "realtime") return error("semantic_unavailable")
    if (index !in 1..stored.nodes.size) return error("node_index_invalid")
    val original = stored.nodes[index - 1]
    val root = rootInActiveWindow ?: return error("semantic_unavailable")
    if (root.packageName?.toString() != stored.target) return error("target_not_focused")
    val queue = java.util.ArrayDeque<AccessibilityNodeInfo>()
    queue.add(root)
    var seen = 0
    var listed = 0
    while (queue.isNotEmpty() && seen < 256 && listed < MAX_NODES) {
      val node = queue.removeFirst()
      seen++
      try {
        if (!node.isVisibleToUser) continue
        val bounds = Rect().also { node.getBoundsInScreen(it) }
        if (bounds.width() > 0 && bounds.height() > 0) {
          listed++
          if (listed == index) {
            if (original.password || node.isPassword ||
                fingerprint(node, bounds) != original.fingerprint)
              return error("node_changed_or_protected")
            return action(node, original)
          }
        }
        for (i in 0 until minOf(node.childCount, 32)) {
          node.getChild(i)?.let { queue.addLast(it) }
        }
      } finally {
        @Suppress("DEPRECATION")
        node.recycle()
      }
    }
    while (queue.isNotEmpty()) {
      @Suppress("DEPRECATION")
      queue.removeFirst().recycle()
    }
    return error("node_no_longer_visible")
  }

  private fun result(mode: String, selected: String, dispatch: Boolean,
                     fallback: Boolean, completed: Boolean): Bundle = Bundle().apply {
    putBoolean("ok", dispatch)
    putBoolean("dispatched", dispatch)
    // performAction returning true only attests OS accepted the action. For a
    // gesture, callback arrives later and completion is NOT falsely claimed.
    putBoolean("completed", completed)
    putString("effectiveMode", mode)
    putString("selectedMode", selected)
    putBoolean("fallbackUsed", fallback)
  }

  private fun click(args: Bundle): Bundle {
    val session = args.getString("sessionId").orEmpty()
    val target = args.getString("targetPackage").orEmpty()
    val selected = args.getString("selectedMode").orEmpty()
    val id = args.getString("snapshotId").orEmpty()
    val (stored, reason) = freshSnapshot(session, target, id)
    if (stored == null) return error(reason ?: "snapshot_required")
    if (selected != "realtime" && selected != "screenshot_click") return error("invalid_mode")
    if (stored.mode == "realtime" && selected == "realtime") {
      val index = args.getInt("index", 0)
      return withIndexedNode(index, stored) { node, record ->
        if (!record.clickable || node.isPassword || !node.isClickable)
          error("node_not_clickable")
        else {
          val accepted = node.performAction(AccessibilityNodeInfo.ACTION_CLICK)
          if (accepted) result("realtime", selected, true, false, false)
          else error("semantic_click_failed")
        }
      }
    }
    if (stored.mode != "screenshot_click") return error("screenshot_snapshot_required")
    val gestureArgs = Bundle(args).apply {
      putString("kind", "tap")
      putInt("toX", getInt("x", -1))
      putInt("toY", getInt("y", -1))
      putInt("durationMs", 110)
    }
    return gesture(gestureArgs)
  }

  private fun fill(args: Bundle): Bundle {
    val session = args.getString("sessionId").orEmpty()
    val target = args.getString("targetPackage").orEmpty()
    val id = args.getString("snapshotId").orEmpty()
    val content = args.getString("text") ?: return error("invalid_text")
    if (content.length !in 1..512 || content.any { it.code == 0 || Character.isISOControl(it) })
      return error("invalid_text")
    val (stored, reason) = freshSnapshot(session, target, id)
    if (stored == null) return error(reason ?: "snapshot_required")
    if (stored.mode != "realtime" || !hasNodePermission()) return error("semantic_unavailable")
    return withIndexedNode(args.getInt("index", 0), stored) { node, record ->
      if (!record.editable || node.isPassword || !node.isEditable)
        error("node_not_editable_or_protected")
      else {
        val arguments = Bundle().apply {
          putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, content)
        }
        val accepted = node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, arguments)
        if (accepted) result("realtime", "realtime", true, false, false)
        else error("semantic_fill_failed")
      }
    }
  }

  private fun gesture(args: Bundle): Bundle {
    val session = args.getString("sessionId").orEmpty()
    val target = args.getString("targetPackage").orEmpty()
    val selected = args.getString("selectedMode").orEmpty()
    val id = args.getString("snapshotId").orEmpty()
    val (stored, reason) = freshSnapshot(session, target, id)
    if (stored == null) return error(reason ?: "snapshot_required")
    if (!hasGesturePermission()) return error("gesture_permission_missing")
    if (stored.mode != "screenshot_click" || !hasScreenshotPermission())
      return error("screenshot_snapshot_required")
    if (selected !in listOf("realtime", "screenshot_click")) return error("invalid_mode")
    val kind = args.getString("kind").orEmpty()
    if (kind != "tap" && kind != "swipe") return error("invalid_gesture")
    val x = args.getInt("x", -1)
    val y = args.getInt("y", -1)
    val toX = args.getInt("toX", -1)
    val toY = args.getInt("toY", -1)
    if (x !in 8 until stored.screenWidth - 8 ||
        y !in 8 until stored.screenHeight - 8 ||
        toX !in 8 until stored.screenWidth - 8 ||
        toY !in 8 until stored.screenHeight - 8)
      return error("gesture_outside_screen")
    val duration = if (kind == "tap") 110 else args.getInt("durationMs", -1)
    if (kind == "swipe" && duration !in 120..1500) return error("invalid_gesture_duration")
    val path = Path().apply {
      moveTo(x.toFloat(), y.toFloat())
      if (kind == "swipe") lineTo(toX.toFloat(), toY.toFloat())
    }
    // Recheck before each OS dispatch, not merely at snapshot creation.
    actionGuard(session, target)?.let { return error(it) }
    val dispatched = try {
      dispatchGesture(GestureDescription.Builder().addStroke(
        GestureDescription.StrokeDescription(path, 0L, duration.toLong())).build(),
        object : GestureResultCallback() {
          override fun onCompleted(gestureDescription: GestureDescription?) {
            // No persistent grant or queued work follows a gesture callback.
          }
          override fun onCancelled(gestureDescription: GestureDescription?) {
            // The caller must observe a new snapshot; no automatic retry.
          }
        }, main)
    } catch (_: Exception) { false }
    return if (dispatched) result("screenshot_click", selected, true,
      selected == "realtime", false)
    else error("gesture_dispatch_denied")
  }

  private fun dp(value: Int): Int = (value * resources.displayMetrics.density + 0.5f).toInt()

  private fun params(width: Int, height: Int, gravity: Int,
                     touchable: Boolean, y: Int = 0): WindowManager.LayoutParams =
    WindowManager.LayoutParams(width, height,
      WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
      WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
        WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
        (if (touchable) WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL
         else WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE),
      PixelFormat.TRANSLUCENT).apply {
      this.gravity = gravity
      this.y = y
    }

  private fun addFrame(windowManager: WindowManager, width: Int, height: Int,
                       gravity: Int) {
    val border = View(this).apply { setBackgroundColor(blue) }
    windowManager.addView(border, params(width, height, gravity, false))
    overlays.add(border)
  }

  private fun attachOverlay() {
    if (!boundToSystem || activeSessionId == null || overlayVisible) return
    val manager = getSystemService(WINDOW_SERVICE) as WindowManager
    try {
      // Separate slender, non-touchable windows preserve taps and gestures in
      // the third-party app. Only the small Stop pill handles touch input.
      addFrame(manager, WindowManager.LayoutParams.MATCH_PARENT, dp(4), Gravity.TOP)
      addFrame(manager, WindowManager.LayoutParams.MATCH_PARENT, dp(4), Gravity.BOTTOM)
      addFrame(manager, dp(4), WindowManager.LayoutParams.MATCH_PARENT, Gravity.START)
      addFrame(manager, dp(4), WindowManager.LayoutParams.MATCH_PARENT, Gravity.END)

      val label = LinearLayout(this).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(12), dp(7), dp(7), dp(7))
        background = GradientDrawable().apply {
          setColor(blue)
          cornerRadius = dp(12).toFloat()
        }
        addView(TextView(this@AgentAccessibilityService).apply {
          text = "Mcp Agent Ai is using this app"
          setTextColor(Color.WHITE)
          textSize = 12f
        })
        addView(TextView(this@AgentAccessibilityService).apply {
          text = "Stop"
          setTextColor(blue)
          textSize = 12f
          gravity = Gravity.CENTER
          setPadding(dp(12), dp(6), dp(12), dp(6))
          background = GradientDrawable().apply {
            setColor(Color.WHITE)
            cornerRadius = dp(8).toFloat()
          }
          contentDescription = "Stop Mcp Agent Ai app automation"
          isClickable = true
          isFocusable = true
          setOnClickListener { endSession(true) }
        }, LinearLayout.LayoutParams(
          LinearLayout.LayoutParams.WRAP_CONTENT,
          LinearLayout.LayoutParams.WRAP_CONTENT).apply { leftMargin = dp(10) })
      }
      manager.addView(label, params(WindowManager.LayoutParams.WRAP_CONTENT,
        WindowManager.LayoutParams.WRAP_CONTENT,
        Gravity.TOP or Gravity.CENTER_HORIZONTAL, true, dp(36)))
      overlays.add(label)
      overlayVisible = true
      Log.i(DEBUG_TAG, "overlay_visible")
    } catch (_: RuntimeException) {
      // Missing OS binding or an OEM overlay rejection must never be shown as
      // successful app control. Reject this session and clean partial windows.
      endSession(false)
      lastFailure = "overlay_unavailable"
      Log.w(DEBUG_TAG, "overlay_unavailable")
    }
  }

  private fun removeOverlay() {
    val manager = getSystemService(WINDOW_SERVICE) as WindowManager
    overlays.asReversed().forEach { view ->
      try { manager.removeViewImmediate(view) } catch (_: RuntimeException) {}
    }
    overlays.clear()
    overlayVisible = false
  }

  private fun endSession(byUser: Boolean) {
    if (activeSessionId != null) lastEndedSessionId = activeSessionId
    if (byUser && activeSessionId != null) {
      stoppedSessionId = activeSessionId
      stopRequested = true
    }
    activeSessionId = null
    targetPackageName = null
    targetEverFocused = false
    lastActionSnapshot = null
    windowGeneration++
    removeOverlay()
  }

  override fun onUnbind(intent: Intent?): Boolean {
    Log.i(DEBUG_TAG, "service_unbound")
    endSession(false)
    unregisterScreenOffWatch()
    boundToSystem = false
    boundSinceElapsedMs = 0L
    if (current === this) current = null
    return super.onUnbind(intent)
  }

  override fun onDestroy() {
    endSession(false)
    unregisterScreenOffWatch()
    imageWorker.shutdownNow()
    boundToSystem = false
    boundSinceElapsedMs = 0L
    if (current === this) current = null
    super.onDestroy()
  }
}
