package ai.mcpagent.app

import android.app.Activity
import android.app.AlertDialog
import android.app.Dialog
import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.text.TextUtils
import android.text.InputType
import android.util.Base64
import android.view.Gravity
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.view.Window
import android.view.WindowManager
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.view.animation.AccelerateInterpolator
import android.view.animation.Animation
import android.view.animation.DecelerateInterpolator
import android.view.animation.TranslateAnimation
import android.webkit.CookieManager
import androidx.webkit.ProfileStore
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import android.webkit.GeolocationPermissions
import android.webkit.PermissionRequest
import android.webkit.SslErrorHandler
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.net.http.SslError
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.net.URLEncoder
import java.util.UUID
import kotlin.math.sqrt

@InvokeArg
class PrivateBrowserOpenArgs {
  lateinit var url: String
  var userInitiated: Boolean = false
}

@InvokeArg
class PrivateBrowserConsentArgs {
  var userInitiated: Boolean = false
}

@InvokeArg
class PrivateBrowserTabArgs {
  var tabId: Int = 0
}

@InvokeArg
class PrivateBrowserActionArgs {
  var tabId: Int = 0
  var snapshotId: Long = 0
  var index: Int = 0
  var value: String = ""
  var deltaY: Int = 0
}

@InvokeArg
class PrivateBrowserPreviewPrepareArgs {
  var tabId: Int = 0
  var userInitiated: Boolean = false
}

@InvokeArg
class PrivateBrowserPreviewCaptureArgs {
  var tabId: Int = 0
  var previewToken: String = ""
  var userInitiated: Boolean = false
}

/**
 * Optional on-screen Android WebViews owned solely by this plugin. The Tauri
 * app's WebView and personal Chrome are never attached or automated.
 * When the Android WebView provider supports MULTI_PROFILE, these WebViews use
 * a fresh named profile, separate from the app's default WebView data profile.
 * Without MULTI_PROFILE the browser cannot open. The Android app process is
 * still shared and deletion of the owned profile's data may finish async.
 * No JavascriptInterface is installed, no app data is injected into pages.
 */
