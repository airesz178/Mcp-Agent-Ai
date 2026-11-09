package ai.mcpagent.app

import android.app.Activity
import android.app.AlertDialog
import android.app.Dialog
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.net.Uri
import android.view.ViewGroup
import android.view.Window
import android.view.WindowManager
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.mozilla.geckoview.AllowOrDeny
import org.mozilla.geckoview.GeckoResult
import org.mozilla.geckoview.GeckoRuntime
import org.mozilla.geckoview.GeckoSession
import org.mozilla.geckoview.GeckoView
import org.mozilla.geckoview.WebExtension
import org.mozilla.geckoview.WebExtensionController

@InvokeArg
class CustomGeckoOpenArgs {
  var url: String = ""
  var userInitiated: Boolean = false
}

@InvokeArg
class CustomGeckoExtensionArgs {
  var id: String = ""
  var enabled: Boolean = false
  var userInitiated: Boolean = false
}

/**
 * A separate, opt-in GeckoView browser. This NEVER replaces the main Tauri
 * WebView or the existing private/ChatGPT WebView profile. It does not attach
 * to personal Chrome, import cookies, execute headless tasks or auto-launch.
 *
 * Chrome CRX/unpacked folders are NOT compatible install packages here.
 * The Settings manager installs only an app-bundled WebExtension probe.
 * Already-installed add-ons can still be listed, enabled, disabled or removed.
 */
@TauriPlugin
class CustomGeckoBrowserPlugin(private val activity: Activity) : Plugin(activity) {
  private var runtime: GeckoRuntime? = null
  private var browser: GeckoSession? = null
  private var web: GeckoView? = null
  private var dialog: Dialog? = null
  private var address: EditText? = null
  private var currentUrl: String = ""
  private var pendingOpen: Boolean = false
  private var pendingInstall: Boolean = false
  private val demoExtensionId = "mcp-agent-browser-probe@example.com"
  private val demoExtensionUri = "resource://android/assets/mcp_extension_probe/"

  private fun onUi(invoke: Invoke, block: () -> Unit) {
    activity.runOnUiThread {
      try { block() } catch (_: Exception) { invoke.reject("Custom Gecko browser operation failed") }
    }
  }

  private fun validHttps(input: String): Boolean = try {
    val url = Uri.parse(input)
    input.length in 9..2048 && url.scheme.equals("https", true) &&
      !url.host.isNullOrBlank() && url.userInfo == null &&
      (url.port == -1 || url.port == 443) && input.none { it.isWhitespace() || it.isISOControl() }
  } catch (_: Exception) { false }

  private fun statusJson(): JSObject = JSObject().apply {
    put("engine", "GeckoView")
    put("supported", true)
    put("running", browser != null && dialog?.isShowing == true)
    put("visible", dialog?.isShowing == true)
    put("extensionsSupported", true)
    put("unpackedChromeSupported", false)
    put("extensionFormats", JSONArray().put("bundled-webextension"))
    put("url", currentUrl.take(2048))
    put("message", "GeckoView is independent of Android Chrome and WebView. Only the bundled test extension can be installed from Settings; there is no unrestricted headless mode.")
  }

  private fun installPrompts(r: GeckoRuntime) {
    r.webExtensionController.setPromptDelegate(object : WebExtensionController.PromptDelegate {
      override fun onInstallPromptRequest(
        extension: WebExtension,
        permissions: Array<String>,
        origins: Array<String>,
        dataCollectionPermissions: Array<String>
      ): GeckoResult<WebExtension.PermissionPromptResponse> {
        val answer = GeckoResult<WebExtension.PermissionPromptResponse>()
        val detail = buildString {
          append("Extension: ").append(extension.metaData.name ?: extension.id)
          append("\n\nRequested permissions:\n")
          append(permissions.joinToString(", ").ifBlank { "None" }.take(1024))
          append("\n\nWebsites:\n")
          append(origins.joinToString("\n").ifBlank { "None" }.take(2048))
          append("\n\nData collection requests:\n")
          append(dataCollectionPermissions.joinToString(", ").ifBlank { "None" }.take(512))
          append("\n\nOnly approve extensions you trust. The extension may access matching page content.")
        }
        val deny = { answer.complete(WebExtension.PermissionPromptResponse(false, false, false)) }
        AlertDialog.Builder(activity)
          .setTitle("Review GeckoView extension permissions")
          .setMessage(detail)
          .setPositiveButton("Install & allow") { _, _ ->
            // Private-browsing permission and optional telemetry stay disabled.
            answer.complete(WebExtension.PermissionPromptResponse(true, false, false))
          }
          .setNegativeButton("Reject") { _, _ -> deny() }
          .setOnCancelListener { deny() }
          .show()
        return answer
      }

      override fun onOptionalPrompt(extension: WebExtension, permissions: Array<String>,
        origins: Array<String>, dataCollectionPermissions: Array<String>): GeckoResult<AllowOrDeny> =
        GeckoResult.deny() // No unreviewed permission escalation.

      override fun onUpdatePrompt(extension: WebExtension, newPermissions: Array<String>,
        newOrigins: Array<String>, newDataCollectionPermissions: Array<String>): GeckoResult<AllowOrDeny> =
        GeckoResult.deny() // Updates that increase access need a separate review flow.
    })
  }

