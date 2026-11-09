package ai.mcpagent.app

import android.app.Activity
import android.app.AlertDialog
import android.app.Application
import android.content.Intent
import android.graphics.Bitmap
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.view.ViewGroup
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.webkit.SslErrorHandler
import android.net.http.SslError
import android.widget.FrameLayout
import androidx.webkit.ProfileStore
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import java.util.UUID

@InvokeArg
class BackgroundBrowserStartArgs {
  var url: String = ""
  var userInitiated: Boolean = false
  // Set only by the native agent host for the current, explicitly sent chat task.
  var chatAuthorized: Boolean = false
}

@InvokeArg
class BackgroundBrowserActionArgs {
  var snapshotId: Long = 0
  var index: Int = 0
  var value: String = ""
  var deltaY: Int = 0
  var userInitiated: Boolean = false
}

@InvokeArg
class ExternalChromeOpenArgs {
  var userInitiated: Boolean = false
}

/**
 * One isolated, offscreen, app-owned HTTPS WebView. No personal Chrome window,
 * no shared ChatGPT profile or cookies, and no JavaScript bridge. Native DOM
 * commands are small, indexed, one-shot operations requiring a fresh snapshot.
 * Android may pause or kill the process while the app is in the background;
 * the runtime never claims guaranteed unattended execution.
 */