@TauriPlugin
class PrivateBrowserPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val MAX_TABS = 5
    private const val MAX_URL = 2048
    private const val MAX_SEARCH_LENGTH = 512
    private const val HANDSET_DRAWER_MAX_DP = 600
    private const val TABLET_DRAWER_MAX_DP = 520
    private const val DRAWER_SLIDE_MS = 190L
    private const val MAX_ELEMENTS = 64
    private const val MAX_TEXT = 6000
    private const val MAX_VALUE = 4096
    private const val MAX_PREVIEW_DIMENSION = 1024
    private const val MAX_PREVIEW_PIXELS = 786432
    private const val MAX_PREVIEW_PNG_BYTES = 1024 * 1024
    private const val PREVIEW_TOKEN_LIFETIME_MS = 60000L
    // Only profiles created by this plugin may ever be deleted. Persist a
    // bounded queue because Android's WebView provider can release a just-
    // destroyed WebView asynchronously (and process death can interrupt it).
    private const val PRIVATE_PROFILE_CLEANUP_PREFS = "private_browser_owned_profiles_v1"
    private const val PRIVATE_PROFILE_CLEANUP_KEY = "pending_names"
    private const val RETAINED_PROFILE_KEY = "retained_profile"
    private const val RETAINED_TABS_KEY = "retained_tabs"
    private const val RETAINED_ACTIVE_KEY = "retained_active_index"
    private const val MAX_PENDING_PROFILE_DELETIONS = 16
    internal fun isOwnedProfileName(name: String): Boolean =
      name.matches(Regex("^cos_inapp_[a-f0-9]{32}$"))

    internal fun retainedUrlsValid(
      urls: List<String>, acceptsHttps: (String) -> Boolean = ::allowedHttps
    ): Boolean = urls.size in 1..MAX_TABS &&
      urls.all { it.length <= MAX_URL && acceptsHttps(it) }

    /** Synthetic test seam; all dimension calculations are performed before allocation. */
    internal fun previewDimensions(width: Int, height: Int): Pair<Int, Int>? {
      if (width !in 1..8192 || height !in 1..8192) return null
      val pixels = width.toLong() * height.toLong()
      val scale = minOf(
        1.0, MAX_PREVIEW_DIMENSION.toDouble() / width,
        MAX_PREVIEW_DIMENSION.toDouble() / height,
        sqrt(MAX_PREVIEW_PIXELS.toDouble() / pixels)
      )
      val outWidth = (width * scale).toInt().coerceAtLeast(1)
      val outHeight = (height * scale).toInt().coerceAtLeast(1)
      if (outWidth > MAX_PREVIEW_DIMENSION || outHeight > MAX_PREVIEW_DIMENSION ||
        outWidth.toLong() * outHeight > MAX_PREVIEW_PIXELS) return null
      return Pair(outWidth, outHeight)
    }

    /** Same validation is used for direct URLs and every top-level redirect. */
    internal fun allowedHttps(raw: String): Boolean {
      if (raw.length !in 9..MAX_URL || raw.any { it.isWhitespace() || Character.isISOControl(it) }) return false
      return try {
        val uri = Uri.parse(raw)
        uri.scheme.equals("https", ignoreCase = true) &&
          uri.isHierarchical && !uri.host.isNullOrBlank() &&
          uri.userInfo.isNullOrEmpty() && uri.port != 0
      } catch (_: Exception) {
        false
      }
    }

    /** Drawer occupies the whole phone; tablets keep chat visible on the left. */
    internal fun drawerWidthDp(screenWidthDp: Int): Int {
      if (screenWidthDp <= 0) return 0
      return if (screenWidthDp <= HANDSET_DRAWER_MAX_DP) screenWidthDp
      else minOf(TABLET_DRAWER_MAX_DP, (screenWidthDp * 0.70f).toInt().coerceAtLeast(1))
    }

    /** Only a typed address/search is normalized; remote open still requires
     * an explicitly authorized, already-valid HTTPS URL as before. No script,
     * intent, file, http, or other scheme is interpreted as a search query. */
    internal fun addressOrSearch(
      raw: String, acceptsHttps: (String) -> Boolean = ::allowedHttps
    ): String? {
      val input = raw.trim()
      if (input.isEmpty() || input.length > MAX_URL ||
        input.any { Character.isISOControl(it) }) return null
      if (input.startsWith("https://", ignoreCase = true))
        return input.takeIf(acceptsHttps)
      if (input.startsWith("//") || input.startsWith("\\") ||
        input.contains("://") || Regex("^[a-zA-Z][a-zA-Z0-9+.-]*:").containsMatchIn(input))
        return null
      // Chrome-like bare host/path input is upgraded to HTTPS, not HTTP. An
      // email, string with spaces or unknown host is treated as a search.
      if (!input.any { it.isWhitespace() } && !input.contains('@')) {
        val host = input.substringBefore('/').substringBefore('?').substringBefore('#')
        if (Regex("^[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+$")
            .matches(host)) {
          val target = "https://$input"
          if (acceptsHttps(target)) return target
        }
      }
      if (input.length > MAX_SEARCH_LENGTH) return null
      val query = URLEncoder.encode(input, "UTF-8").replace("+", "%20")
      return "https://www.google.com/search?q=$query".takeIf(acceptsHttps)
    }
  }

  private data class Node(val selector: String, val signature: String, val fillable: Boolean)
  private data class Snapshot(
    val id: Long,
    val generation: Long,
    val url: String,
    val nodes: Map<Int, Node>
  )
  private data class PreviewGrant(
    val token: String, val tabId: Int, val session: Long, val pageGeneration: Long,
    val url: String, val expiresAt: Long
  )
  private class Tab(val id: Int, val webView: WebView) {
    var generation: Long = 0
    var snapshot: Snapshot? = null
    var currentUrl: String = ""
    var visualCommitted = false
    var loadError: String? = null
    var loadTimeout: Runnable? = null
  }
  private data class RetainedSession(val profile: String, val urls: List<String>, val active: Int)

  private val tabs = LinkedHashMap<Int, Tab>()
  private var currentId: Int = 0
  private var nextId: Int = 1
  private var nextSnapshotId: Long = 1
  private var automationAllowed = false
  private var pendingOpenConsent = false
  private var pendingAutomationConsent = false
  private var pendingPreviewConsent = false
  private var previewGrant: PreviewGrant? = null
  private var sessionGeneration: Long = 1
  private var sessionProfileName: String? = null
  private var lastProfileCleanup: String = "not_attempted"
  private var dialog: Dialog? = null
  // Dialog.hide() retains the owned WebViews/cookies. Dialog.isShowing remains
  // true while hidden, so visibility must be tracked independently.
  private var drawerVisible = false
  private var drawerRoot: View? = null
  private var closingBrowser = false
  private var viewport: FrameLayout? = null
  private var tabSpinner: Spinner? = null
  private var address: EditText? = null
  private var updatingTabs = false
  private var tabLabels = emptyList<String>()
  private var browserSubtitle: TextView? = null
  private var browserNotice: TextView? = null
  private var browserProgress: ProgressBar? = null
  private var backAction: TextView? = null
  private var forwardAction: TextView? = null
  private var reloadAction: TextView? = null
  private var closeTabAction: TextView? = null
  private var addTabAction: TextView? = null
  private val loadHandler = Handler(Looper.getMainLooper())
  private var cleanupRetryScheduled = false
  private var cleanupRetries = 0
  private var restoringSavedSession = false

  init {
    // No service or periodic background work: retry cleanup when our
    // application plugin is created, and only for our own private profiles.
    loadHandler.post { retryOwnedProfileCleanup() }
  }

  private fun pendingOwnedProfileNames(): MutableSet<String> =
    activity.getSharedPreferences(PRIVATE_PROFILE_CLEANUP_PREFS, Context.MODE_PRIVATE)
      .getStringSet(PRIVATE_PROFILE_CLEANUP_KEY, emptySet())
      ?.filterTo(mutableSetOf()) { isOwnedProfileName(it) } ?: mutableSetOf()

  private fun sessionPreferences() =
    activity.getSharedPreferences(PRIVATE_PROFILE_CLEANUP_PREFS, Context.MODE_PRIVATE)

  /** The named WebView provider owns cookies. Preferences store only bounded
   * HTTPS tab addresses and an owned-profile receipt, never cookie data. */
  private fun retainedSession(): RetainedSession? {
    return try {
      val prefs = sessionPreferences()
      val profile = prefs.getString(RETAINED_PROFILE_KEY, null) ?: return null
      if (!isOwnedProfileName(profile) || profile !in pendingOwnedProfileNames()) return null
      val saved = prefs.getString(RETAINED_TABS_KEY, null) ?: return null
      if (saved.length > MAX_TABS * (MAX_URL + 4)) return null
      val array = JSONArray(saved)
      val urls = (0 until array.length()).map { array.getString(it) }
      if (!retainedUrlsValid(urls)) return null
      RetainedSession(profile, urls,
        prefs.getInt(RETAINED_ACTIVE_KEY, 0).coerceIn(0, urls.lastIndex))
    } catch (_: Exception) { null }
  }

  private fun persistRetainedSession() {
    if (restoringSavedSession) return
    val profile = sessionProfileName ?: return
    val urls = tabs.values.map { it.currentUrl }
    if (!isOwnedProfileName(profile) || !retainedUrlsValid(urls)) return
    val selected = tabs.keys.indexOf(currentId).coerceAtLeast(0)
    val saved = sessionPreferences().edit()
      .putString(RETAINED_PROFILE_KEY, profile)
      .putString(RETAINED_TABS_KEY, JSONArray(urls).toString())
      .putInt(RETAINED_ACTIVE_KEY, selected)
      .commit()
    if (!saved) lastProfileCleanup = "session_metadata_save_failed"
  }

  private fun clearRetainedSession() {
    if (!sessionPreferences().edit()
        .remove(RETAINED_PROFILE_KEY).remove(RETAINED_TABS_KEY)
        .remove(RETAINED_ACTIVE_KEY).commit())
      lastProfileCleanup = "session_metadata_clear_failed"
  }

  private fun savePendingOwnedProfileNames(names: Set<String>): Boolean =
    activity.getSharedPreferences(PRIVATE_PROFILE_CLEANUP_PREFS, Context.MODE_PRIVATE)
      .edit().putStringSet(PRIVATE_PROFILE_CLEANUP_KEY, names.toSet()).commit()

  private fun rememberOwnedProfile(name: String) {
    val pending = pendingOwnedProfileNames()
    if (pending.size >= MAX_PENDING_PROFILE_DELETIONS || !isOwnedProfileName(name)) {
      throw SecurityException("Private browser profile cleanup capacity reached")
    }
    pending.add(name)
    check(savePendingOwnedProfileNames(pending)) {
      "Cannot safely persist private browser profile cleanup receipt"
    }
  }

  private fun retryOwnedProfileCleanup() {
    if (!supportsIndependentProfile()) {
      if (pendingOwnedProfileNames().isNotEmpty())
        lastProfileCleanup = "deferred_provider_unavailable"
      return
    }
    val pending = pendingOwnedProfileNames()
    if (pending.isEmpty()) return
    val remaining = pending.toMutableSet()
    val retainedProfile = retainedSession()?.profile
    for (name in pending) {
      // Never delete an active browser profile, nor any profile not issued
      // by our random-name generator. ProfileStore false means already absent.
      if (name == sessionProfileName || name == retainedProfile) continue
      try {
        val deletionRequested = ProfileStore.getInstance().deleteProfile(name)
        remaining.remove(name)
        lastProfileCleanup = if (deletionRequested) "requested_async_deletion"
          else "profile_already_absent"
      } catch (_: IllegalStateException) {
        lastProfileCleanup = "deferred_provider_in_use"
      } catch (_: Exception) {
        lastProfileCleanup = "deferred_provider_refused"
      }
    }
    if (remaining != pending && !savePendingOwnedProfileNames(remaining))
      lastProfileCleanup = "deferred_receipt_save_failed"
    if (remaining.any { it != sessionProfileName && it != retainedProfile })
      scheduleOwnedProfileCleanup()
  }

  private fun scheduleOwnedProfileCleanup() {
    if (cleanupRetryScheduled || cleanupRetries >= 3) return
    val waitMs = longArrayOf(500L, 2_000L, 5_000L)[cleanupRetries++]
    cleanupRetryScheduled = true
    loadHandler.postDelayed({
      cleanupRetryScheduled = false
      retryOwnedProfileCleanup()
    }, waitMs)
  }

  private fun ui(invoke: Invoke, work: () -> Unit) {
    activity.runOnUiThread {
      try { work() } catch (_: Exception) { invoke.reject("Private browser operation failed") }
    }
  }

  private fun parseOpen(invoke: Invoke): PrivateBrowserOpenArgs? = try {
    invoke.parseArgs(PrivateBrowserOpenArgs::class.java)
  } catch (_: Exception) {
    invoke.reject("Invalid private browser URL arguments")
    null
  }

  private fun parseTab(invoke: Invoke): PrivateBrowserTabArgs? = try {
    invoke.parseArgs(PrivateBrowserTabArgs::class.java)
  } catch (_: Exception) {
    invoke.reject("Invalid private browser tab")
    null
  }

  private fun parseAction(invoke: Invoke): PrivateBrowserActionArgs? = try {
    invoke.parseArgs(PrivateBrowserActionArgs::class.java)
  } catch (_: Exception) {
    invoke.reject("Invalid private browser action")
    null
  }

  private fun json(value: JSONObject): JSObject = JSObject().apply {
    val names = value.keys()
    while (names.hasNext()) {
      val key = names.next()
      put(key, value.get(key))
    }
  }

  private fun tabInfo(tab: Tab): JSONObject = JSONObject().apply {
    put("tabId", tab.id)
    put("url", tab.currentUrl)
    put("title", (tab.webView.title ?: "").take(150))
    put("active", tab.id == currentId)
    put("loading", tab.webView.progress < 100)
  }

  private fun supportsIndependentProfile(): Boolean = try {
    WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)
  } catch (_: Exception) {
    false
  }

  private fun listResponse(): JSObject = JSObject().apply {
    val shown = drawerVisible && dialog?.isShowing == true
    val anchoredEnd = shown && dialog?.window?.attributes?.gravity
      ?.and(Gravity.RELATIVE_HORIZONTAL_GRAVITY_MASK) == Gravity.END
    val screenWidthDp = availableScreenWidthDp()
    val handsetOverlay = screenWidthDp <= HANDSET_DRAWER_MAX_DP
    val isolated = sessionProfileName != null && tabs.isNotEmpty()
    val activeTab = tabs[currentId]
    val activeTabVisible = shown && !closingBrowser && isolated && activeTab != null &&
      activeTab.webView.parent === viewport &&
      activeTab.webView.visibility == View.VISIBLE
    val pageIsHttps = activeTab?.webView?.url?.let { allowedHttps(it) } == true
    put("opened", shown)
    put("automationAllowed", automationAllowed && !closingBrowser)
    put("profileIsolationSupported", supportsIndependentProfile())
    put("isolatedProfile", isolated)
    put("profileLimitation", "Independent WebView data profile when supported; same app process. Profile deletion can finish asynchronously")
    put("cookiesBlockedForSession", false)
    put("cookieScope", "Separate browser-only profile; first-party cookies may exist until deletion")
    put("lastProfileCleanup", lastProfileCleanup)
    put("pendingOwnedProfileDeletions", pendingOwnedProfileNames().size)
    put("activeTabId", currentId)
    put("maxTabs", MAX_TABS)
    put("tabs", JSONArray().apply { tabs.values.forEach { put(tabInfo(it)) } })
    // Actual native END-gravity Dialog window; a narrow tablet drawer leaves
    // the host chat visible. Nothing ever controls Chrome or its cookies.
    put("mode", "in_app_webview")
    put("supported", supportsIndependentProfile())
    put("realtime", true)
    put("presentation", if (anchoredEnd) "native_right_drawer" else "not_presented")
    put("fullscreenVisible", shown && isolated && handsetOverlay && anchoredEnd)
    put("rightSidebarImplemented", anchoredEnd)
    put("drawerWidthDp", drawerWidthDp(screenWidthDp))
    put("handsetOverlay", handsetOverlay)
    put("activeTabVisible", activeTabVisible)
    put("activeTabHttps", pageIsHttps)
    put("domControl", !closingBrowser && automationAllowed && activeTabVisible && pageIsHttps)
    put("chromeControlled", false)
  }

  @Command
  fun status(invoke: Invoke) = ui(invoke) { invoke.resolve(listResponse()) }

  @Command
  fun listTabs(invoke: Invoke) = ui(invoke) { invoke.resolve(listResponse()) }

  /** Open a separate, visible browser window only on explicit user request. */
  @Command
  fun open(invoke: Invoke) {
    val args = parseOpen(invoke) ?: return
    if (!args.userInitiated || !allowedHttps(args.url)) {
      invoke.reject("Explicit user initiation and HTTPS URL required")
      return
    }
    ui(invoke) {
      if (closingBrowser) { invoke.reject("Private browser is closing"); return@ui }
      // The browser toolbar always passes the Google start page. Reopening
      // after Close returns to the exact existing tab and history.
      if (!drawerVisible && dialog != null && tabs.isNotEmpty() &&
          args.url == "https://www.google.com/") {
        showExistingDrawer()
        invoke.resolve(JSObject().apply {
          put("opened", true)
          put("reopened", true)
          put("tab", tabInfo(tabs[currentId]!!))
        })
        return@ui
      }
      if (tabs.size >= MAX_TABS) { invoke.reject("Private browser tab limit reached"); return@ui }
      if (pendingOpenConsent) { invoke.reject("Browser opening confirmation is pending"); return@ui }
      if (dialog == null) {
        if (!supportsIndependentProfile()) {
          invoke.reject("Isolated Android WebView profiles are not supported on this device")
          return@ui
        }
        pendingOpenConsent = true
        val consentGeneration = sessionGeneration
        val hostname = Uri.parse(args.url).host ?: "HTTPS website"
        AlertDialog.Builder(activity)
          .setTitle("Open in-app browser?")
          .setMessage("Open $hostname in an isolated browser? This also enables JavaScript and DOM tools for the visible tab, including page snapshots and approved indexed actions. Close hides the drawer and preserves its tabs and cookies; closing all tabs ends the session. No Chrome data is imported.")
          .setPositiveButton("Open") { _, _ ->
            val stillCurrent = pendingOpenConsent && consentGeneration == sessionGeneration
            pendingOpenConsent = false
            if (!stillCurrent) {
              invoke.resolve(JSObject().apply { put("opened", false); put("cancelled", true) })
            } else {
              try {
                setAutomationAllowed(true) // Authorized by the combined native dialog.
                openConfirmedTab(invoke, args.url)
              }
              catch (_: Exception) { invoke.reject("Cannot open secure in-app browser") }
            }
          }
          .setNegativeButton("Cancel") { _, _ ->
            pendingOpenConsent = false
            invoke.resolve(JSObject().apply { put("opened", false); put("cancelled", true) })
          }
          .setOnCancelListener {
            pendingOpenConsent = false
            invoke.resolve(JSObject().apply { put("opened", false); put("cancelled", true) })
          }
          .show()
      } else {
        openConfirmedTab(invoke, args.url)
      }
    }
  }

  private fun openConfirmedTab(invoke: Invoke, url: String) {
    if (closingBrowser) { invoke.reject("Private browser is closing"); return }
    val restoring = dialog == null && retainedSession() != null
    if (dialog == null) showBrowser()
    else if (!drawerVisible) showExistingDrawer()
    if (!supportsIndependentProfile() || sessionProfileName == null) {
      endBrowserSession()
      invoke.reject("Independent WebView data profile is unavailable")
      return
    }
    if (restoring && url == "https://www.google.com/" && tabs.isNotEmpty()) {
      invoke.resolve(JSObject().apply {
        put("opened", true)
        put("reopened", true)
        put("tab", tabInfo(tabs[currentId]!!))
      })
      return
    }
    if (tabs.size >= MAX_TABS) { invoke.reject("Private browser tab limit reached"); return }
    val tab = createTab(url)
    invoke.resolve(JSObject().apply { put("opened", true); put("tab", tabInfo(tab)) })
  }

  /** DOM automation requires consent through a visible Android dialog. */
  @Command
  fun requestAutomation(invoke: Invoke) {
    val args = try { invoke.parseArgs(PrivateBrowserConsentArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid consent request"); return }
    if (!args.userInitiated) { invoke.reject("Explicit user initiation required"); return }
    ui(invoke) {
      if (closingBrowser || !drawerVisible || dialog?.isShowing != true || tabs.isEmpty()) {
        invoke.reject("Open the private browser first")
        return@ui
      }
      consentDialog { allowed ->
        invoke.resolve(JSObject().apply { put("allowed", allowed) })
      }
    }
  }

  private fun setAutomationAllowed(allowed: Boolean) {
    automationAllowed = allowed
    previewGrant = null
    tabs.values.forEach {
      it.snapshot = null
      it.webView.settings.javaScriptEnabled = allowed
      it.webView.settings.domStorageEnabled = allowed
    }
    refreshChrome()
  }

  private fun consentDialog(onChoice: (Boolean) -> Unit) {
    if (pendingAutomationConsent) {
      onChoice(false)
      return
    }
    pendingAutomationConsent = true
    val consentGeneration = sessionGeneration
    AlertDialog.Builder(activity)
      .setTitle("Allow in-app browser DOM tools?")
      .setMessage("Enable JavaScript and permit page text snapshots and indexed click, fill, and scroll for this visible session? Avoid sensitive information.")
      .setPositiveButton("Allow") { _, _ ->
        pendingAutomationConsent = false
        val stillCurrent = !closingBrowser && consentGeneration == sessionGeneration &&
          drawerVisible && dialog?.isShowing == true
        if (stillCurrent) setAutomationAllowed(true)
        onChoice(stillCurrent)
      }
      .setNegativeButton("Deny") { _, _ ->
        pendingAutomationConsent = false
        setAutomationAllowed(false)
        onChoice(false)
      }
      .setOnCancelListener {
        pendingAutomationConsent = false
        setAutomationAllowed(false)
        onChoice(false)
      }
      .show()
  }

  @Command
  fun switchTab(invoke: Invoke) {
    val args = parseTab(invoke) ?: return
    ui(invoke) {
      val tab = tabs[args.tabId]
      if (tab == null) { invoke.reject("Unknown private browser tab"); return@ui }
      selectTab(tab.id)
      invoke.resolve(JSObject().apply { put("tab", tabInfo(tab)) })
    }
  }

  @Command
  fun closeTab(invoke: Invoke) {
    val args = parseTab(invoke) ?: return
    ui(invoke) {
      if (!tabs.containsKey(args.tabId)) { invoke.reject("Unknown private browser tab"); return@ui }
      removeTab(args.tabId)
      invoke.resolve(listResponse())
    }
  }

  @Command
  fun closeAll(invoke: Invoke) = ui(invoke) {
    endBrowserSession()
    invoke.resolve(listResponse())
  }

  private fun endBrowserSession() {
    // Dialog dismiss callbacks may be queued. Detach that listener before
    // synchronously clearing the current session, so a delayed callback cannot
    // tear down a brand new browser opened immediately afterward.
    dialog?.let { instance ->
      instance.setOnDismissListener(null)
      instance.dismiss()
    }
    resetBrowser()
  }

  private fun dp(value: Int): Int =
    (value * activity.resources.displayMetrics.density + 0.5f).toInt()

  private fun roundedPanel(fill: Int, border: Int, radius: Int = 10): GradientDrawable =
    GradientDrawable().apply {
      shape = GradientDrawable.RECTANGLE
      cornerRadius = dp(radius).toFloat()
      setColor(fill)
      setStroke(dp(1), border)
    }

  /** Compact, accessible native controls, without the device theme's large
   * default gray Button padding and minimum dimensions. */
  private fun browserAction(
    label: String,
    description: String,
    destructive: Boolean = false,
    onClick: () -> Unit
  ): TextView = TextView(activity).apply {
    text = label
    contentDescription = description
    textSize = if (label.length > 5) 11f else 14f
    typeface = Typeface.DEFAULT_BOLD
    setTextColor(if (destructive) Color.rgb(255, 218, 222) else Color.rgb(228, 236, 250))
    gravity = Gravity.CENTER
    isClickable = true
    isFocusable = true
    minHeight = dp(40)
    setPadding(dp(4), 0, dp(4), 0)
    background = RippleDrawable(
      ColorStateList.valueOf(Color.argb(85, 173, 202, 255)),
      roundedPanel(
        if (destructive) Color.rgb(66, 36, 47) else Color.rgb(32, 43, 61),
        if (destructive) Color.rgb(130, 67, 80) else Color.rgb(56, 72, 95)
      ),
      null
    )
    setOnClickListener { if (!closingBrowser) onClick() }
  }

  private fun toolbarAction(row: LinearLayout, view: TextView, widthDp: Int) {
    row.addView(view, LinearLayout.LayoutParams(dp(widthDp), dp(40)).apply {
      leftMargin = dp(4)
    })
  }

  private fun tabAdapter(labels: List<String>): ArrayAdapter<String> =
    object : ArrayAdapter<String>(activity, android.R.layout.simple_spinner_item, labels) {
      private fun row(position: Int, recycled: View?, dropdown: Boolean): View =
        (recycled as? TextView ?: TextView(activity)).apply {
          text = getItem(position).orEmpty()
          textSize = 12f
          typeface = Typeface.DEFAULT_BOLD
          setTextColor(Color.rgb(226, 235, 249))
          gravity = Gravity.CENTER_VERTICAL
          isSingleLine = true
          ellipsize = TextUtils.TruncateAt.END
          minHeight = dp(40)
          setPadding(dp(11), 0, dp(7), 0)
          setBackgroundColor(if (dropdown) Color.rgb(29, 40, 58) else Color.TRANSPARENT)
        }

      override fun getView(position: Int, convertView: View?, parent: ViewGroup): View =
        row(position, convertView, false)

      private val tabIds = tabs.keys.toList()
      override fun getDropDownView(position: Int, convertView: View?, parent: ViewGroup): View =
        LinearLayout(activity).apply {
          orientation = LinearLayout.HORIZONTAL
          gravity = Gravity.CENTER_VERTICAL
          setBackgroundColor(Color.rgb(29, 40, 58))
          minimumHeight = dp(46)
          addView(row(position, null, true), LinearLayout.LayoutParams(0, dp(46), 1f))
          addView(TextView(activity).apply {
            text = "×"
            textSize = 23f
            gravity = Gravity.CENTER
            setTextColor(Color.rgb(245, 165, 174))
            contentDescription = "Close tab ${tabIds.getOrNull(position) ?: position + 1}"
            isClickable = true
            isFocusable = true
            setOnClickListener {
              val id = tabIds.getOrNull(position)
              if (id != null && tabs.containsKey(id) && !closingBrowser) {
                // Defer adapter replacement until this dropdown click finishes.
                loadHandler.post { if (tabs.containsKey(id)) removeTab(id) }
              }
            }
          }, LinearLayout.LayoutParams(dp(46), dp(46)))
        }
    }

  private fun invalidatePageActions(tab: Tab) {
    tab.snapshot = null
    if (previewGrant?.tabId == tab.id) previewGrant = null
  }

  private fun goToAddress() {
    if (closingBrowser) return
    val target = addressOrSearch(address?.text?.toString().orEmpty())
    val tab = tabs[currentId]
    if (tab == null || target == null || !allowedHttps(target)) {
      Toast.makeText(activity, "Enter HTTPS address or search terms", Toast.LENGTH_SHORT).show()
      return
    }
    invalidatePageActions(tab)
    val editor = address
    editor?.clearFocus()
    (activity.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager)
      ?.hideSoftInputFromWindow(editor?.windowToken, 0)
    tab.webView.loadUrl(target) // Existing HTTPS guard applies to all redirects.
  }

  private fun slideDrawer(root: View, entering: Boolean, onEnd: (() -> Unit)? = null) {
    val animation = TranslateAnimation(Animation.RELATIVE_TO_SELF,
      if (entering) 1f else 0f, Animation.RELATIVE_TO_SELF,
      if (entering) 0f else 1f, Animation.RELATIVE_TO_SELF, 0f,
      Animation.RELATIVE_TO_SELF, 0f).apply {
      duration = DRAWER_SLIDE_MS
      interpolator = if (entering) DecelerateInterpolator() else AccelerateInterpolator()
      setAnimationListener(object : Animation.AnimationListener {
        override fun onAnimationStart(animation: Animation?) = Unit
        override fun onAnimationRepeat(animation: Animation?) = Unit
        override fun onAnimationEnd(animation: Animation?) { onEnd?.invoke() }
      })
    }
    root.startAnimation(animation)
  }

  private fun showExistingDrawer() {
    val instance = dialog ?: return
    if (closingBrowser || tabs.isEmpty()) return
    instance.show() // Re-shows the same WebViews and their existing history.
    drawerVisible = true
    instance.window?.setLayout(drawerWindowWidth(), ViewGroup.LayoutParams.MATCH_PARENT)
    previewGrant = null
    tabs.values.forEach { it.snapshot = null }
    refreshChrome()
    drawerRoot?.let { root ->
      root.isEnabled = true
      root.requestFocus()
      root.post { if (drawerVisible && dialog === instance) slideDrawer(root, entering = true) }
    }
  }

  private fun closeDrawer() {
    val instance = dialog ?: return
    if (closingBrowser || !drawerVisible) return
    closingBrowser = true
    previewGrant = null
    sessionGeneration++
    tabs.values.forEach { it.snapshot = null }
    val root = drawerRoot
    if (!instance.isShowing || root == null) {
      drawerVisible = false
      instance.hide()
      closingBrowser = false
      return
    }
    root.isEnabled = false
    slideDrawer(root, entering = false) {
      if (dialog === instance && closingBrowser) {
        drawerVisible = false
        instance.hide()
        closingBrowser = false
      }
    }
    // A detached window may not dispatch the animation-end callback.
    loadHandler.postDelayed(Runnable {
      if (dialog === instance && closingBrowser) {
        drawerVisible = false
        instance.hide()
        closingBrowser = false
      }
    }, DRAWER_SLIDE_MS + 150L)
  }

  private fun availableScreenWidthDp(): Int {
    val metrics = activity.resources.displayMetrics
    return activity.resources.configuration.screenWidthDp.takeIf { it > 0 }
      ?: (metrics.widthPixels / metrics.density).toInt().coerceAtLeast(1)
  }

  private fun drawerWindowWidth(): Int {
    val metrics = activity.resources.displayMetrics
    return dp(drawerWidthDp(availableScreenWidthDp())).coerceAtMost(metrics.widthPixels)
  }

  private fun cancelLoadTimeout(tab: Tab) {
    tab.loadTimeout?.let(loadHandler::removeCallbacks)
    tab.loadTimeout = null
  }

  private fun scheduleLoadTimeout(tab: Tab) {
    cancelLoadTimeout(tab)
    val generation = tab.generation
    val task = Runnable {
      if (tabs[tab.id] === tab && tab.generation == generation &&
          !tab.visualCommitted && tab.loadError == null) {
        tab.loadError = "This page is taking longer than expected. Try Reload."
        if (currentId == tab.id) refreshChrome()
      }
    }
    tab.loadTimeout = task
    loadHandler.postDelayed(task, 10_000L)
  }

  private fun pageLoadFailed(tab: Tab, message: String) {
    tab.visualCommitted = false
    tab.loadError = message
    cancelLoadTimeout(tab)
    if (currentId == tab.id) refreshChrome()
  }

  private fun showBrowser() {
    // Feature availability depends on the installed WebView provider, not just
    // Android's OS version. Never open on the shared Default profile.
    if (!supportsIndependentProfile()) throw SecurityException("MULTI_PROFILE unavailable")
    check(sessionProfileName == null && tabs.isEmpty())
    retryOwnedProfileCleanup()
    val savedSession = retainedSession()
    val ownedName = savedSession?.profile ?:
      "cos_inapp_${UUID.randomUUID().toString().replace("-", "")}"
    if (savedSession == null) rememberOwnedProfile(ownedName)
    sessionProfileName = ownedName
    try {
      val root = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        setBackgroundColor(Color.rgb(14, 20, 33))
        isFocusableInTouchMode = true // Never raise the keyboard on first open.
      }

      // Header remains visible even when a site takes a long time to load.
      val header = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(12), dp(7), dp(12), dp(6))
      }
      val heading = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
      heading.addView(TextView(activity).apply {
        text = "Browser · Mcp Agent Ai is using"
        textSize = 13f
        typeface = Typeface.DEFAULT_BOLD
        setTextColor(Color.WHITE)
        maxLines = 2
        ellipsize = TextUtils.TruncateAt.END
      })
      browserSubtitle = TextView(activity).apply {
        text = "Isolated WebView · HTTPS only"
        textSize = 10f
        setTextColor(Color.rgb(155, 177, 208))
        maxLines = 1
        ellipsize = TextUtils.TruncateAt.END
      }
      heading.addView(browserSubtitle)
      header.addView(heading, LinearLayout.LayoutParams(0,
        ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
      toolbarAction(header, browserAction("Close", "Hide browser drawer; preserve tabs and login", true) {
        closeDrawer()
      }, 68)
      root.addView(header, LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))

      val tabsRow = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(8), 0, dp(12), dp(4))
      }
      tabSpinner = Spinner(activity).apply {
        contentDescription = "Select in-app browser tab"
        minimumHeight = dp(40)
        background = roundedPanel(Color.rgb(28, 39, 56), Color.rgb(57, 75, 103))
        setPopupBackgroundDrawable(roundedPanel(Color.rgb(29, 40, 58), Color.rgb(73, 91, 118)))
        dropDownWidth = minOf(dp(240),
          activity.resources.displayMetrics.widthPixels - dp(24))
        onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
          override fun onNothingSelected(parent: AdapterView<*>?) = Unit
          override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
            if (!updatingTabs) {
              val next = tabs.keys.elementAtOrNull(position)
              if (next != null && next != currentId) {
                address?.clearFocus()
                selectTab(next)
              }
            }
          }
        }
      }
      tabsRow.addView(tabSpinner, LinearLayout.LayoutParams(0, dp(40), 1f))
      addTabAction = browserAction("+", "Open another isolated tab") {
        if (tabs.size >= MAX_TABS) {
          Toast.makeText(activity, "Maximum 5 tabs", Toast.LENGTH_SHORT).show()
        } else {
          // New tabs stay in the same isolated profile, never personal Chrome.
          createTab("https://www.google.com/")
        }
      }
      toolbarAction(tabsRow, addTabAction!!, 40)
      root.addView(tabsRow)

      // One compact navigation/address row instead of the former large gray
      // Back / Forward / Reload row and separately oversized URL bar.
      val navigation = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(8), 0, dp(12), dp(7))
      }
      backAction = browserAction("‹", "Go back in this tab") {
        val tab = tabs[currentId]
        if (tab != null && tab.webView.canGoBack()) {
          invalidatePageActions(tab)
          tab.webView.goBack()
        }
      }
      toolbarAction(navigation, backAction!!, 40)
      forwardAction = browserAction("›", "Go forward in this tab") {
        val tab = tabs[currentId]
        if (tab != null && tab.webView.canGoForward()) {
          invalidatePageActions(tab)
          tab.webView.goForward()
        }
      }
      toolbarAction(navigation, forwardAction!!, 40)
      reloadAction = browserAction("↻", "Reload secure page") {
        val tab = tabs[currentId]
        if (tab != null && allowedHttps(tab.webView.url ?: "")) {
          invalidatePageActions(tab)
          tab.webView.reload()
        }
      }
      toolbarAction(navigation, reloadAction!!, 40)
      address = EditText(activity).apply {
        isSingleLine = true
        hint = "Search or enter HTTPS address"
        textSize = 12f
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
        imeOptions = EditorInfo.IME_ACTION_GO or EditorInfo.IME_FLAG_NO_EXTRACT_UI
        setTextColor(Color.rgb(242, 246, 255))
        setHintTextColor(Color.rgb(131, 155, 185))
        setSelectAllOnFocus(true)
        setPadding(dp(11), 0, dp(8), 0)
        background = roundedPanel(Color.rgb(26, 37, 53), Color.rgb(62, 80, 108))
        setOnEditorActionListener { _, actionId, _ ->
          if (actionId == EditorInfo.IME_ACTION_GO) {
            goToAddress()
            true
          } else false
        }
      }
      navigation.addView(address, LinearLayout.LayoutParams(0, dp(40), 1f).apply {
        leftMargin = dp(4)
      })
      toolbarAction(navigation, browserAction("Go", "Navigate to HTTPS address or web search") {
        goToAddress()
      }, 40)
      root.addView(navigation)

      browserProgress = ProgressBar(activity, null,
        android.R.attr.progressBarStyleHorizontal).apply {
        max = 100
        progress = 0
        progressTintList = ColorStateList.valueOf(Color.rgb(137, 171, 255))
        progressBackgroundTintList = ColorStateList.valueOf(Color.rgb(35, 47, 66))
      }
      root.addView(browserProgress, LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT, dp(2)))

      // Explicit MATCH_PARENT dialog content and weight=1 prevent the old
      // WRAP_CONTENT dialog chrome from measuring the actual WebView to zero.
      viewport = FrameLayout(activity).apply { setBackgroundColor(Color.WHITE) }
      root.addView(viewport, LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
      val instance = Dialog(activity)
      instance.requestWindowFeature(Window.FEATURE_NO_TITLE)
      instance.setContentView(root, ViewGroup.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
      instance.setCanceledOnTouchOutside(false)
      instance.setOnDismissListener {
        // An Android lifecycle dismissal keeps the named profile recoverable;
        // explicit Close all unregisters this listener before clearing it.
        resetBrowser(discardRetained = false)
      }
      instance.setOnKeyListener { _, code, event ->
        if (code != KeyEvent.KEYCODE_BACK) false
        else {
          if (event.action == KeyEvent.ACTION_UP && event.repeatCount == 0) closeDrawer()
          true
        }
      }
      instance.window?.apply {
        setBackgroundDrawable(ColorDrawable(Color.TRANSPARENT))
        addFlags(WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS)
        setGravity(Gravity.END or Gravity.TOP)
        addFlags(WindowManager.LayoutParams.FLAG_DIM_BEHIND)
        setDimAmount(if (availableScreenWidthDp() > HANDSET_DRAWER_MAX_DP) 0.2f else 0f)
        statusBarColor = Color.rgb(14, 20, 33)
        navigationBarColor = Color.rgb(14, 20, 33)
        setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE or
          WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_HIDDEN)
      }
      dialog = instance
      drawerRoot = root
      instance.show()
      drawerVisible = true
      instance.window?.setLayout(
        drawerWindowWidth(), ViewGroup.LayoutParams.MATCH_PARENT)
      root.post {
        if (dialog === instance && instance.isShowing && !closingBrowser)
          slideDrawer(root, entering = true)
      }
      root.requestFocus()
      if (savedSession != null) {
        restoringSavedSession = true
        try {
          savedSession.urls.forEach(::createTab)
          tabs.keys.elementAtOrNull(savedSession.active)?.let(::selectTab)
        } finally { restoringSavedSession = false }
        persistRetainedSession()
      }
    } catch (error: Exception) {
      dialog?.let { instance ->
        instance.setOnDismissListener(null)
        instance.dismiss()
      }
      resetBrowser(discardRetained = savedSession == null)
      throw error
    }
  }

  private fun resetBrowser(discardRetained: Boolean = true) {
    sessionGeneration++
    closingBrowser = false
    drawerVisible = false
    drawerRoot?.animation?.setAnimationListener(null)
    drawerRoot?.clearAnimation()
    drawerRoot = null
    previewGrant = null
    pendingPreviewConsent = false
    tabs.values.toList().forEach(::destroyTab)
    tabs.clear()
    currentId = 0
    automationAllowed = false
    pendingOpenConsent = false
    pendingAutomationConsent = false
    viewport = null
    address = null
    tabSpinner = null
    tabLabels = emptyList()
    browserSubtitle = null
    browserNotice = null
    browserProgress = null
    backAction = null
    forwardAction = null
    reloadAction = null
    closeTabAction = null
    addTabAction = null
    dialog = null
    restoringSavedSession = false
    if (discardRetained) clearRetainedSession()
    // No active WebViews remain. Only a uniquely generated profile name is
    // eligible for deletion; if the WebView provider still holds it, the
    // durable bounded receipt allows retry after release or next app start.
    sessionProfileName = null
    retryOwnedProfileCleanup()
  }

  private fun destroyTab(tab: Tab) {
    cancelLoadTimeout(tab)
    tab.snapshot = null
    (tab.webView.parent as? ViewGroup)?.removeView(tab.webView)
    tab.webView.stopLoading()
    tab.webView.clearHistory()
    // No clearCache/removeAllCookies; those can affect the app's default WebView.
    tab.webView.clearFormData()
    tab.webView.destroy()
  }

  private fun createTab(url: String): Tab {
    require(tabs.size < MAX_TABS && allowedHttps(url))
    if (!supportsIndependentProfile()) throw SecurityException("MULTI_PROFILE unavailable")
    val profileName = sessionProfileName ?: throw SecurityException("Private profile unavailable")
    val web = WebView(activity)
    // setProfile MUST precede all settings, clients, or navigation. If it
    // fails, destroy the unused view without ever loading a site into Default.
    try {
      WebViewCompat.setProfile(web, profileName)
    } catch (error: Exception) {
      runCatching { web.destroy() }
      throw SecurityException("Cannot assign isolated WebView profile", error)
    }
    val tab = Tab(nextId++, web).apply { currentUrl = url }
    try {
    web.setBackgroundColor(Color.WHITE)
    web.settings.apply {
      javaScriptEnabled = automationAllowed // Combined native open consent grants DOM use.
      domStorageEnabled = automationAllowed // Modern HTTPS logins require local storage.
      databaseEnabled = false
      allowFileAccess = false
      allowContentAccess = false
      allowFileAccessFromFileURLs = false
      allowUniversalAccessFromFileURLs = false
      mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
      cacheMode = WebSettings.LOAD_NO_CACHE
      setSupportMultipleWindows(false)
      javaScriptCanOpenWindowsAutomatically = false
      mediaPlaybackRequiresUserGesture = true
      if (Build.VERSION.SDK_INT >= 26) safeBrowsingEnabled = true
    }
    web.isSaveEnabled = false
    // This API is per-WebView (third-party cookies only), unlike the global
    // CookieManager.setAcceptCookie that previously affected the Tauri view.
    CookieManager.getInstance().setAcceptThirdPartyCookies(web, false)
    web.webChromeClient = object : WebChromeClient() {
      override fun onPermissionRequest(request: PermissionRequest) = request.deny()
      override fun onGeolocationPermissionsShowPrompt(origin: String?, callback: GeolocationPermissions.Callback?) {
        callback?.invoke(origin, false, false)
      }
      override fun onProgressChanged(view: WebView?, newProgress: Int) {
        if (tab.id == currentId && view === tab.webView) {
          browserProgress?.progress = newProgress.coerceIn(0, 100)
          if (newProgress == 100) refreshChrome()
        }
      }
    }
    web.webViewClient = object : WebViewClient() {
      override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest): Boolean =
        !allowedHttps(request.url.toString())

      @Deprecated("Compatibility for URL navigation on older Android WebViews")
      override fun shouldOverrideUrlLoading(view: WebView?, url: String?): Boolean =
        !allowedHttps(url ?: "")

      override fun shouldInterceptRequest(view: WebView?, request: WebResourceRequest): WebResourceResponse? {
        // Reject HTTP, content:, file:, intent:, blob: and all other subresources.
        if (allowedHttps(request.url.toString())) return null
        return WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden", emptyMap(),
          ByteArrayInputStream(ByteArray(0)))
      }

      override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
        if (view !== tab.webView || tabs[tab.id] !== tab) return
        tab.generation++
        tab.snapshot = null
        if (previewGrant?.tabId == tab.id) previewGrant = null
        tab.currentUrl = url ?: ""
        persistRetainedSession()
        tab.visualCommitted = false
        tab.loadError = null
        scheduleLoadTimeout(tab)
        if (!allowedHttps(tab.currentUrl)) view?.stopLoading()
        if (tab.id == currentId) refreshChrome()
      }

      // The URL/progress reaching 100 is not proof that Chromium actually
      // committed a drawable page. Keep a visible loading or error state until
      // the selected, owned HTTPS WebView reports a visual commit.
      override fun onPageCommitVisible(view: WebView?, url: String?) {
        if (view !== tab.webView || tabs[tab.id] !== tab ||
            url != tab.currentUrl || !allowedHttps(url.orEmpty()) ||
            tab.loadError != null) return
        tab.visualCommitted = true
        cancelLoadTimeout(tab)
        if (tab.id == currentId) refreshChrome()
      }

      override fun onPageFinished(view: WebView?, url: String?) {
        if (view !== tab.webView || tabs[tab.id] !== tab) return
        tab.currentUrl = url ?: ""
        persistRetainedSession()
        if (!allowedHttps(tab.currentUrl)) view?.stopLoading()
        if (tab.id == currentId) refreshChrome()
      }

      override fun onReceivedError(
        view: WebView?, request: WebResourceRequest, error: android.webkit.WebResourceError
      ) {
        if (view === tab.webView && tabs[tab.id] === tab && request.isForMainFrame) {
          pageLoadFailed(tab, "Network error or unreachable HTTPS website.")
        }
      }

      override fun onReceivedHttpError(
        view: WebView?, request: WebResourceRequest, response: WebResourceResponse
      ) {
        if (view === tab.webView && tabs[tab.id] === tab && request.isForMainFrame &&
            response.statusCode >= 400) {
          pageLoadFailed(tab, "Website responded with HTTP ${response.statusCode}.")
        }
      }

      override fun onReceivedSslError(view: WebView?, handler: SslErrorHandler, error: SslError?) {
        handler.cancel()
        if (view === tab.webView && tabs[tab.id] === tab) {
          pageLoadFailed(tab, "The secure connection could not be verified.")
        }
      }
    }
    tabs[tab.id] = tab
    selectTab(tab.id)
    web.loadUrl(url)
    persistRetainedSession()
    return tab
    } catch (error: Exception) {
      tabs.remove(tab.id)
      runCatching { (web.parent as? ViewGroup)?.removeView(web) }
      runCatching { web.stopLoading() }
      runCatching { web.destroy() }
      throw error
    }
  }

  private fun selectTab(id: Int) {
    val tab = tabs[id] ?: return
    previewGrant = null
    currentId = id
    viewport?.removeAllViews()
    (tab.webView.parent as? ViewGroup)?.removeView(tab.webView)
    viewport?.addView(tab.webView, FrameLayout.LayoutParams(
      ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
    browserNotice = TextView(activity).apply {
      textSize = 13f
      gravity = Gravity.CENTER
      setTextColor(Color.rgb(225, 234, 249))
      setPadding(dp(18), dp(14), dp(18), dp(14))
      background = roundedPanel(Color.rgb(27, 38, 56), Color.rgb(71, 91, 119), 12)
      isClickable = false
      isFocusable = false
    }.also { notice ->
      viewport?.addView(notice, FrameLayout.LayoutParams(
        ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT,
        Gravity.CENTER).apply {
        leftMargin = dp(20)
        rightMargin = dp(20)
      })
    }
    refreshChrome()
    persistRetainedSession()
  }

  private fun removeTab(id: Int) {
    previewGrant = null
    val tab = tabs.remove(id) ?: return
    destroyTab(tab)
    if (tabs.isEmpty()) endBrowserSession()
    else if (currentId == id) selectTab(tabs.keys.last())
    else refreshChrome()
    if (tabs.isNotEmpty()) persistRetainedSession()
  }

  private fun refreshChrome() {
    val tab = tabs[currentId]
    val currentUrl = tab?.currentUrl.orEmpty()
    // Never overwrite an address the user is typing on each WebView progress
    // callback. Switching tabs clears URL focus in selectTab or the spinner.
    if (address?.hasFocus() != true && address?.text?.toString() != currentUrl) {
      address?.setText(currentUrl)
    }
    val status = when {
      tab == null -> "Isolated WebView · waiting for a tab"
      tab.loadError != null -> "HTTPS page unavailable · Reload to retry"
      !tab.visualCommitted -> "Loading secure HTTPS page…"
      automationAllowed -> "Isolated WebView · DOM allowed for this session"
      else -> "Isolated WebView · HTTPS only"
    }
    browserSubtitle?.text = status
    browserNotice?.apply {
      visibility = if (tab != null && (tab.loadError != null || !tab.visualCommitted))
        View.VISIBLE else View.GONE
      text = when {
        tab?.loadError != null -> "Unable to show this secure page.\n${tab.loadError}\nUse ↻ Reload to retry."
        else -> "Opening secure page…"
      }
    }
    browserProgress?.apply {
      progress = tab?.webView?.progress?.coerceIn(0, 100) ?: 0
      alpha = if (tab != null && tab.loadError == null && !tab.visualCommitted) 1f else 0f
    }
    backAction?.let { it.isEnabled = tab?.webView?.canGoBack() == true; it.alpha = if (it.isEnabled) 1f else .4f }
    forwardAction?.let { it.isEnabled = tab?.webView?.canGoForward() == true; it.alpha = if (it.isEnabled) 1f else .4f }
    reloadAction?.let {
      it.isEnabled = tab != null && allowedHttps(tab.webView.url ?: "")
      it.alpha = if (it.isEnabled) 1f else .4f
    }
    closeTabAction?.let { it.isEnabled = tab != null; it.alpha = if (it.isEnabled) 1f else .4f }
    addTabAction?.let {
      it.isEnabled = tabs.size < MAX_TABS
      it.alpha = if (it.isEnabled) 1f else .4f
    }
    val spinner = tabSpinner ?: return
    val names = tabs.values.map { entry ->
      val host = try { Uri.parse(entry.currentUrl).host } catch (_: Exception) { null }
      "${entry.id} · ${(host ?: "Opening tab").take(24)} ▾"
    }
    updatingTabs = true
    try {
      // Recreating the Spinner adapter on every page callback can trigger a
      // delayed selection event and accidentally switch an active tab.
      if (names != tabLabels) {
        spinner.adapter = tabAdapter(names)
        tabLabels = names
      }
      val activeIndex = tabs.keys.indexOf(currentId)
      if (activeIndex >= 0 && spinner.selectedItemPosition != activeIndex) {
        spinner.setSelection(activeIndex, false)
      }
    } finally { updatingTabs = false }
  }

  /** Only the foreground, visibly selected view created by this plugin. */
  private fun visiblePreviewTab(id: Int): Tab? {
    if (closingBrowser || !drawerVisible) return null
    val tab = tabs[id] ?: return null
    val owner = viewport ?: return null
    val browserDialog = dialog ?: return null
    if (!supportsIndependentProfile() || sessionProfileName == null ||
      !browserDialog.isShowing || browserDialog.window?.decorView?.hasWindowFocus() != true ||
      id != currentId || tab.webView.parent !== owner || owner.visibility != View.VISIBLE ||
      !tab.webView.isShown || !tab.webView.isAttachedToWindow ||
      tab.webView.width < 1 || tab.webView.height < 1 ||
      tab.webView.progress != 100 || tab.generation < 1 ||
      !allowedHttps(tab.currentUrl) || tab.webView.url != tab.currentUrl) return null
    return tab
  }

  private fun previewStillValid(grant: PreviewGrant): Tab? {
    if (sessionGeneration != grant.session ||
      SystemClock.elapsedRealtime() > grant.expiresAt) return null
    val tab = visiblePreviewTab(grant.tabId) ?: return null
    if (tab.generation != grant.pageGeneration || tab.currentUrl != grant.url) return null
    return tab
  }

  private class BoundedPreviewPng : ByteArrayOutputStream(8192) {
    override fun write(b: Int) {
      if (size() >= MAX_PREVIEW_PNG_BYTES) throw IOException("PNG limit reached")
      super.write(b)
    }
    override fun write(b: ByteArray, off: Int, len: Int) {
      if (len < 0 || len > MAX_PREVIEW_PNG_BYTES - size()) throw IOException("PNG limit reached")
      super.write(b, off, len)
    }
  }

  /**
   * User explicitly selects the visible owned tab; token only lives in RAM,
   * expires after one minute and binds tab, URL and two generations.
   */
  @Command
  fun preparePreview(invoke: Invoke) {
    val args = try { invoke.parseArgs(PrivateBrowserPreviewPrepareArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid private browser preview request"); return }
    if (!args.userInitiated || args.tabId < 1) {
      invoke.reject("Private browser preview requires explicit user initiation")
      return
    }
    ui(invoke) {
      if (pendingPreviewConsent) {
        invoke.reject("Private browser preview consent is already pending")
        return@ui
      }
      val tab = visiblePreviewTab(args.tabId)
      if (tab == null || previewDimensions(tab.webView.width, tab.webView.height) == null) {
        invoke.reject("Selected private browser tab is not visible or ready")
        return@ui
      }
      val token = UUID.randomUUID().toString()
      val grant = PreviewGrant(token, tab.id, sessionGeneration, tab.generation,
        tab.currentUrl, SystemClock.elapsedRealtime() + PREVIEW_TOKEN_LIFETIME_MS)
      previewGrant = grant
      invoke.resolve(JSObject().apply {
        put("tabId", tab.id)
        put("previewToken", token)
        put("pageGeneration", grant.pageGeneration)
        put("expiresInMs", PREVIEW_TOKEN_LIFETIME_MS)
        put("maxDimension", MAX_PREVIEW_DIMENSION)
        put("maxPngBytes", MAX_PREVIEW_PNG_BYTES)
      })
    }
  }

  /** Capture WebView.draw only; never the Activity, Android screen, or Chrome. */
  private fun drawOwnedPreview(tab: Tab, grant: PreviewGrant): JSObject {
    // Called exactly once on the UI thread after the native Allow-once dialog.
    if (previewStillValid(grant) !== tab) throw SecurityException("Selected tab changed")
    val (width, height) = previewDimensions(tab.webView.width, tab.webView.height)
      ?: throw SecurityException("Invalid private browser size")
    val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
    val png = try {
      val canvas = Canvas(bitmap)
      canvas.drawColor(Color.WHITE)
      canvas.scale(width.toFloat() / tab.webView.width, height.toFloat() / tab.webView.height)
      tab.webView.draw(canvas)
      val bytes = BoundedPreviewPng().use { output ->
        if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, output))
          throw IOException("Cannot encode private browser preview")
        output.toByteArray()
      }
      if (bytes.isEmpty() || bytes.size > MAX_PREVIEW_PNG_BYTES)
        throw IOException("Private browser preview too large")
      bytes
    } finally {
      bitmap.recycle()
    }
    if (previewStillValid(grant) !== tab) throw SecurityException("Private browser page changed")
    return JSObject().apply {
      put("captured", true)
      put("tabId", tab.id)
      put("pageGeneration", grant.pageGeneration)
      put("mimeType", "image/png")
      put("width", width)
      put("height", height)
      put("pngBytes", png.size)
      put("base64", Base64.encodeToString(png, Base64.NO_WRAP))
      put("previewTokenConsumed", true)
    }
  }

  /** Native Allow/Deny prompt gates every one-shot preview, token is consumed
   * before prompting and never replaced on denial or a stale-window failure. */
  @Command
  fun capturePreview(invoke: Invoke) {
    val args = try { invoke.parseArgs(PrivateBrowserPreviewCaptureArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid private browser preview token"); return }
    if (!args.userInitiated || args.tabId < 1 || args.previewToken.length != 36) {
      invoke.reject("Private browser preview requires explicit user initiation and a token")
      return
    }
    ui(invoke) {
      if (pendingPreviewConsent) {
        invoke.reject("Private browser preview consent is already pending")
        return@ui
      }
      val grant = previewGrant
      previewGrant = null // One attempt per user-issued token, even if denied.
      if (grant == null || grant.token != args.previewToken || grant.tabId != args.tabId ||
        previewStillValid(grant) == null) {
        invoke.reject("Private browser preview token expired or selected tab changed")
        return@ui
      }
      pendingPreviewConsent = true
      try {
        var approved = false
        val host = Uri.parse(grant.url).host ?: "selected HTTPS tab"
        AlertDialog.Builder(activity)
          .setTitle("Preview this browser tab once?")
          .setMessage("Capture only the visible $host tab in this private browser and return one PNG to the app? The page may contain sensitive information. Nothing is saved to storage.")
          .setPositiveButton("Allow once") { _, _ -> approved = true }
          .setNegativeButton("Deny") { _, _ -> }
          .setOnDismissListener {
            if (!approved) {
              pendingPreviewConsent = false
              invoke.resolve(JSObject().apply {
                put("captured", false)
                put("cancelled", true)
                put("previewTokenConsumed", true)
              })
            } else {
              // Capture only after the consent dialog dismisses; no background
              // worker, periodic polling, MediaProjection or device screenshot.
              val selected = tabs[grant.tabId]
              if (selected == null) {
                pendingPreviewConsent = false
                invoke.reject("Private browser preview tab closed")
              } else {
                activity.window.decorView.postDelayed({
                  try {
                    invoke.resolve(drawOwnedPreview(selected, grant))
                  } catch (_: Exception) {
                    invoke.reject("Private browser preview stale or unavailable")
                  } finally {
                    pendingPreviewConsent = false
                  }
                }, 100L)
              }
            }
          }
          .show()
      } catch (_: Exception) {
        pendingPreviewConsent = false
        invoke.reject("Private browser preview consent unavailable")
      }
    }
  }

  private val snapshotJavaScript = """
    (function(){
      if(location.protocol!=='https:') return JSON.stringify({error:'non_https'});
      const xs=[];
      const q=document.querySelectorAll('a,button,input,textarea,select,[role="button"],[contenteditable="true"]');
      function path(e){
        const s=[];let n=e;
        while(n && n.nodeType===1){
          let i=1,p=n.previousElementSibling;
          while(p){if(p.tagName===n.tagName)i++;p=p.previousElementSibling;}
          s.unshift(n.tagName.toLowerCase()+':nth-of-type('+i+')');n=n.parentElement;
        }
        return s.join(' > ');
      }
      function sig(e){return [e.tagName,e.getAttribute('type')||'',e.getAttribute('role')||'',
        e.id||'',e.getAttribute('name')||'',(e.innerText||'').slice(0,60)].join('|').slice(0,220);}
      for(const e of q){
        if(xs.length>=64)break;
        if(e.getAttribute('type')==='password'||e.getAttribute('type')==='file')continue;
        if(!e.getClientRects().length)continue;
        const selector=path(e);if(selector.length>700)continue;
        const fillable=(e.tagName==='TEXTAREA'||
          (e.tagName==='INPUT'&&['text','search','url','email','tel','number',''].includes((e.type||'').toLowerCase()))||
          e.getAttribute('contenteditable')==='true');
        xs.push({index:xs.length+1,selector:selector,signature:sig(e),fillable:fillable,
          tag:e.tagName.toLowerCase(),role:(e.getAttribute('role')||'').slice(0,40),
          label:(e.getAttribute('aria-label')||e.innerText||e.getAttribute('placeholder')||'').slice(0,140)});
      }
      return JSON.stringify({url:location.href,title:document.title.slice(0,150),
        text:(document.body?document.body.innerText:'').slice(0,6000),elements:xs});
    })();
  """.trimIndent()

  private fun evaluate(tab: Tab, script: String, callback: (JSONObject?) -> Unit) {
    tab.webView.evaluateJavascript(script) { raw ->
      val obj = try {
        val unwrapped = JSONTokener(raw ?: "").nextValue() as? String
        if (unwrapped == null || unwrapped.length > 40000) null else JSONObject(unwrapped)
      } catch (_: Exception) { null }
      callback(obj)
    }
  }

  private fun automationTab(invoke: Invoke, id: Int): Tab? {
    if (!automationAllowed || !drawerVisible || dialog?.isShowing != true) {
      invoke.reject("Private browser automation requires user consent")
      return null
    }
    val tab = tabs[id]
    if (tab == null || id != currentId || !allowedHttps(tab.currentUrl) || tab.webView.progress < 100) {
      invoke.reject("Private browser tab is not active and ready")
      return null
    }
    return tab
  }

  private fun indexedSnapshot(invoke: Invoke, args: PrivateBrowserActionArgs): Pair<Tab, Pair<Snapshot, Node>>? {
    if (args.tabId < 1 || args.snapshotId < 1 || args.index !in 1..MAX_ELEMENTS ||
      args.value.isNotEmpty() || args.deltaY != 0) {
      invoke.reject("Invalid indexed private browser request")
      return null
    }
    if (pendingPreviewConsent) {
      invoke.reject("Private browser preview consent is pending")
      return null
    }
    val tab = automationTab(invoke, args.tabId) ?: return null
    // A previously selected/closed browser cannot inspect or hover another
    // WebView, the Tauri WebView, an offscreen tab, or another browser profile.
    if (!supportsIndependentProfile() || sessionProfileName == null ||
      tab.webView.parent !== viewport || !tab.webView.isShown ||
      !tab.webView.isAttachedToWindow) {
      invoke.reject("Private browser tab is no longer visible or owned")
      return null
    }
    val snap = tab.snapshot
    if (snap == null || snap.id != args.snapshotId || snap.generation != tab.generation ||
      snap.url != tab.currentUrl || tab.webView.url != snap.url) {
      invoke.reject("Stale private browser snapshot; capture a new one")
      return null
    }
    val node = snap.nodes[args.index]
    if (node == null) {
      invoke.reject("Unknown indexed private browser element")
      return null
    }
    return Pair(tab, Pair(snap, node))
  }

  // Only safe metadata is returned. No input value, arbitrary attribute list,
  // URL, innerHTML, cookies, tokens, or hidden/password node text.
  private val inspectTags = setOf("a", "button", "input", "textarea", "select",
    "div", "span", "label", "option")
  private val inspectRoles = setOf("", "button", "link", "textbox", "searchbox",
    "checkbox", "radio", "switch", "combobox", "listbox", "option", "menuitem",
    "tab", "slider", "spinbutton")
  private val inspectTypes = setOf("", "text", "search", "url", "email",
    "tel", "number", "checkbox", "radio", "button", "submit", "reset")
  private val inspectSensitive = Regex(
    "(?i)(password|passcode|passphrase|\\bpin\\b|\\botp\\b|token|secret|bearer|api.?key|auth(orization)?|cookie|session|credential|credit.?card|cvv|cvc|verification.?code|[A-Za-z0-9_/-]{18,}|[0-9]{6,}|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,})"
  )
  private fun safeInspectLabel(raw: String): String {
    if (raw.length > 160 || raw.any { Character.isISOControl(it) } ||
      raw.toByteArray(Charsets.UTF_8).size > 240 || inspectSensitive.containsMatchIn(raw)) return ""
    return raw.trim().take(120)
  }

  /** Read only the indexed, same-generation visible element. Inspection
   * does NOT consume the snapshot; actions (including hover) still do. */
  @Command
  fun inspect(invoke: Invoke) {
    val args = parseAction(invoke) ?: return
    ui(invoke) {
      val (tab, details) = indexedSnapshot(invoke, args) ?: return@ui
      val (snap, node) = details
      val session = sessionGeneration
      val script = """
        (function(){
          const sel=${JSONObject.quote(node.selector)}, expected=${JSONObject.quote(node.signature)};
          if(location.protocol!=='https:' || location.href!==${JSONObject.quote(snap.url)})
            return JSON.stringify({ok:false});
          const e=document.querySelector(sel);
          if(!e || e.closest('[hidden],[inert],[aria-hidden="true"]') ||
            e.getAttribute('type')==='password' || e.getAttribute('type')==='hidden' ||
            e.getAttribute('type')==='file')
            return JSON.stringify({ok:false});
          const rect=e.getBoundingClientRect(), style=getComputedStyle(e);
          if(!e.getClientRects().length || rect.width<=0 || rect.height<=0 ||
             rect.bottom<=0 || rect.right<=0 || rect.top>=innerHeight || rect.left>=innerWidth ||
             style.display==='none' || style.visibility==='hidden' || style.visibility==='collapse')
            return JSON.stringify({ok:false});
          const sig=[e.tagName,e.getAttribute('type')||'',e.getAttribute('role')||'',
            e.id||'',e.getAttribute('name')||'',(e.innerText||'').slice(0,60)].join('|').slice(0,220);
          if(sig!==expected) return JSON.stringify({ok:false});
          const tag=e.tagName.toLowerCase();
          const types=['','text','search','url','email','tel','number','checkbox','radio','button','submit','reset'];
          const type=(e.getAttribute('type')||'').toLowerCase();
          const roles=['','button','link','textbox','searchbox','checkbox','radio',
            'switch','combobox','listbox','option','menuitem','tab','slider','spinbutton'];
          const role=(e.getAttribute('role')||'').toLowerCase();
          if(!['a','button','input','textarea','select','div','span','label','option'].includes(tag) ||
             !types.includes(type) || !roles.includes(role)) return JSON.stringify({ok:false});
          // A label comes only from explicit accessibility hints, or visible
          // button/link text. Never read an input's value, or textContent.
          const candidate=e.getAttribute('aria-label') || e.getAttribute('placeholder') ||
            (tag==='button' || tag==='a' ? e.innerText : '') || '';
          const label=typeof candidate==='string' ? candidate.slice(0,161) : '';
          return JSON.stringify({ok:true,tag:tag,role:role,label:label,
            attributes:{
              type:type,disabled:!!e.disabled,readOnly:!!e.readOnly,
              required:!!e.required,
              ariaExpanded:e.getAttribute('aria-expanded')==='true',
              ariaPressed:e.getAttribute('aria-pressed')==='true',
              ariaChecked:e.getAttribute('aria-checked')==='true'
            }});
        })();
      """.trimIndent()
      evaluate(tab, script) { result ->
        if (result?.optBoolean("ok") != true || result.toString().length > 2048 ||
          sessionGeneration != session || !automationAllowed || !drawerVisible || dialog?.isShowing != true ||
          tabs[tab.id] !== tab || currentId != tab.id || tab.snapshot !== snap ||
          tab.generation != snap.generation || tab.currentUrl != snap.url ||
          tab.webView.url != snap.url) {
          invoke.reject("Private browser inspection failed or snapshot changed")
          return@evaluate
        }
        val tag = result.optString("tag")
        val role = result.optString("role")
        val label = result.optString("label")
        val attributes = result.optJSONObject("attributes")
        val type = attributes?.optString("type") ?: ""
        if (tag !in inspectTags || role !in inspectRoles || type !in inspectTypes ||
          attributes == null || result.optString("label").length > 160 ||
          listOf("disabled","readOnly","required","ariaExpanded","ariaPressed","ariaChecked")
            .any { attributes.opt(it) !is Boolean }) {
          invoke.reject("Private browser inspection returned invalid metadata")
          return@evaluate
        }
        invoke.resolve(JSObject().apply {
          put("ok", true)
          put("tabId", tab.id)
          put("snapshotId", snap.id)
          put("index", args.index)
          put("tag", tag)
          put("role", role)
          put("label", safeInspectLabel(label))
          put("attributes", JSONObject().apply {
            put("type", type)
            for (key in listOf("disabled","readOnly","required","ariaExpanded","ariaPressed","ariaChecked"))
              put(key, attributes.getBoolean(key))
          })
          put("snapshotInvalidated", false)
        })
      }
    }
  }

  @Command
  fun snapshot(invoke: Invoke) {
    val args = parseTab(invoke) ?: return
    ui(invoke) {
      val tab = automationTab(invoke, args.tabId) ?: return@ui
      val generation = tab.generation
      val currentUrl = tab.currentUrl
      tab.snapshot = null
      evaluate(tab, snapshotJavaScript) { obj ->
        if (obj == null || generation != tab.generation || currentUrl != tab.currentUrl ||
          obj.optString("url") != currentUrl || obj.has("error")) {
          invoke.reject("Private browser snapshot is stale or unavailable")
          return@evaluate
        }
        val elements = obj.optJSONArray("elements") ?: JSONArray()
        if (elements.length() > MAX_ELEMENTS) {
          invoke.reject("Private browser snapshot exceeds limit")
          return@evaluate
        }
        val nodes = HashMap<Int, Node>()
        val visible = JSONArray()
        for (i in 0 until elements.length()) {
          val element = elements.optJSONObject(i) ?: continue
          val index = element.optInt("index")
          val selector = element.optString("selector")
          val signature = element.optString("signature")
          if (index != i + 1 || selector.length > 700 || signature.length > 220 || selector.isEmpty()) continue
          nodes[index] = Node(selector, signature, element.optBoolean("fillable"))
          visible.put(JSONObject().apply {
            put("index", index)
            put("tag", element.optString("tag").take(40))
            put("role", element.optString("role").take(40))
            put("label", element.optString("label").take(140))
            put("fillable", element.optBoolean("fillable"))
          })
        }
        val id = nextSnapshotId++
        tab.snapshot = Snapshot(id, generation, currentUrl, nodes)
        invoke.resolve(JSObject().apply {
          put("tabId", tab.id)
          put("snapshotId", id)
          put("url", currentUrl)
          put("title", obj.optString("title").take(150))
          put("text", obj.optString("text").take(MAX_TEXT))
          put("elements", visible)
        })
      }
    }
  }

  private fun runAction(invoke: Invoke, kind: String) {
    val args = parseAction(invoke) ?: return
    if (args.index < 0 || args.index > MAX_ELEMENTS || args.snapshotId <= 0 ||
      args.value.length > MAX_VALUE || args.deltaY !in -2000..2000 ||
      (kind != "scroll" && args.index == 0) ||
      (kind == "hover" && (args.value.isNotEmpty() || args.deltaY != 0))) {
      invoke.reject("Invalid private browser action parameters")
      return
    }
    ui(invoke) {
      if (pendingPreviewConsent) {
        invoke.reject("Private browser preview consent is pending")
        return@ui
      }
      previewGrant = null
      if (kind == "hover") {
        // Apply the stronger exact-tab and snapshot validation used by inspect.
        val pair = indexedSnapshot(invoke, args) ?: return@ui
        if (pair.first.snapshot !== pair.second.first) {
          invoke.reject("Stale private browser hover snapshot")
          return@ui
        }
      }
      val tab = automationTab(invoke, args.tabId) ?: return@ui
      val snap = tab.snapshot
      if (snap == null || snap.id != args.snapshotId || snap.generation != tab.generation ||
        snap.url != tab.currentUrl) {
        invoke.reject("Stale snapshot; capture a new one")
        return@ui
      }
      // Click/fill/scroll must enforce the same owned, visible WebView boundary
      // as inspect and hover before mutating any indexed page element.
      if (!supportsIndependentProfile() || sessionProfileName == null ||
        tab.webView.parent !== viewport || !tab.webView.isShown ||
        !tab.webView.isAttachedToWindow || tab.webView.url != snap.url) {
        invoke.reject("Private browser action target is no longer visible or owned")
        return@ui
      }
      val node = if (args.index == 0 && kind == "scroll") null else snap.nodes[args.index]
      if (args.index != 0 && node == null) { invoke.reject("Unknown snapshot element"); return@ui }
      if (kind == "fill" && node?.fillable != true) {
        invoke.reject("Snapshot element is not an editable text field")
        return@ui
      }
      val selector = JSONObject.quote(node?.selector ?: "")
      val signature = JSONObject.quote(node?.signature ?: "")
      val value = JSONObject.quote(args.value)
      val mode = JSONObject.quote(kind)
      // A new evaluation locates by full nth-of-type DOM path and verifies the
      // snapshot fingerprint, URL, and visibility before changing anything.
      val session = sessionGeneration
      val script = """
        (function(){
          const mode=$mode, sel=$selector, expected=$signature, text=$value;
          if(location.protocol!=='https:' || location.href!==${JSONObject.quote(snap.url)})
            return JSON.stringify({ok:false});
          if(mode==='scroll' && ${args.index}===0){window.scrollBy(0,${args.deltaY});return JSON.stringify({ok:true});}
          const e=document.querySelector(sel);
          if(!e || e.closest('[hidden],[inert],[aria-hidden="true"]') ||
            ['password','hidden','file'].includes(e.getAttribute('type')) ||
            !e.getClientRects().length) return JSON.stringify({ok:false});
          const rect=e.getBoundingClientRect(),style=getComputedStyle(e);
          if(rect.width<=0 || rect.height<=0 || rect.bottom<=0 || rect.right<=0 ||
            rect.top>=innerHeight || rect.left>=innerWidth ||
            style.display==='none' || style.visibility!=='visible')
            return JSON.stringify({ok:false});
          const sig=[e.tagName,e.getAttribute('type')||'',e.getAttribute('role')||'',
            e.id||'',e.getAttribute('name')||'',(e.innerText||'').slice(0,60)].join('|').slice(0,220);
          if(sig!==expected)return JSON.stringify({ok:false});
          if(mode==='hover'){
            e.dispatchEvent(new MouseEvent('mouseover',{bubbles:true,cancelable:true,view:window}));
            e.dispatchEvent(new MouseEvent('mouseenter',{bubbles:false,cancelable:false,view:window}));
            return JSON.stringify({ok:true,syntheticEvents:true});
          }else if(mode==='click'){
            if(e.disabled||e.getAttribute('type')==='password'||e.getAttribute('type')==='file')return JSON.stringify({ok:false});
            const a=e.closest('a');if(a && a.href && !a.href.startsWith('https://'))return JSON.stringify({ok:false});
            e.click();
          }else if(mode==='fill'){
            if(e.disabled||e.readOnly||e.getAttribute('type')==='password'||e.getAttribute('type')==='file')return JSON.stringify({ok:false});
            if(e.isContentEditable)e.textContent=text;
            else {if(!('value' in e))return JSON.stringify({ok:false});e.value=text;}
            e.dispatchEvent(new Event('input',{bubbles:true}));e.dispatchEvent(new Event('change',{bubbles:true}));
          }else if(mode==='scroll'){e.scrollBy(0,${args.deltaY});}
          else return JSON.stringify({ok:false});
          return JSON.stringify({ok:true});
        })();
      """.trimIndent()
      tab.snapshot = null
      evaluate(tab, script) { obj ->
        if (obj?.optBoolean("ok") != true) {
          invoke.reject("Private browser action was not confirmed; capture a fresh snapshot")
        } else if (snap.generation != tab.generation ||
          snap.url != tab.currentUrl || session != sessionGeneration ||
          currentId != tab.id || tabs[tab.id] !== tab ||
          !automationAllowed || !drawerVisible || dialog?.isShowing != true ||
          tab.webView.parent !== viewport || !tab.webView.isShown ||
          !tab.webView.isAttachedToWindow || tab.webView.url != snap.url ||
          (kind == "hover" && obj.optBoolean("syntheticEvents") != true)) {
          // A click can navigate before its callback. Never claim nonexecution
          // or automatically replay a possibly effective browser action.
          invoke.reject("Private browser page changed; action outcome uncertain (do not auto-retry)")
        } else {
          invoke.resolve(JSObject().apply {
            put("ok", true)
            put("tabId", args.tabId)
            put("snapshotInvalidated", true)
            if (kind == "hover") {
              put("syntheticEvents", true)
              put("physicalPointerMoved", false)
            }
          })
        }
      }
    }
  }

  @Command fun click(invoke: Invoke) = runAction(invoke, "click")
  @Command fun fill(invoke: Invoke) = runAction(invoke, "fill")
  @Command fun scroll(invoke: Invoke) = runAction(invoke, "scroll")
  @Command fun hover(invoke: Invoke) = runAction(invoke, "hover")
}