  private fun runtime(): GeckoRuntime {
    runtime?.let { return it }
    val created = GeckoRuntime.create(activity)
    installPrompts(created)
    runtime = created
    return created
  }

  private fun px(dp: Int): Int = (dp * activity.resources.displayMetrics.density + 0.5f).toInt()

  private fun newButton(label: String, click: () -> Unit) = Button(activity).apply {
    text = label
    isAllCaps = false
    minWidth = 0
    minimumWidth = 0
    minHeight = 0
    minimumHeight = 0
    setTextColor(Color.rgb(233, 239, 250))
    textSize = 12f
    background = GradientDrawable().apply {
      setColor(Color.rgb(35, 49, 68))
      cornerRadius = px(9).toFloat()
      setStroke(px(1), Color.rgb(73, 90, 112))
    }
    setOnClickListener { click() }
  }
  private fun addActionRow(root: LinearLayout, vararg buttons: Button) {
    val row = LinearLayout(activity).apply {
      orientation = LinearLayout.HORIZONTAL
      setPadding(px(8), px(5), px(8), 0)
    }
    buttons.forEachIndexed { index, button ->
      row.addView(button, LinearLayout.LayoutParams(0, px(44), 1f).apply {
        if (index != buttons.lastIndex) rightMargin = px(5)
      })
    }
    root.addView(row)
  }
  private fun navigate(url: String) {
    if (!validHttps(url)) {
      android.widget.Toast.makeText(activity, "HTTPS only", android.widget.Toast.LENGTH_SHORT).show()
      return
    }
    browser?.loadUri(url)
  }

  private fun closeBrowser() {
    dialog = null
    address = null
    web?.releaseSession()
    web = null
    browser?.close()
    browser = null
    currentUrl = ""
    // GeckoRuntime (and installed extension data) is intentionally retained;
    // a browser tab close is not permission to delete extension/user data.
  }