@TauriPlugin
class BackgroundBrowserAutomationPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val MAX_URL = 2048
    private const val MAX_ELEMENTS = 48
    private const val MAX_VALUE = 2048
    private const val CHROME_PACKAGE = "com.android.chrome"
    internal fun allowedHttps(raw: String): Boolean {
      if (raw.length !in 9..MAX_URL || raw.any { it.isWhitespace() || Character.isISOControl(it) }) return false
      return try {
        val uri = Uri.parse(raw)
        uri.isHierarchical && uri.scheme.equals("https", true) &&
          !uri.host.isNullOrBlank() && uri.userInfo.isNullOrEmpty() && uri.port != 0
      } catch (_: Exception) { false }
    }
  }

  private data class Node(val selector: String, val signature: String, val fillable: Boolean)
  private var container: FrameLayout? = null
  private var browser: WebView? = null
  private var profile: String? = null
  private var currentUrl = ""
  private var pageTitle = ""
  private var ready = false
  private var pageGeneration = 0L
  private var snapshotGeneration = 0L
  private var nextSnapshot = 1L
  private var snapshotId = 0L
  private var snapshotUrl = ""
  private var nodes = emptyMap<Int, Node>()
  private var pendingConsent = false
  private var destroyed = false
  private var errorCode = ""
  private var lastCleanup = "not_attempted"
  private val cleanupHandler = Handler(Looper.getMainLooper())
  private val pendingProfileCleanup = LinkedHashMap<String, Int>()
  private val cleanupPrefs by lazy {
    activity.getSharedPreferences("mcp_background_browser_cleanup_v1", Activity.MODE_PRIVATE)
  }
  private fun isOwnedProfile(name: String): Boolean =
    name.startsWith("mcp_background_") && name.length == "mcp_background_".length + 32 &&
      name.drop("mcp_background_".length).all { it in '0'..'9' || it in 'a'..'f' }
  private fun persistCleanupQueue() {
    cleanupPrefs.edit().putStringSet("profiles", pendingProfileCleanup.keys.toSet()).apply()
  }
  private fun resumeCleanupOnStartup() {
    // Discover sessions created by older app versions that did not persist
    // their pending deletion. Never inspect or delete the Default/ChatGPT
    // profiles: only our exact, randomly named background-browser prefix.
    val discovered = try {
      if (WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE))
        ProfileStore.getInstance().allProfileNames.filter(::isOwnedProfile)
      else emptyList()
    } catch (_: Exception) { emptyList() }
    val profiles = (cleanupPrefs.getStringSet("profiles", emptySet()).orEmpty() + discovered)
      .filter(::isOwnedProfile).distinct().take(32)
    if (profiles.isEmpty()) return
    for (name in profiles) pendingProfileCleanup[name] = 0
    lastCleanup = "profile_cleanup_pending"
    cleanupHandler.post { for (name in profiles) retryProfileCleanup(name) }
  }

  // A just-destroyed WebView can remain associated with its Profile until
  // Chromium processes the destruction callback. Deleting synchronously from
  // stopBrowser() reports IllegalStateException on some Android WebViews.
  // Retry on the main looper, never silently claim the profile was deleted.
  private fun retryProfileCleanup(name: String) {
    val attempt = pendingProfileCleanup[name] ?: return
    try {
      val existed = WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE) &&
        ProfileStore.getInstance().deleteProfile(name)
      pendingProfileCleanup.remove(name)
      persistCleanupQueue()
      lastCleanup = if (existed) "requested_async_deletion" else "profile_not_present"
    } catch (_: IllegalStateException) {
      if (attempt >= 8) {
        // Store the profile name for a fresh process, when no live WebView can
        // still refer to it. Never mark the deletion as completed prematurely.
        pendingProfileCleanup[name] = 9
        persistCleanupQueue()
        lastCleanup = "profile_cleanup_deferred_until_restart"
      } else {
        pendingProfileCleanup[name] = attempt + 1
        lastCleanup = "profile_cleanup_pending"
        cleanupHandler.postDelayed({ retryProfileCleanup(name) },
          (250L shl attempt.coerceAtMost(4)))
      }
    } catch (_: Exception) {
      pendingProfileCleanup[name] = 9
      persistCleanupQueue()
      lastCleanup = "profile_cleanup_deferred_until_restart"
    }
  }

  private fun supported(): Boolean = try {
    !destroyed && WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)
  } catch (_: Exception) { false }

  /** Only an actual, explicitly addressed Chrome launcher is accepted.
   * The app's manifest already queries ACTION_MAIN / CATEGORY_LAUNCHER.
   * This never queries Chrome cookies/tabs or uses a URL to target a browser. */
  private fun verifiedChromeLauncher(): Intent? = try {
    activity.packageManager.getLaunchIntentForPackage(CHROME_PACKAGE)
      ?.takeIf { intent ->
        val component = intent.component
        component != null && component.packageName == CHROME_PACKAGE &&
          intent.action == Intent.ACTION_MAIN &&
          intent.hasCategory(Intent.CATEGORY_LAUNCHER)
      }
  } catch (_: Exception) { null }

  private fun chromeForeground(): Boolean =
    !destroyed && !activity.isFinishing && !activity.isDestroyed && activity.hasWindowFocus()

  private fun externalChromeStatus(): JSObject {
    val available = verifiedChromeLauncher() != null
    val foreground = chromeForeground()
    return JSObject().apply {
      put("mode", "external_chrome")
      put("packageName", CHROME_PACKAGE)
      put("supported", true) // Android can dispatch explicitly addressed activities.
      put("available", available) // Only when the OS confirms this exact launcher.
      put("foreground", foreground)
      put("canLaunch", available && foreground)
      put("launchOnly", true)
      put("domControl", false)
      put("automation", false)
      put("tabsReadable", false)
      put("pageReadyVerified", false)
      put("reason", when {
        !available -> "chrome_launcher_not_resolved"
        !foreground -> "app_not_foreground"
        else -> "ready_for_user_initiated_launch"
      })
    }
  }

  private fun onUi(invoke: Invoke, action: () -> Unit) {
    activity.runOnUiThread {
      try { action() }
      catch (_: Exception) { invoke.reject("Offscreen browser operation unavailable") }
    }
  }

  private fun clearSnapshot() {
    nodes = emptyMap()
    snapshotId = 0L
    snapshotUrl = ""
    snapshotGeneration = 0L
  }

  private fun status(): JSObject = JSObject().apply {
    put("supported", supported())
    put("running", browser != null)
    put("ready", ready && browser != null && allowedHttps(currentUrl))
    put("offscreen", browser != null)
    put("profileIsolated", browser != null && profile != null)
    put("url", currentUrl.take(MAX_URL))
    put("title", pageTitle.take(150))
    put("pageGeneration", pageGeneration)
    put("lastCleanup", lastCleanup)
    put("reason", errorCode)
    put("mode", "headless_webview")
    put("realtime", true)
    put("domControl", supported()) // Only supported isolated WebView, with approved DOM actions.
    put("chromeControlled", false)
  }

  /** Read-only Chrome launcher capability probe; never launches any Activity. */
  @Command
  fun externalChromeStatus(invoke: Invoke) = onUi(invoke) {
    invoke.resolve(externalChromeStatus())
  }

  /** Open personal Chrome through its exact OS launcher. No URL, cookies,
   * WebView debugging, DOM bridge, or inferred Chrome page readiness. */
  @Command
  fun openExternalChrome(invoke: Invoke) {
    val args = try { invoke.parseArgs(ExternalChromeOpenArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid Chrome launch request"); return }
    if (!args.userInitiated) {
      invoke.reject("Opening external Chrome requires an explicit user action")
      return
    }
    onUi(invoke) {
      if (!chromeForeground()) {
        invoke.reject("Opening Chrome requires the foreground Android app")
        return@onUi
      }
      val launch = verifiedChromeLauncher()
      if (launch == null) {
        invoke.reject("The official Chrome app launcher was not verified")
        return@onUi
      }
      try {
        // Preserve the OS-resolved explicit Chrome component. Never fall back
        // to a generic VIEW intent or an app chooser, which could open a
        // different browser or navigate to an untrusted URL.
        activity.startActivity(launch)
        invoke.resolve(JSObject().apply {
          put("dispatched", true)
          put("mode", "external_chrome")
          put("packageName", CHROME_PACKAGE)
          put("domControl", false)
          put("automation", false)
          put("pageReadyVerified", false)
        })
      } catch (_: Exception) {
        invoke.reject("Chrome launcher dispatch was not confirmed")
      }
    }
  }

  private fun stopBrowser() {
    clearSnapshot()
    pageGeneration++
    ready = false
    currentUrl = ""
    pageTitle = ""
    errorCode = ""
    val old = browser
    browser = null
    if (old != null) {
      runCatching { old.stopLoading() }
      // Never queue a new navigation just before destroying the WebView.
      runCatching { container?.removeView(old) }
      runCatching { old.webViewClient = WebViewClient() }
      runCatching { old.destroy() }
    }
    val root = container
    container = null
    if (root != null) runCatching { (root.parent as? ViewGroup)?.removeView(root) }
    val oldProfile = profile
    profile = null
    if (oldProfile != null) {
      pendingProfileCleanup[oldProfile] = 0
      persistCleanupQueue()
      lastCleanup = "profile_cleanup_pending"
      cleanupHandler.postDelayed({ retryProfileCleanup(oldProfile) }, 250L)
    }
  }

  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, state: Bundle?) = Unit
    override fun onActivityStarted(a: Activity) = Unit
    override fun onActivityResumed(a: Activity) = Unit
    override fun onActivityPaused(a: Activity) = Unit
    override fun onActivityStopped(a: Activity) = Unit
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) = Unit
    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        destroyed = true
        stopBrowser()
        a.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }

  init {
    activity.application.registerActivityLifecycleCallbacks(lifecycle)
    activity.runOnUiThread { resumeCleanupOnStartup() }
  }

  @Command
  fun status(invoke: Invoke) = onUi(invoke) { invoke.resolve(status()) }

  @Command
  fun start(invoke: Invoke) {
    val args = try { invoke.parseArgs(BackgroundBrowserStartArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid browser request"); return }
    if (!args.userInitiated || !allowedHttps(args.url)) {
      invoke.reject("Explicit user approval and HTTPS URL required")
      return
    }
    onUi(invoke) {
      if (!supported() || activity.isFinishing || activity.isDestroyed ||
          (!activity.hasWindowFocus() && !args.chatAuthorized) ||
          (browser != null && !args.chatAuthorized) || pendingConsent) {
        invoke.reject("Offscreen browser unavailable or already running")
        return@onUi
      }
      if (args.chatAuthorized) {
        // An authenticated tool invocation or the user's Send instruction
        // can run with the Activity paused, using the app-owned offscreen
        // WebView (never Chrome). The Activity still must exist; Android may
        // suspend/destroy it in the background, in which case we fail closed.
        // Never show a dialog in a non-foreground Activity.
        try {
          // A newer link supplied by the user replaces the old isolated tab.
          // The previous WebView is destroyed and its owned profile queued for
          // deletion before the replacement is created.
          if (browser != null) stopBrowser()
          createBrowser(args.url)
          invoke.resolve(status())
        }
        catch (_: Exception) {
          stopBrowser(); invoke.reject("Unable to start isolated offscreen browser")
        }
        return@onUi
      }
      pendingConsent = true
      val host = Uri.parse(args.url).host.orEmpty()
      AlertDialog.Builder(activity)
        .setTitle("Start background browser?")
        .setMessage("Mcp Agent Ai will load $host in its own offscreen WebView. JavaScript and limited browser DOM actions are enabled for this session. No Chrome window opens. Android may suspend this browser after the app is backgrounded. Stop closes it.")
        .setPositiveButton("Start") { _, _ ->
          pendingConsent = false
          try { createBrowser(args.url); invoke.resolve(status()) }
          catch (_: Exception) { stopBrowser(); invoke.reject("Unable to start isolated offscreen browser") }
        }
        .setNegativeButton("Cancel") { _, _ ->
          pendingConsent = false
          invoke.resolve(status())
        }
        .setOnCancelListener {
          if (pendingConsent) {
            pendingConsent = false
            invoke.resolve(status())
          }
        }.show()
    }
  }

  private fun createBrowser(url: String) {
    if (!supported() || browser != null || pendingProfileCleanup.size >= 32)
      throw IllegalStateException("Profile or browser unavailable")
    val owned = "mcp_background_${UUID.randomUUID().toString().replace("-", "")}" 
    val frame = FrameLayout(activity).apply {
      alpha = 0f
      isClickable = false
      importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO_HIDE_DESCENDANTS
    }
    val web = WebView(activity)
    try {
      WebViewCompat.setProfile(web, owned)
      profile = owned
      pendingProfileCleanup[owned] = 0
      persistCleanupQueue() // Recover profile even if Android kills the process before Stop.
      web.settings.apply {
        javaScriptEnabled = true
        domStorageEnabled = false
        allowFileAccess = false
        allowContentAccess = false
        javaScriptCanOpenWindowsAutomatically = false
        setSupportMultipleWindows(false)
        mediaPlaybackRequiresUserGesture = true
        loadsImagesAutomatically = false
        @Suppress("DEPRECATION")
        allowFileAccessFromFileURLs = false
        @Suppress("DEPRECATION")
        allowUniversalAccessFromFileURLs = false
      }
      web.webViewClient = object : WebViewClient() {
        override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest?): Boolean =
          !allowedHttps(request?.url?.toString().orEmpty())
        @Deprecated("Old WebView providers may use the URL overload")
        override fun shouldOverrideUrlLoading(view: WebView?, address: String?): Boolean =
          !allowedHttps(address.orEmpty())
        override fun onPageStarted(view: WebView?, address: String?, favicon: Bitmap?) {
          if (view !== browser) return
          if (!allowedHttps(address.orEmpty())) {
            errorCode = "blocked_non_https_navigation"
            view?.stopLoading()
            ready = false
            clearSnapshot()
            return
          }
          currentUrl = address.orEmpty()
          pageGeneration++
          ready = false
          clearSnapshot()
        }
        override fun onPageFinished(view: WebView?, address: String?) {
          if (view !== browser || !allowedHttps(address.orEmpty())) return
          currentUrl = address.orEmpty()
          pageTitle = view?.title.orEmpty().take(150)
          ready = true
          errorCode = ""
        }
        override fun onReceivedError(view: WebView?, request: WebResourceRequest?, error: WebResourceError?) {
          if (view === browser && request?.isForMainFrame == true) {
            ready = false
            errorCode = "navigation_failed"
            clearSnapshot()
          }
        }
        override fun onReceivedHttpError(view: WebView?, request: WebResourceRequest?, response: WebResourceResponse?) {
          if (view === browser && request?.isForMainFrame == true) errorCode = "http_error"
        }
        override fun onReceivedSslError(view: WebView?, handler: SslErrorHandler?, error: SslError?) {
          handler?.cancel()
          if (view === browser) { ready = false; errorCode = "ssl_error"; clearSnapshot() }
        }
      }
      frame.addView(web, FrameLayout.LayoutParams(320, 480))
      activity.addContentView(frame, ViewGroup.LayoutParams(1, 1))
      container = frame
      browser = web
      currentUrl = url
      ready = false
      pageGeneration++
      web.loadUrl(url)
    } catch (error: Exception) {
      runCatching { frame.removeView(web) }
      runCatching { web.destroy() }
      runCatching { (frame.parent as? ViewGroup)?.removeView(frame) }
      throw error
    }
  }

  @Command
  fun stop(invoke: Invoke) = onUi(invoke) {
    pendingConsent = false
    stopBrowser()
    invoke.resolve(status())
  }

  private val snapshotJs = """
    (function(){
      if(location.protocol!=='https:') return JSON.stringify({error:'non_https'});
      const list=[],items=document.querySelectorAll('a,button,input,textarea,select,[role="button"]');
      function path(e){let names=[],x=e;while(x&&x.nodeType===1){let i=1,p=x.previousElementSibling;
        while(p){if(p.tagName===x.tagName)i++;p=p.previousElementSibling;}
        names.unshift(x.tagName.toLowerCase()+':nth-of-type('+i+')');x=x.parentElement;}
        return names.join(' > ');}
      for(const el of items){if(list.length>=48)break;
        if(['password','file','hidden'].includes((el.type||'').toLowerCase()))continue;
        const selector=path(el);if(selector.length>700)continue;
        const signature=(el.tagName+'|'+(el.type||'')+'|'+(el.id||'')+'|'+(el.getAttribute('name')||'')).slice(0,180);
        const fillable=el.tagName==='TEXTAREA'||(el.tagName==='INPUT'&&
          ['text','search','email','tel','url','number',''].includes((el.type||'').toLowerCase()));
        const label=(el.getAttribute('aria-label')||el.innerText||el.getAttribute('placeholder')||'').slice(0,100);
        list.push({index:list.length+1,selector,signature,tag:el.tagName.toLowerCase(),label,fillable});}
      return JSON.stringify({url:location.href,title:(document.title||'').slice(0,150),
        text:(document.body?.innerText||'').slice(0,3000),elements:list});
    })();
  """.trimIndent()

  @Command
  fun snapshot(invoke: Invoke) = onUi(invoke) {
    val web = browser
    if (web == null || !ready || !allowedHttps(currentUrl)) {
      invoke.reject("Background browser page not ready")
      return@onUi
    }
    val generation = pageGeneration
    val url = currentUrl
    web.evaluateJavascript(snapshotJs) { raw ->
      try {
        val unwrapped = JSONTokener(raw.orEmpty()).nextValue() as? String
        if (unwrapped == null || unwrapped.length > 26000) throw IllegalArgumentException()
        val result = JSONObject(unwrapped)
        if (browser !== web || generation != pageGeneration || url != currentUrl ||
            result.optString("url") != url) throw IllegalStateException()
        val elements = result.optJSONArray("elements") ?: throw IllegalArgumentException()
        if (elements.length() > MAX_ELEMENTS) throw IllegalArgumentException()
        val nextNodes = LinkedHashMap<Int, Node>()
        val safeElements = JSONArray()
        for (n in 0 until elements.length()) {
          val element = elements.getJSONObject(n)
          val index = n + 1
          val selector = element.optString("selector")
          val signature = element.optString("signature")
          if (selector.length !in 1..700 || signature.length > 180) throw IllegalArgumentException()
          nextNodes[index] = Node(selector,signature,element.optBoolean("fillable"))
          safeElements.put(JSONObject().put("index",index)
            .put("tag",element.optString("tag").take(20))
            .put("label",element.optString("label").take(100))
            .put("fillable",element.optBoolean("fillable")))
        }
        val id = nextSnapshot++
        nodes = nextNodes
        snapshotId = id
        snapshotUrl = url
        snapshotGeneration = generation
        invoke.resolve(JSObject().apply {
          put("snapshotId",id)
          put("url",url)
          put("title",result.optString("title").take(150))
          put("text",result.optString("text").take(3000))
          put("elements",safeElements)
        })
      } catch (_: Exception) {
        clearSnapshot()
        invoke.reject("Background browser snapshot stale or unavailable")
      }
    }
  }

  private fun runIndexed(invoke: Invoke, mode: String) {
    val args = try { invoke.parseArgs(BackgroundBrowserActionArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid browser action"); return }
    if (!args.userInitiated || args.snapshotId <= 0 || args.index !in 1..MAX_ELEMENTS ||
        args.value.length > MAX_VALUE || args.value.any { Character.isISOControl(it) } ||
        args.deltaY !in -2000..2000) {
      invoke.reject("Browser action requires a fresh approved snapshot")
      return
    }
    onUi(invoke) {
      val web = browser
      val node = nodes[args.index]
      if (web == null || !ready || node == null || snapshotId != args.snapshotId ||
          snapshotGeneration != pageGeneration || currentUrl != snapshotUrl ||
          (mode == "fill" && !node.fillable)) {
        invoke.reject("Browser snapshot expired, refresh before acting")
        return@onUi
      }
      clearSnapshot() // Each attempted action consumes the snapshot, including failed ones.
      val selector = JSONObject.quote(node.selector)
      val signature = JSONObject.quote(node.signature)
      val value = JSONObject.quote(args.value)
      val script = """
        (function(){
          if(location.protocol!=='https:')return JSON.stringify({ok:false});
          const el=document.querySelector($selector);
          if(!el)return JSON.stringify({ok:false});
          const actual=(el.tagName+'|'+(el.type||'')+'|'+(el.id||'')+'|'+(el.getAttribute('name')||'')).slice(0,180);
          if(actual!==$signature || ['password','file','hidden'].includes((el.type||'').toLowerCase()))return JSON.stringify({ok:false});
          if('$mode'==='click'){el.click();}
          if('$mode'==='fill'){el.value=$value;el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));}
          if('$mode'==='scroll'){el.scrollBy(0,${args.deltaY});}
          return JSON.stringify({ok:true});
        })();
      """.trimIndent()
      val generation = pageGeneration
      web.evaluateJavascript(script) { raw ->
        val ok = try { (JSONTokener(raw.orEmpty()).nextValue() as? String)?.let {
          JSONObject(it).optBoolean("ok",false) } ?: false } catch (_: Exception) { false }
        if (!ok || browser !== web || pageGeneration != generation) {
          invoke.reject("Browser action failed or page changed")
        } else invoke.resolve(JSObject().apply { put("ok",true); put("snapshotConsumed",true) })
      }
    }
  }

  @Command fun click(invoke: Invoke) = runIndexed(invoke,"click")
  @Command fun fill(invoke: Invoke) = runIndexed(invoke,"fill")
  @Command fun scroll(invoke: Invoke) = runIndexed(invoke,"scroll")
}