  private fun openBrowser(url: String) {
    if (dialog != null || browser != null) error("Browser already active")
    val r = runtime()
    val session = GeckoSession()
    session.navigationDelegate = object : GeckoSession.NavigationDelegate {
      override fun onLoadRequest(session: GeckoSession,
        request: GeckoSession.NavigationDelegate.LoadRequest): GeckoResult<AllowOrDeny> {
        // Reject file://, javascript:, HTTP and browser-privileged URIs.
        return if (validHttps(request.uri)) GeckoResult.allow() else GeckoResult.deny()
      }
      override fun onLocationChange(session: GeckoSession, url: String?,
        perms: List<GeckoSession.PermissionDelegate.ContentPermission>, hasUserGesture: Boolean) {
        if (url != null && validHttps(url)) {
          currentUrl = url
          address?.setText(url)
        }
      }
    }
    session.open(r)
    browser = session
    val view = GeckoView(activity).apply { setSession(session) }
    web = view
    val root = LinearLayout(activity).apply {
      orientation = LinearLayout.VERTICAL
      setBackgroundColor(Color.rgb(16, 24, 36))
    }
    // Weighted, two-row controls guarantee that Add-ons and Done stay
    // visible on narrow Android phones (no horizontal toolbar clipping).
    addActionRow(root,
      newButton("Back") { session.goBack() },
      newButton("Next") { session.goForward() },
      newButton("Reload") { session.reload() })
    addActionRow(root,
      newButton("Add-ons") { showExtensionManager() },
      newButton("Done") { dialog?.dismiss() })
    val inputRow = LinearLayout(activity).apply {
      orientation = LinearLayout.HORIZONTAL
      setPadding(px(8), px(5), px(8), px(5))
    }
    address = EditText(activity).apply {
      setSingleLine(true)
      setText(url)
      setTextColor(Color.rgb(235, 241, 250))
      setHintTextColor(Color.rgb(163, 180, 202))
      setPadding(px(11), 0, px(11), 0)
      textSize = 13f
      background = GradientDrawable().apply {
        setColor(Color.rgb(28, 39, 54))
        cornerRadius = px(9).toFloat()
        setStroke(px(1), Color.rgb(73, 90, 112))
      }
      inputType = android.text.InputType.TYPE_CLASS_TEXT or android.text.InputType.TYPE_TEXT_VARIATION_URI
      imeOptions = android.view.inputmethod.EditorInfo.IME_ACTION_GO
      setOnEditorActionListener { _, _, _ ->
        navigate(text?.toString()?.trim() ?: "")
        true
      }
    }
    inputRow.addView(address, LinearLayout.LayoutParams(0, px(44), 1f).apply {
      rightMargin = px(6)
    })
    inputRow.addView(newButton("Go") { navigate(address?.text?.toString()?.trim() ?: "") },
      LinearLayout.LayoutParams(px(65), px(44)))
    root.addView(inputRow)
    root.addView(view, LinearLayout.LayoutParams(
      ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
    val sheet = Dialog(activity)
    sheet.requestWindowFeature(Window.FEATURE_NO_TITLE)
    sheet.setContentView(root)
    sheet.setOnDismissListener { closeBrowser() }
    dialog = sheet
    sheet.show()
    sheet.window?.setLayout(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT)
    sheet.window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
    navigate(url)
  }

  private fun showExtensionManager() {
    val r = runtime ?: return
    r.webExtensionController.list().accept({ addons ->
      val installed = addons ?: emptyList()
      val lines = if (installed.isEmpty()) "No extensions installed." else installed
        .joinToString("\n") { a -> "${a.metaData.name ?: a.id} — ${if (a.metaData.enabled) "Enabled" else "Disabled"}" }
      AlertDialog.Builder(activity)
        .setTitle("GeckoView extensions")
        .setMessage(lines + "\n\nOnly the bundled test extension can be installed from Settings. No Chrome CRX, XPI import or arbitrary unpacked folder support.")
        .setPositiveButton("OK", null)
        .show()
    }, {
      android.widget.Toast.makeText(activity, "Extension list unavailable", android.widget.Toast.LENGTH_SHORT).show()
    })
  }

  @Command
  fun status(invoke: Invoke) = onUi(invoke) { invoke.resolve(statusJson()) }

  @Command
  fun open(invoke: Invoke) {
    val args = try { invoke.parseArgs(CustomGeckoOpenArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid custom browser request"); return }
    onUi(invoke) {
      if (!args.userInitiated || !validHttps(args.url)) {
        invoke.reject("A user action and HTTPS URL are required"); return@onUi
      }
      if (pendingOpen || browser != null) {
        invoke.reject("One custom GeckoView browser at a time"); return@onUi
      }
      pendingOpen = true
      AlertDialog.Builder(activity)
        .setTitle("Open GeckoView Custom Browser?")
        .setMessage("Open an isolated Firefox/GeckoView engine in Mcp Agent Ai? It has its own browsing data and extension permissions. No Chrome profile or account will be imported. It runs only while Android permits.")
        .setPositiveButton("Open") { _, _ ->
          pendingOpen = false
          try { openBrowser(args.url); invoke.resolve(statusJson()) }
          catch (_: Exception) { closeBrowser(); invoke.reject("GeckoView could not start") }
        }
        .setNegativeButton("Cancel") { _, _ -> pendingOpen = false; invoke.resolve(statusJson()) }
        .setOnCancelListener { pendingOpen = false; invoke.resolve(statusJson()) }
        .show()
    }
  }

  @Command
  fun show(invoke: Invoke) = onUi(invoke) {
    val window = dialog
    if (window == null || browser == null) { invoke.reject("GeckoView is stopped"); return@onUi }
    if (!window.isShowing) window.show()
    invoke.resolve(statusJson())
  }

  @Command
  fun close(invoke: Invoke) = onUi(invoke) {
    dialog?.dismiss()
    if (dialog == null && browser != null) closeBrowser()
    invoke.resolve(statusJson())
  }

  @Command
  fun listExtensions(invoke: Invoke) = onUi(invoke) {
    // Read the persisted GeckoView profile even before a browser tab has ever
    // been opened in this process. A null runtime is NOT evidence that nothing
    // is installed. Creating the runtime does not navigate or install anything.
    val r = try { runtime() } catch (_: Exception) {
      invoke.reject("Unable to inspect installed GeckoView extensions")
      return@onUi
    }
    r.webExtensionController.list().accept({ addons ->
      val list = JSONArray()
      (addons ?: emptyList()).forEach { addon ->
        list.put(JSONObjectExtension(addon))
      }
      invoke.resolve(JSObject().apply { put("extensions", list); put("runtimeStarted", true) })
    }, { invoke.reject("Unable to list GeckoView WebExtensions") })
  }

  private fun JSONObjectExtension(addon: WebExtension) = org.json.JSONObject().apply {
    put("id", addon.id.take(200))
    put("name", (addon.metaData.name ?: addon.id).take(150))
    put("version", addon.metaData.version.take(50))
    put("enabled", addon.metaData.enabled)
    put("builtIn", addon.isBuiltIn)
    put("requiredPermissions", JSONArray(addon.metaData.requiredPermissions.take(64)))
    put("requiredOrigins", JSONArray(addon.metaData.requiredOrigins.take(64)))
  }

  @Command
  fun installProbe(invoke: Invoke) = onUi(invoke) {
    // The probe is an original, no-site-access WebExtension bundled with this
    // APK. It is only installed on an explicit user click after consent.
    // Settings is not interactive while the full-screen browser dialog is
    // visible; installing from the STOPPED state is intentional and necessary.
    // The app-owned GeckoRuntime persists without starting a browser tab.
    if (browser != null || pendingInstall) {
      invoke.reject("Close GeckoView before reviewing or installing an extension")
      return@onUi
    }
    pendingInstall = true
    AlertDialog.Builder(activity)
      .setTitle("Install original GeckoView probe?")
      .setMessage("Install an app-bundled test WebExtension with no website access? This validates GeckoView extension loading only. It does not add Chrome extension support.")
      .setPositiveButton("Install") { _, _ ->
        try {
          runtime().webExtensionController.ensureBuiltIn(demoExtensionUri, demoExtensionId)
            .accept({ addon ->
            pendingInstall = false
            if (addon == null) {
              invoke.reject("Bundled GeckoView extension was not returned")
            } else invoke.resolve(JSObject().apply {
              put("installed", true); put("id", addon.id); put("name", addon.metaData.name ?: addon.id)
            })
          }, {
            pendingInstall = false
            invoke.reject("Bundled GeckoView WebExtension installation failed")
          })
        } catch (_: Exception) {
          pendingInstall = false
          invoke.reject("Unable to initialize app-owned GeckoView WebExtension runtime")
        }
      }
      .setNegativeButton("Cancel") { _, _ ->
        pendingInstall = false
        invoke.resolve(JSObject().apply { put("installed", false) })
      }
      .setOnCancelListener {
        pendingInstall = false
        invoke.resolve(JSObject().apply { put("installed", false) })
      }
      .show()
  }

  private fun changeAddon(invoke: Invoke, op: String) {
    val args = try { invoke.parseArgs(CustomGeckoExtensionArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid extension management request"); return }
    onUi(invoke) {
      if (!args.userInitiated || args.id.length !in 3..200 || pendingInstall) {
        invoke.reject("Invalid user-approved extension action"); return@onUi
      }
      val r = runtime ?: run { invoke.reject("GeckoView runtime has not started"); return@onUi }
      r.webExtensionController.list().accept({ addons ->
        val addon = addons?.firstOrNull { it.id == args.id }
        if (addon == null) { invoke.reject("Extension is no longer installed"); return@accept }
        val action = when (op) {
          "enable" -> "Enable"
          "disable" -> "Disable"
          else -> "Remove"
        }
        AlertDialog.Builder(activity)
          .setTitle("$action GeckoView extension?")
          .setMessage("${addon.metaData.name ?: addon.id}\n\n${addon.id}\n\nThis action affects only the app-owned GeckoView profile.")
          .setPositiveButton(action) { _, _ ->
            val outcome = when (op) {
              "enable" -> r.webExtensionController.enable(addon, WebExtensionController.EnableSource.USER)
              "disable" -> r.webExtensionController.disable(addon, WebExtensionController.EnableSource.USER)
              else -> r.webExtensionController.uninstall(addon)
            }
            outcome.accept({ invoke.resolve(JSObject().apply { put("updated", true); put("id", addon.id) }) },
              { invoke.reject("GeckoView extension change failed") })
          }
          .setNegativeButton("Cancel") { _, _ ->
            invoke.resolve(JSObject().apply { put("updated", false) })
          }
          .setOnCancelListener { invoke.resolve(JSObject().apply { put("updated", false) }) }
          .show()
      }, { invoke.reject("Could not find installed GeckoView extension") })
    }
  }

  @Command
  fun enableExtension(invoke: Invoke) = changeAddon(invoke, "enable")
  @Command
  fun disableExtension(invoke: Invoke) = changeAddon(invoke, "disable")
  @Command
  fun removeExtension(invoke: Invoke) = changeAddon(invoke, "remove")
}
