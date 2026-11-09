package ai.mcpagent.app

import android.app.Activity
import android.app.AlertDialog
import android.app.Dialog
import android.content.ActivityNotFoundException
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.net.http.SslError
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.view.View
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.GeolocationPermissions
import android.webkit.PermissionRequest
import android.webkit.SslErrorHandler
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import org.json.JSONTokener
import org.json.JSONObject
import java.util.UUID
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import androidx.browser.customtabs.CustomTabsIntent
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class ChatGptOpenArgs {
  var userInitiated: Boolean = false
}

@InvokeArg
class ChatGptBridgeArgs {
  var requestId: String = ""
  var prompt: String = ""
  var userInitiated: Boolean = false
  var expectedConversationId: String? = null
}

/**
 * App-owned ChatGPT website, separate from the Tauri UI and personal Chrome.
 *
 * The explicit bridge below uses a bounded, app-owned DOM lease. It does not
 * inspect browser credentials, import Chrome state, or install a JS interface.
 * The WebView may keep loading while the user chats in this foreground app,
 * but Android can suspend or kill the process when it moves to the background.
 * No Android service, wake lock or Accessibility permission is requested.
 */
@TauriPlugin
class BackgroundChatGptPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val PROFILE = "mcp_chatgpt_builtin_v1"
    private const val HOME = "https://chatgpt.com/"
  }
  private var browser: WebView? = null
  private var authPopup: WebView? = null
  private var browserPages: FrameLayout? = null
  private var lastExternalAuthOpen: Long = 0L
  private var window: Dialog? = null
  private var browserNotice: TextView? = null
  private var signInGuidance: String? = null
  private var googleGuideShowing = false
  private var visible = false
  private var pageGeneration: Long = 0
  private var probePending = false
  // A consented browser handoff is only a recent proof for this one live
  // WebView/page generation, not a transferable Chrome cookie or an AI reply.
  private var sessionCheckPending = false
  private var linkedPageGeneration: Long = -1
  private var linkedAtElapsed: Long = 0
  // A linked session is an evidence lease, refreshed only by a real same-origin
  // account response. A timer alone never means the user signed out.
  private var lastSessionCheckAtElapsed: Long = 0L
  private fun sessionLinked(): Boolean = browser != null &&
    linkedPageGeneration == pageGeneration && linkedAtElapsed > 0 &&
    android.os.SystemClock.elapsedRealtime() - linkedAtElapsed < 5 * 60_000L
  // Step 3 uses two actions only. The native, app-owned browser checks the
  // live website session itself while the user completes manual sign-in.
  // This is short-lived evidence, not a cookie export or GeckoView pairing.
  private var signInPageGeneration: Long = -1
  private var signInAtElapsed: Long = 0
  private fun signInVerified(): Boolean = browser != null && !visible &&
    signInPageGeneration == pageGeneration && signInAtElapsed > 0 &&
    android.os.SystemClock.elapsedRealtime() - signInAtElapsed < 2 * 60_000L
  private var autoSignInActive = false
  private var autoSignInDeadline: Long = 0L
  private val signInHandler = Handler(Looper.getMainLooper())
  private val autoSignInCheck = object : Runnable {
    override fun run() {
      if (!autoSignInActive || browser == null || !visible) return
      if (android.os.SystemClock.elapsedRealtime() >= autoSignInDeadline) {
        autoSignInActive = false
        return // No verified login; user can retry with Sign in to ChatGPT.
      }
      if (!sessionCheckPending && !probePending && lease == null &&
          isChatGptHome(browser?.url)) {
        verifySignIn(null, false)
      } else scheduleAutoSignInCheck()
    }
  }
  private fun scheduleAutoSignInCheck(delay: Long = 3500L) {
    signInHandler.removeCallbacks(autoSignInCheck)
    if (autoSignInActive && browser != null && visible)
      signInHandler.postDelayed(autoSignInCheck, delay)
  }
  private fun startAutoSignInCheck() {
    autoSignInActive = true
    autoSignInDeadline = android.os.SystemClock.elapsedRealtime() + 10 * 60_000L
    scheduleAutoSignInCheck(2800)
  }
  private fun refreshHiddenSessionIfDue() {
    val web = browser ?: return
    if (visible || !isChatGptHome(web.url) || sessionCheckPending || probePending || lease != null)
      return
    val now = android.os.SystemClock.elapsedRealtime()
    if (lastSessionCheckAtElapsed > 0L && now - lastSessionCheckAtElapsed < 45_000L)
      return
    lastSessionCheckAtElapsed = now
    verifySignIn(null, false)
  }
  private data class TurnLease(
    val requestId: String,
    val prompt: String,
    val nonce: String,
    val generation: Long,
    val initialPath: String,
    val expectedConversationId: String?,
    var state: String = "preparing",
    var accepted: Boolean = false,
    var completed: Boolean = false,
    var answer: String? = null,
    var attempted: Boolean = false,
    var operation: Boolean = false,
    var candidate: String? = null,
    var candidateSince: Long = 0L,
    var conversationId: String? = null
  )
  private var lease: TurnLease? = null
  private var nativeSendConfirmationPending = false
  // A process crash after a click must not let the same request ID be prepared again.
  // Persist only opaque IDs in this app's private preferences, never prompts or replies.
  private val spentIds by lazy {
    activity.getSharedPreferences("mcp_chatgpt_bridge_tombstones_v1", Activity.MODE_PRIVATE)
      .getStringSet("spent", emptySet())?.toMutableSet() ?: mutableSetOf<String>()
  }

  private fun validId(value: String) = value.length == 36 && value.indices.all { index ->
    if (index in listOf(8, 13, 18, 23)) value[index] == '-'
    else value[index] in "0123456789abcdefABCDEF"
  }

  private fun validPrompt(value: String) = value.isNotBlank() && value.length <= 32000 &&
    value.none { it == '\u0000' || (it < ' ' && it != '\n' && it != '\t' && it != '\r') }

  private fun pagePath(web: WebView): String? = try {
    if (!isChatGptHome(web.url)) null else Uri.parse(web.url).path ?: "/"
  } catch (_: Exception) { null }

  private fun bridgeReceipt(turn: TurnLease) = JSObject().apply {
    put("requestId", turn.requestId)
    put("state", turn.state)
    put("accepted", turn.accepted)
    put("completed", turn.completed)
    put("pageGeneration", turn.generation)
    if (turn.accepted && turn.conversationId != null) put("conversationId", turn.conversationId)
    if (turn.completed && turn.answer != null) put("answer", turn.answer)
  }

  private fun failUncertain(turn: TurnLease) {
    turn.state = if (turn.attempted) "uncertain" else "failed_before_send"
    turn.completed = false
    turn.answer = null
  }

  /** All script output is a short state code, except the exact terminal assistant text.
   * The original conversation URL, DOM message ids and transcript stay in the owned WebView.
   * React remounts revoke the composer lease; no automatic send recovery is attempted.
   */
  private fun bridgeScript(action: String, turn: TurnLease): String {
    val input = JSONObject().put("action", action).put("nonce", turn.nonce)
      .put("prompt", turn.prompt).put("initialPath", turn.initialPath)
      .put("expectedConversationId", turn.expectedConversationId ?: JSONObject.NULL)
    return """(function() {
      'use strict';
      const q = ${input};
      const result = (state, extra = {}) => JSON.stringify(Object.assign({state}, extra));
      const compact = value => String(value || '').replace(/\s+/g, ' ').trim();
      const path = () => location.pathname;
      const exactOrigin = () => location.origin === 'https://chatgpt.com' || location.origin === 'https://www.chatgpt.com';
      const allowedPath = value => value === '/' || /^\/(?:g\/[^/]+\/)?c\/[0-9a-f-]{8,64}\/?$/i.test(value);
      const editors = () => {
        const classic = [...document.querySelectorAll('#prompt-textarea')];
        if (classic.length) return classic;
        return [...document.querySelectorAll('form[data-chatgpt-composer] [contenteditable="true"][role="textbox"], form [data-composer-markdown][contenteditable="true"][role="textbox"]')];
      };
      const nativeEditor = () => {
        const all = editors().filter(node => node.isConnected && !node.closest('[hidden],[inert],[aria-hidden="true"],[data-message-author-role],[data-turn-key],.markdown'));
        if (all.length !== 1) return null;
        const editor = all[0], form = editor.closest('form');
        if (!form || editor.getAttribute('aria-disabled') === 'true' || editor.getAttribute('contenteditable') === 'false') return null;
        return {editor,form};
      };
      const text = node => (typeof node.innerText === 'string' ? node.innerText : node.textContent || '').trim();
      const files = form => !!form.querySelector('[data-inline-file-uploading],[role="progressbar"],button[aria-label^="Remove file" i],button[data-testid*="remove-file"],button[aria-label*="Remove attachment" i]') ||
        [...form.querySelectorAll('input[type="file"]')].some(input => input.files?.length);
      const stop = form => !!form.querySelector('button[data-testid="stop-button"],button[data-testid="composer-stop-button"],button[aria-label^="Stop streaming" i],button[aria-label^="Stop generating" i]') ||
        [...form.querySelectorAll('button[class*="size-token-button-composer"][class*="bg-composer-primary"]')].some(b => {
          const paths = b.querySelectorAll('svg path');
          return paths.length === 1 && /^\s*M4\.5 5\.75/.test(paths[0].getAttribute('d') || '');
        });
      const sendControl = form => {
        const labelled = [...form.querySelectorAll('button[data-testid="send-button"],button[aria-label^="Send" i],button[type="submit"]')];
        const candidates = labelled.length ? labelled : [...form.querySelectorAll('button[class*="size-token-button-composer"][class*="bg-composer-primary"]')].filter(b =>
          !b.hasAttribute('data-state') && b.querySelectorAll('svg path').length >= 1 && b.querySelectorAll('svg path').length <= 2 &&
          !/^\s*M4\.5 5\.75/.test(b.querySelector('svg path')?.getAttribute('d') || ''));
        return candidates.length === 1 && candidates[0].closest('form') === form && !candidates[0].disabled && candidates[0].getAttribute('aria-disabled') !== 'true' ? candidates[0] : null;
      };
      const rows = role => {
        const selector = role === 'user' ? '[data-message-author-role="user"][data-message-id],[data-content-search-unit-key$=":user"],[data-chatgpt-search-unit-key$=":user"]' :
          '[data-message-author-role="assistant"][data-message-id],[data-content-search-unit-key$=":assistant"],[data-chatgpt-search-unit-key$=":assistant"]';
        const legacy = [...document.querySelectorAll('section[data-testid^="conversation-turn"] [data-message-id]')].filter(node =>
          node.closest('[data-message-author-role]')?.getAttribute('data-message-author-role') === role);
        const unique = new Map();
        for (const node of [...document.querySelectorAll(selector), ...legacy]) {
          if (!node.isConnected || node.closest('form')) continue;
          const id = node.getAttribute('data-message-id') || node.getAttribute('data-content-search-unit-key') || node.getAttribute('data-chatgpt-search-unit-key');
          if (id && !unique.has(id)) unique.set(id,{node,id});
        }
        return [...unique.values()].sort((a,b) =>
          (a.node.compareDocumentPosition(b.node) & Node.DOCUMENT_POSITION_FOLLOWING) ? -1 : 1);
      };
      const userText = node => {
        const blocks = [...node.querySelectorAll('.whitespace-pre-wrap')].filter(part => !part.parentElement?.closest('.whitespace-pre-wrap'));
        return blocks.length ? blocks.map(block => block.textContent || '').join('\n') : node.textContent || '';
      };
      try {
        if (!exactOrigin() || !allowedPath(path()) || !allowedPath(q.initialPath)) return result('uncertain');
        if (q.action === 'prepare') {
          // Native bound this operation to the selected local conversation before
          // touching the provider's editor. A new app chat only owns the home route.
          if (q.expectedConversationId === null ? q.initialPath !== '/' :
              q.initialPath !== '/c/' + q.expectedConversationId) return result('failed_before_send');
          if (path() !== q.initialPath || window.__mcpOwnedBridge) return result('failed_before_send');
          const pair = nativeEditor();
          if (!pair || text(pair.editor) || files(pair.form) || stop(pair.form) || !pair.editor.isContentEditable) return result('failed_before_send');
          const origin = {nonce:q.nonce,editor:pair.editor,form:pair.form,path:path(),baseline:new Set(rows('user').map(row => row.id)),touched:false,clicked:false,ownerId:null,observedPath:null,navigationTouched:false};
          const touch = event => { if (event.isTrusted) origin.touched = true; };
          const navigationTouch = event => { if (event.isTrusted && !pair.form.contains(event.target)) origin.navigationTouched = true; };
          window.__mcpOwnedBridge = origin;
          pair.editor.focus();
          const selection = document.getSelection();
          if (!selection || !pair.editor.isConnected) return result('failed_before_send');
          selection.selectAllChildren(pair.editor);
          selection.collapseToEnd();
          if (!document.execCommand('insertText',false,q.prompt) || compact(text(pair.editor)) !== compact(q.prompt)) return result('failed_before_send');
          // execCommand may raise trusted input events; the lease begins after our insertion.
          for (const kind of ['input','beforeinput','keydown','paste','drop','pointerdown']) pair.form.addEventListener(kind,touch,true);
          document.addEventListener('pointerdown',navigationTouch,true);
          document.addEventListener('keydown',navigationTouch,true);
          window.addEventListener('popstate',() => { origin.navigationTouched = true; });
          return result('prepared');
        }
        const owned = window.__mcpOwnedBridge;
        if (!owned || owned.nonce !== q.nonce || owned.path !== q.initialPath) return result('uncertain');
        const pair = nativeEditor();
        if (q.action === 'cancel') {
          // After click there is no safe undo: preserve any in-flight result and disown it.
          if (!owned.clicked && pair && pair.editor === owned.editor && !owned.touched && !files(pair.form) && compact(text(pair.editor)) === compact(q.prompt)) {
            pair.editor.focus();
            const selection = document.getSelection();
            if (selection) { selection.selectAllChildren(pair.editor); document.execCommand('delete',false); }
          }
          delete window.__mcpOwnedBridge;
          return result(owned.clicked ? 'uncertain' : 'cancelled');
        }
        const route = path();
        if (owned.navigationTouched) return result('uncertain');
        if (route !== q.initialPath && !(owned.clicked && q.initialPath === '/' && /^\/c\/[0-9a-f-]{8,64}\/?$/i.test(route))) return result('uncertain');
        if (owned.observedPath && owned.observedPath !== route) return result('uncertain');
        if (q.action === 'send') {
          if (owned.clicked || !pair || pair.editor !== owned.editor || pair.form !== owned.form || owned.touched || files(pair.form) || stop(pair.form) ||
              route !== q.initialPath || compact(text(pair.editor)) !== compact(q.prompt)) return result('failed_before_send');
          const button = sendControl(pair.form);
          if (!button) return result('failed_before_send');
          owned.clicked = true; // Set before invocation: never click again, even after a JS exception.
          button.click();
          return result('submitted');
        }
        if (q.action === 'observe') {
          if (!owned.clicked) return result('failed_before_send');
          const users = rows('user');
          const own = users.filter(row => !owned.baseline.has(row.id) && compact(userText(row.node)) === compact(q.prompt));
          if (own.length !== 1 || users[users.length-1]?.id !== own[0].id) return result('answering',{accepted:false});
          if (owned.ownerId && owned.ownerId !== own[0].id) return result('uncertain');
          owned.ownerId = own[0].id;
          if (owned.observedPath && owned.observedPath !== route) return result('uncertain');
          owned.observedPath = route;
          const conversationId = /^\/(?:g\/[^/]+\/)?c\/([0-9a-f-]{8,64})\/?$/i.exec(route)?.[1];
          if (!conversationId) return result('answering',{accepted:true});
          const assistants = rows('assistant').filter(row => !!(own[0].node.compareDocumentPosition(row.node) & Node.DOCUMENT_POSITION_FOLLOWING));
          let matching = assistants.filter(row => row.node.querySelector('.markdown,[data-markdown-text-style="assistant-message"]'));
          if (matching.length === 0) {
            // COS observes older assistant turns without message ids. The turn itself
            // must be unique and follow the exact new user row in DOM order.
            const sections = [...document.querySelectorAll('section[data-testid^="conversation-turn"][data-testid]')].filter(node =>
              !!(own[0].node.compareDocumentPosition(node) & Node.DOCUMENT_POSITION_FOLLOWING) &&
              !node.querySelector('[data-message-author-role="user"]') &&
              node.querySelector('.markdown,[data-markdown-text-style="assistant-message"]'));
            matching = sections.length === 1 ? [{node:sections[0], id:sections[0].getAttribute('data-testid')}] : [];
          }
          if (matching.length !== 1) return result('answering',{accepted:true,conversationId});
          const assistant = matching[0].node;
          if (assistant.closest('[data-interrupted]') || assistant.querySelector('[data-interrupted]')) return result('answering',{accepted:true,conversationId});
          const blocks = [...assistant.querySelectorAll('.markdown,[data-markdown-text-style="assistant-message"]')].filter(node => !node.closest('[data-interrupted]'));
          const answer = blocks.map(block => block.textContent || '').filter(Boolean).join('\n\n').trim();
          if (!answer || answer.length > 100000) return result('answering',{accepted:true,conversationId});
          const busy = !!document.querySelector('button[data-testid="stop-button"],button[data-testid="composer-stop-button"],button[aria-label^="Stop generating" i],button[aria-label^="Stop streaming" i]') || (pair && stop(pair.form));
          return busy ? result('answering',{accepted:true,conversationId}) : result('candidate',{accepted:true,conversationId,candidate:answer});
        }
        return result('uncertain');
      } catch (_) { return result('uncertain'); }
    })()""".trimIndent()
  }

  private fun readBridgeResult(raw: String): JSONObject? = try {
    val embedded = JSONTokener(raw).nextValue() as? String
    if (embedded == null || embedded.length > 110000) null else JSONObject(embedded).takeIf {
      it.length() in 1..4 && it.optString("state") in setOf("prepared","submitted","answering","candidate","uncertain","failed_before_send","cancelled") &&
        (!it.has("accepted") || it.opt("accepted") is Boolean) &&
        (!it.has("conversationId") || (it.opt("conversationId") is String && Regex("^[0-9a-fA-F-]{8,64}$").matches(it.optString("conversationId")))) &&
        (!it.has("candidate") || (it.opt("candidate") is String && it.optString("candidate").length <= 100000))
    }
  } catch (_: Exception) { null }

  private fun runBridge(invoke: Invoke, turn: TurnLease, action: String) {
    val web = browser
    if (web == null || turn.operation || pageGeneration != turn.generation || !isChatGptHome(web.url)) {
      failUncertain(turn)
      invoke.resolve(bridgeReceipt(turn))
      return
    }
    turn.operation = true
    if (action == "send") turn.attempted = true
    var settled = false
    val timeout = Runnable {
      if (!settled) {
        settled = true
        turn.operation = false
        failUncertain(turn)
        invoke.resolve(bridgeReceipt(turn))
      }
    }
    val main = Handler(Looper.getMainLooper())
    main.postDelayed(timeout, 15000)
    try {
    web.evaluateJavascript(bridgeScript(action, turn)) { raw ->
      if (settled) return@evaluateJavascript
      settled = true
      main.removeCallbacks(timeout)
      turn.operation = false
      if (lease !== turn || browser !== web || pageGeneration != turn.generation || !isChatGptHome(web.url)) {
        failUncertain(turn)
        invoke.resolve(bridgeReceipt(turn))
        return@evaluateJavascript
      }
      val result = readBridgeResult(raw)
      val state = result?.optString("state") ?: "uncertain"
      val conversation = result?.optString("conversationId")?.takeIf { it.isNotEmpty() }
      if (conversation != null && (turn.conversationId == null || turn.conversationId == conversation)) {
        turn.conversationId = conversation
      } else if (conversation != null) {
        failUncertain(turn)
        invoke.resolve(bridgeReceipt(turn))
        return@evaluateJavascript
      }
      when (action) {
        "prepare" -> turn.state = if (state == "prepared") "prepared" else "failed_before_send"
        "send" -> turn.state = if (state == "submitted") "submitted" else "uncertain"
        "observe" -> {
          if (state == "answering" || state == "candidate") {
            turn.accepted = result?.optBoolean("accepted", false) == true || turn.accepted
            if (state == "candidate" && turn.accepted) {
              val answer = result?.optString("candidate") ?: ""
              if (answer.isNotBlank() && turn.candidate == answer &&
                android.os.SystemClock.elapsedRealtime() - turn.candidateSince >= 2500) {
                turn.answer = answer
                turn.state = "completed"
                turn.completed = true
              } else {
                turn.candidate = answer
                turn.candidateSince = android.os.SystemClock.elapsedRealtime()
                turn.state = "answering"
              }
            } else {
              turn.state = "answering"
              turn.candidate = null
            }
          } else failUncertain(turn)
        }
        "cancel" -> {
          if (state == "cancelled" && !turn.attempted) {
            turn.state = "cancelled"
            turn.accepted = false
            turn.completed = false
            turn.answer = null
            lease = null
          } else if (turn.completed && state == "uncertain") {
            lease = null
          } else failUncertain(turn)
        }
      }
      invoke.resolve(bridgeReceipt(turn))
    }
    } catch (_: Exception) {
      if (!settled) {
        settled = true
        main.removeCallbacks(timeout)
        turn.operation = false
        failUncertain(turn)
        invoke.resolve(bridgeReceipt(turn))
      }
    }
  }

  @Command
  fun prepare(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptBridgeArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid ChatGPT prepare arguments"); return }
    if (!validId(args.requestId) || !validPrompt(args.prompt)) {
      invoke.reject("Invalid ChatGPT request ID or prompt"); return
    }
    ui(invoke) {
      val web = browser
      val path = web?.let(::pagePath)
      val expectedId = args.expectedConversationId
      val bound = if (expectedId == null) path == "/" else
        validId(expectedId) && path == "/c/$expectedId"
      if (web == null || path == null || !bound || lease != null || args.requestId in spentIds || probePending) {
        invoke.reject("Owned ChatGPT browser unavailable or another request is active"); return@ui
      }
      val turn = TurnLease(args.requestId, args.prompt, UUID.randomUUID().toString(), pageGeneration, path, expectedId)
      val persisted = spentIds.toMutableSet().apply { add(turn.requestId) }
      if (!activity.getSharedPreferences("mcp_chatgpt_bridge_tombstones_v1", Activity.MODE_PRIVATE)
          .edit().putStringSet("spent", persisted).commit()) {
        invoke.reject("Could not durably reserve ChatGPT request ID"); return@ui
      }
      lease = turn
      spentIds.add(turn.requestId)
      runBridge(invoke, turn, "prepare")
    }
  }

  @Command
  fun send(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptBridgeArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid ChatGPT send arguments"); return }
    if (!validId(args.requestId) || !args.userInitiated) {
      invoke.reject("ChatGPT send requires a reviewed explicit user action"); return
    }
    ui(invoke) {
      val turn = lease
      if (turn == null || turn.requestId != args.requestId || turn.state != "prepared" ||
          turn.operation || turn.attempted || nativeSendConfirmationPending) {
        invoke.reject("ChatGPT request is not prepared or send was already attempted"); return@ui
      }
      nativeSendConfirmationPending = true
      // A native Android dialog is independent of the JS renderer; a forged
      // userInitiated:true parameter alone cannot authorize a remote Send.
      try {
        AlertDialog.Builder(activity)
          .setTitle("Confirm ChatGPT website Send")
          .setMessage("Send this prepared message ONCE to the signed-in ChatGPT website? Remote delivery cannot be undone. No automatic retry.")
          .setPositiveButton("Send once") { _, _ ->
            nativeSendConfirmationPending = false
            if (lease !== turn || turn.state != "prepared" || turn.attempted || turn.operation) {
              invoke.reject("ChatGPT draft changed during native Send confirmation")
            } else runBridge(invoke, turn, "send")
          }
          .setNegativeButton("Do not send") { _, _ ->
            nativeSendConfirmationPending = false
            if (lease === turn && turn.state == "prepared" && !turn.attempted)
              runBridge(invoke, turn, "cancel")
            else invoke.reject("ChatGPT draft changed during cancellation; outcome not verified")
          }
          .setOnCancelListener {
            nativeSendConfirmationPending = false
            if (lease === turn && turn.state == "prepared" && !turn.attempted)
              runBridge(invoke, turn, "cancel")
            else invoke.reject("ChatGPT confirmation was dismissed; outcome not verified")
          }
          .show()
      } catch (_: Exception) {
        nativeSendConfirmationPending = false
        invoke.reject("Native ChatGPT Send confirmation unavailable; no Send authorized")
      }
    }
  }

  @Command
  fun observe(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptBridgeArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid ChatGPT observe arguments"); return }
    ui(invoke) {
      val turn = lease
      if (!validId(args.requestId) || turn == null || turn.requestId != args.requestId || turn.operation) {
        invoke.reject("ChatGPT observation has no exclusive active request"); return@ui
      }
      // A lost Send or Observe callback can leave the exact in-page lease
      // uncertain after the one allowed click. Re-observe that same lease
      // read-only; runBridge still rejects a changed document/generation.
      // Never re-enter Send or treat an unsent preparation as deliverable.
      if (turn.completed || (turn.state == "uncertain" && !turn.attempted) ||
          turn.state == "failed_before_send" || turn.state == "prepared") {
        invoke.resolve(bridgeReceipt(turn))
      } else runBridge(invoke, turn, "observe")
    }
  }

  @Command
  fun cancel(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptBridgeArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid ChatGPT cancel arguments"); return }
    ui(invoke) {
      val turn = lease
      if (!validId(args.requestId) || turn == null || turn.requestId != args.requestId || turn.operation) {
        invoke.reject("ChatGPT cancellation has no exclusive active request"); return@ui
      }
      runBridge(invoke, turn, "cancel")
      // Future requests require explicit reconciliation through stop/reopen; IDs never replay.
    }
  }

  private fun independentProfile() = try {
    WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)
  } catch (_: Exception) { false }

  private fun appleAuthentication(url: String?): Boolean = try {
    val uri = Uri.parse(url)
    val host = uri.host?.lowercase() ?: ""
    uri.scheme?.lowercase() == "https" && uri.userInfo == null && uri.port == -1 &&
      (host == "appleid.apple.com" || host == "idmsa.apple.com")
  } catch (_: Exception) { false }

  private fun googleAuthentication(url: String?): Boolean = try {
    val uri = Uri.parse(url)
    val host = uri.host?.lowercase() ?: ""
    uri.scheme?.lowercase() == "https" && uri.userInfo == null && uri.port == -1 &&
      (host == "accounts.google.com" || host == "accounts.google.co.uk")
  } catch (_: Exception) { false }

  private fun allowed(url: String?): Boolean = try {
    val uri = Uri.parse(url)
    val host = uri.host?.lowercase() ?: ""
    uri.scheme?.lowercase() == "https" &&
      uri.userInfo == null && uri.port == -1 &&
      (host == "chatgpt.com" || host.endsWith(".chatgpt.com") ||
        host == "openai.com" || host.endsWith(".openai.com") ||
        appleAuthentication(url))
  } catch (_: Exception) { false }

  // A system Custom Tab is a *separate* browser: it cannot hand its cookies
  // to this isolated WebView or satisfy Bring Session. Only open it after an
  // explicit second user action; never silently redirect the Google button.
  private fun openSystemChatGpt(): Boolean {
    val now = android.os.SystemClock.elapsedRealtime()
    if (now - lastExternalAuthOpen < 1500L) return true
    for (packageName in listOf("com.android.chrome", "com.brave.browser", null)) {
      try {
        val tab = CustomTabsIntent.Builder()
          .setShowTitle(true)
          .setShareState(CustomTabsIntent.SHARE_STATE_OFF)
          .build()
        if (packageName != null) tab.intent.setPackage(packageName)
        tab.launchUrl(activity, Uri.parse(HOME))
        lastExternalAuthOpen = now
        // Return to the two-button Setup when Chrome is closed. The private
        // page stays available for an independent, app-owned session check.
        autoSignInActive = false
        signInHandler.removeCallbacks(autoSignInCheck)
        browser?.stopLoading()
        browser?.loadUrl(HOME)
        window?.hide()
        visible = false
        showBrowserNotice("ChatGPT opened in Chrome. Its login is separate from this private browser and " +
          "cannot enable Bring Session. Return here to sign in with an available email/password method.")
        return true
      } catch (_: ActivityNotFoundException) { /* Try another installed browser. */ }
      catch (_: SecurityException) { /* Browser cannot handle this request. */ }
    }
    showBrowserNotice("No browser could open ChatGPT. Install or enable Chrome and try again.")
    return false
  }

  private fun closeAuthPopup() {
    val child = authPopup ?: return
    authPopup = null
    (child.parent as? ViewGroup)?.removeView(child)
    child.stopLoading()
    child.destroy()
    // Apple may finish by closing its popup and notifying the opener. Check
    // the original ChatGPT document again; never infer success from closure.
    if (autoSignInActive && visible) scheduleAutoSignInCheck(1200L)
  }

  // Google explicitly blocks OAuth inside an embedded WebView. Previously,
  // pressing Continue with Google jumped straight to an already signed-in
  // Chrome homepage and made Step 3 look broken: the private website session
  // still had no credentials, and Bring Session was correctly disabled.
  // Explain that boundary BEFORE any optional external-browser handoff.
  private fun showGoogleLoginGuidance() {
    val guidance = "Google sign-in cannot finish in this private browser. " +
      "Chrome sign-in is separate and cannot be brought into Mcp Agent Ai. " +
      "If your account offers Add password under ChatGPT Settings > Security and login, " +
      "you can set it in Chrome, then sign in here with your email and password."
    signInGuidance = guidance
    showBrowserNotice(guidance)
    if (googleGuideShowing) return
    googleGuideShowing = true
    try {
      AlertDialog.Builder(activity)
        .setTitle("Google login needs a supported browser")
        .setMessage(guidance + "\n\nNothing is linked to Mcp Agent Ai yet.")
        .setPositiveButton("Back to ChatGPT") { _, _ -> returnToChatGpt() }
        .setNeutralButton("Open Chrome (not linked)") { _, _ -> openSystemChatGpt() }
        .setOnDismissListener { googleGuideShowing = false }
        .show()
    } catch (_: Exception) {
      googleGuideShowing = false
      // The in-app notice remains visible if Android cannot show a dialog.
    }
  }

  private fun handleAuthNavigation(url: String?): Boolean {
    if (allowed(url)) return false
    if (googleAuthentication(url)) {
      if (authPopup != null) {
        // Never destroy a child WebView synchronously inside its navigation
        // callback; only the user may choose to open a separate browser.
        Handler(Looper.getMainLooper()).post {
          closeAuthPopup()
          showGoogleLoginGuidance()
        }
      } else showGoogleLoginGuidance()
      return true
    }
    showBrowserNotice(blockedNavigationMessage(url))
    return true
  }

  private fun showBrowserNotice(message: String?) {
    val notice = browserNotice ?: return
    notice.text = message ?: ""
    notice.visibility = if (message == null) View.GONE else View.VISIBLE
  }

  private fun blockedNavigationMessage(url: String?): String {
    val host = try { Uri.parse(url).host?.lowercase() ?: "" }
      catch (_: Exception) { "" }
    return if (host == "accounts.google.com" || host.endsWith(".accounts.google.com")) {
      "Google requires a supported system browser. Use Chrome or Brave to sign in to ChatGPT."
    } else {
      "This destination cannot open in the private ChatGPT browser. " +
        "Tap Back to ChatGPT to recover."
    }
  }

  private fun returnToChatGpt() {
    val web = browser ?: return
    closeAuthPopup()
    showBrowserNotice(null)
    // Do not navigate an OAuth popup, reuse Chrome cookies, or replace the
    // isolated browser. Recover its original ChatGPT page in the SAME profile.
    web.stopLoading()
    web.loadUrl(HOME)
    if (visible) startAutoSignInCheck()
  }

  private fun statusResponse(): JSObject = JSObject().apply {
    val active = browser != null
    val supported = independentProfile()
    put("supported", supported)
    put("running", active)
    put("visible", active && visible)
    put("sessionLinked", sessionLinked())
    put("signInVerified", signInVerified())
    put("engine", "Android owned WebView (separate profile)")
    put("message", when {
      !supported -> "This Android WebView provider cannot isolate a ChatGPT browser profile; no browser was started."
      !active -> "Built-in ChatGPT website is stopped. Open explicitly to sign in; your private profile remains stored."
      sessionLinked() -> "ChatGPT session brought into this owned browser recently. No Chrome cookies or chat history were imported."
      signInVerified() -> "ChatGPT confirmed your login. The owned browser window was hidden; Bring Session can now independently recheck it."
      signInGuidance != null -> signInGuidance!!
      visible -> "ChatGPT website is visible. Google sign-in requires Chrome, whose cookies cannot be linked to the private browser."
      else -> "ChatGPT website is hidden inside Mcp Agent Ai. Android may suspend it in the background."
    })
  }

  private fun ui(invoke: Invoke, work: () -> Unit) {
    activity.runOnUiThread {
      try { work() } catch (_: Exception) {
        invoke.reject("Owned ChatGPT browser operation failed; the status can be checked again")
      }
    }
  }

  @Command
  fun status(invoke: Invoke) = ui(invoke) {
    // Step 3 can inspect an existing private ChatGPT login immediately after
    // opening Setup, including after a process restart. The WebView's existing
    // profile is reloaded offscreen without bringing a login window forward.
    if (browser == null && independentProfile()) {
      try { createOwned(false) }
      catch (_: Exception) { destroyOwned() }
    }
    refreshHiddenSessionIfDue()
    invoke.resolve(statusResponse())
  }

  /**
   * COS-style B: check the account using ChatGPT's own same-origin session
   * response, never by guessing from a visible composer (guest mode exists).
   * No session cookie, bearer token, account data, transcript or URL is ever
   * exposed to Tauri. "Bring session" keeps the SAME private WebView profile
   * hidden in this foreground app; it does not import Chrome credentials.
   */
  private fun verifySignIn(invoke: Invoke?, bring: Boolean) {
    val web = browser
    // The native auto-monitor uses the same read-only check as explicit Bring.
    // Only a short enum receipt can leave this function, never account data.
    if (web == null || !isChatGptHome(web.url) || !independentProfile() ||
        sessionCheckPending || probePending || lease != null) {
      invoke?.reject("Open the owned ChatGPT browser and finish sign-in before checking this session")
      scheduleAutoSignInCheck()
      return
    }
    val generation = pageGeneration
    sessionCheckPending = true
    val main = Handler(Looper.getMainLooper())
    var settled = false
    val nonce = UUID.randomUUID().toString()
    // evaluateJavascript does NOT await a JS Promise. Start the same-origin
    // read, then poll only its private nonce-scoped boolean result. Do not
    // install any JavaScript-to-native interface or hand login response bytes to native.
    val script = """(function(){
      if ((location.origin !== 'https://chatgpt.com' &&
           location.origin !== 'https://www.chatgpt.com') ||
          !(/^\/$|^\/(?:g\/[^/]+\/)?c\/[0-9a-f-]{8,64}\/?$/i.test(location.pathname)))
        return 'invalid';
      const slot = {nonce:${JSONObject.quote(nonce)},state:'pending'};
      window.__mcpAccountCheck = slot;
      (async function() {
        let outcome = 'unknown';
        try {
          const response = await fetch('/api/auth/session', {
            credentials:'include',cache:'no-store',signal:AbortSignal.timeout(4000)
          });
          if (response.ok) {
            const account = await response.json();
            outcome = account && typeof account === 'object' &&
              (account.accessToken || account.user) ? 'signed_in' : 'signed_out';
          }
        } catch (_) { /* Unknown never verifies a login. */ }
        if (window.__mcpAccountCheck === slot) slot.state = outcome;
      })();
      return 'started';
    })()""".trimIndent()
    fun finish(state: String) {
      if (settled) return
      settled = true
      sessionCheckPending = false
      if (browser !== web || pageGeneration != generation || !isChatGptHome(web.url)) {
        linkedPageGeneration = -1
        linkedAtElapsed = 0L
        signInPageGeneration = -1
        signInAtElapsed = 0L
        invoke?.reject("ChatGPT browser page changed during account verification")
        scheduleAutoSignInCheck()
        return
      }
      val answer = state.takeIf { it in setOf("signed_in","signed_out","unknown") } ?: "unknown"
      if (answer != "signed_in") {
        linkedPageGeneration = -1
        linkedAtElapsed = 0L
        signInPageGeneration = -1
        signInAtElapsed = 0L
      } else {
        signInGuidance = null
        signInPageGeneration = generation
        signInAtElapsed = android.os.SystemClock.elapsedRealtime()
        if (linkedPageGeneration == generation && linkedAtElapsed > 0L)
          linkedAtElapsed = signInAtElapsed
      }
      if (bring && answer == "signed_in") {
        linkedPageGeneration = generation
        linkedAtElapsed = android.os.SystemClock.elapsedRealtime()
        window?.hide()
        visible = false
      }
      if (!bring && answer == "signed_in" && autoSignInActive && visible) {
        // Hide the foreground login window, retaining this exact WebView and
        // its private profile for the user's subsequent Bring Session action.
        autoSignInActive = false
        signInHandler.removeCallbacks(autoSignInCheck)
        window?.hide()
        visible = false
      }
      invoke?.resolve(JSObject().apply {
        put("owned", true)
        put("origin", "chatgpt.com")
        put("readOnly", true)
        put("pageGeneration", generation)
        put("signInState", answer)
        put("sessionLinked", sessionLinked())
      })
      scheduleAutoSignInCheck(4500L)
    }
    val timeout = Runnable { finish("unknown") }
    main.postDelayed(timeout, 6500)
    val pollScript = """(function(){
      const value = window.__mcpAccountCheck;
      if (!value || value.nonce !== ${JSONObject.quote(nonce)}) return 'unknown';
      const state = value.state;
      if (state !== 'pending') delete window.__mcpAccountCheck;
      return state;
    })()""".trimIndent()
    fun poll() {
      if (settled) return
      if (browser !== web || pageGeneration != generation || !isChatGptHome(web.url)) {
        main.removeCallbacks(timeout)
        finish("unknown")
        return
      }
      try {
        web.evaluateJavascript(pollScript) { raw ->
          if (settled) return@evaluateJavascript
          val state = try { JSONTokener(raw).nextValue() as? String }
            catch (_: Exception) { null }
          if (state == "pending") main.postDelayed({ poll() }, 250)
          else {
            main.removeCallbacks(timeout)
            finish(state ?: "unknown")
          }
        }
      } catch (_: Exception) {
        main.removeCallbacks(timeout)
        finish("unknown")
      }
    }
    try {
      web.evaluateJavascript(script) { raw ->
        if (settled) return@evaluateJavascript
        val state = try { JSONTokener(raw).nextValue() as? String }
          catch (_: Exception) { null }
        if (state == "started") poll()
        else {
          main.removeCallbacks(timeout)
          finish("unknown")
        }
      }
    } catch (_: Exception) {
      if (!settled) {
        main.removeCallbacks(timeout)
        finish("unknown")
      }
    }
  }

  @Command
  fun checkSession(invoke: Invoke) = ui(invoke) { verifySignIn(invoke, false) }

  @Command
  fun bringSession(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptOpenArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid session handoff request"); return }
    if (!args.userInitiated) {
      invoke.reject("Bringing a ChatGPT session into the app requires an explicit user action")
      return
    }
    ui(invoke) {
      if (!signInVerified() || visible) {
        invoke.reject("ChatGPT sign-in must be verified and its browser hidden before Bring Session")
        return@ui
      }
      // Never trust the previous login proof alone; the native same-origin
      // endpoint must confirm a fresh authenticated session again.
      verifySignIn(invoke, true)
    }
  }

  /** Read ONLY a compositor-ready capability fact from an owned ChatGPT page.
   * Never evaluates on login/OAuth origins and never reads messages, tokens,
   * composer contents, cookies, conversation URLs or local storage.
   */
  @Command
  fun probe(invoke: Invoke) = ui(invoke) {
    val web = browser
    if (web == null || probePending || sessionCheckPending || !visible || !isChatGptHome(web.url)) {
      invoke.reject("ChatGPT browser must be visible on an owned ChatGPT page to check readiness")
      return@ui
    }
    val revision = pageGeneration
    probePending = true
    val code = """(function(){
      if(location.origin !== 'https://chatgpt.com' && location.origin !== 'https://www.chatgpt.com')
        return JSON.stringify({originOkay:false,composerReady:false,sendButtonReady:false});
      const editor = document.querySelector('#prompt-textarea');
      const controls = editor?.closest('form');
      const send = controls?.querySelector('button[data-testid="send-button"],button[aria-label^="Send" i],button[type="submit"]');
      return JSON.stringify({originOkay:true,composerReady:Boolean(editor?.isConnected && !editor.closest('[inert],[hidden]')),
        sendButtonReady:Boolean(send && !send.disabled && send.getAttribute('aria-disabled') !== 'true')});
    })()""".trimIndent()
    web.evaluateJavascript(code) { result ->
      // One-shot callback is only valid for the SAME live attached owned page.
      probePending = false
      if (browser !== web || pageGeneration != revision || !visible || !isChatGptHome(web.url)) {
        invoke.reject("ChatGPT page changed during readiness check")
        return@evaluateJavascript
      }
      val payload = try {
        val embedded = JSONTokener(result).nextValue() as? String
        if (embedded == null || embedded.length > 300) null else JSONObject(embedded)
      } catch (_: Exception) { null }
      if (payload == null || !payload.has("originOkay") || !payload.has("composerReady") ||
        !payload.has("sendButtonReady") || payload.length() != 3 ||
        !payload.optBoolean("originOkay", false)) {
        invoke.reject("ChatGPT page readiness was not verified")
        return@evaluateJavascript
      }
      invoke.resolve(JSObject().apply {
        put("owned", true)
        put("origin", "chatgpt.com")
        put("pageGeneration", revision)
        put("composerReady", payload.optBoolean("composerReady", false))
        put("sendButtonReady", payload.optBoolean("sendButtonReady", false))
        put("readOnly", true)
      })
    }
  }

  private fun isChatGptHome(url: String?): Boolean = try {
    val value = Uri.parse(url)
    value.scheme == "https" && value.userInfo == null &&
      value.port == -1 && (value.host == "chatgpt.com" || value.host == "www.chatgpt.com")
  } catch (_: Exception) { false }

  @Command
  fun open(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptOpenArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid browser open request"); return }
    if (!args.userInitiated) {
      invoke.reject("Opening the ChatGPT website requires explicit user initiation")
      return
    }
    ui(invoke) {
      if (browser != null) {
        showOwned()
        startAutoSignInCheck()
        invoke.resolve(statusResponse())
        return@ui
      }
      if (!independentProfile()) {
        invoke.reject("Separate Android WebView profile is unavailable; the Tauri profile was not reused")
        return@ui
      }
      // The explicit Sign in to ChatGPT tap already authorizes opening this
      // app-owned browser. Do not insert a second confirmation step.
      try {
        createOwned()
        invoke.resolve(statusResponse())
      } catch (_: Exception) {
        destroyOwned()
        invoke.reject("ChatGPT browser could not be created with an isolated profile")
      }
    }
  }

  @Command
  fun show(invoke: Invoke) = ui(invoke) {
    if (browser == null) { invoke.reject("ChatGPT browser is not running"); return@ui }
    showOwned()
    startAutoSignInCheck()
    invoke.resolve(statusResponse())
  }

  @Command
  fun hide(invoke: Invoke) = ui(invoke) {
    if (browser == null) { invoke.reject("ChatGPT browser is not running"); return@ui }
    autoSignInActive = false
    signInHandler.removeCallbacks(autoSignInCheck)
    window?.hide()
    visible = false
    invoke.resolve(statusResponse())
  }

  @Command
  fun close(invoke: Invoke) = ui(invoke) {
    destroyOwned()
    invoke.resolve(statusResponse())
  }

  private fun showOwned() {
    val dialog = window ?: throw IllegalStateException("ChatGPT browser has no window")
    if (!visible) {
      dialog.show()
      dialog.window?.setLayout(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT)
      visible = true
    }
  }

  private fun createOwned(showOnCreate: Boolean = true) {
    check(browser == null && window == null && independentProfile())
    val web = WebView(activity)
    try {
      // Must occur before reading/changing WebView settings or navigating.
      WebViewCompat.setProfile(web, PROFILE)
      web.settings.apply {
        javaScriptEnabled = true // ChatGPT site, NOT any native JavascriptInterface.
        domStorageEnabled = true
        databaseEnabled = true
        allowFileAccess = false
        allowContentAccess = false
        allowFileAccessFromFileURLs = false
        allowUniversalAccessFromFileURLs = false
        mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
        setSupportMultipleWindows(true)
        javaScriptCanOpenWindowsAutomatically = false
        mediaPlaybackRequiresUserGesture = true
        if (Build.VERSION.SDK_INT >= 26) safeBrowsingEnabled = true
      }
      CookieManager.getInstance().setAcceptThirdPartyCookies(web, false)
      web.webChromeClient = object : WebChromeClient() {
        override fun onCreateWindow(
          view: WebView?, isDialog: Boolean, isUserGesture: Boolean,
          resultMsg: android.os.Message?
        ): Boolean {
          // Apple sign-in uses a user-clicked popup. Keep it in the SAME private
          // profile so Apple's return to the opener can establish a site session.
          // Do not permit nested, scripted or unmanaged popup windows.
          if (!isUserGesture || resultMsg == null || authPopup != null || browserPages == null) {
            showBrowserNotice("Only one user-opened sign-in window is supported. Tap Back to ChatGPT to retry.")
            return false
          }
          return try {
            val child = WebView(activity)
            WebViewCompat.setProfile(child, PROFILE)
            child.settings.apply {
              javaScriptEnabled = true
              domStorageEnabled = true
              allowFileAccess = false
              allowContentAccess = false
              allowFileAccessFromFileURLs = false
              allowUniversalAccessFromFileURLs = false
              mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
              setSupportMultipleWindows(false)
              javaScriptCanOpenWindowsAutomatically = false
              if (Build.VERSION.SDK_INT >= 26) safeBrowsingEnabled = true
            }
            CookieManager.getInstance().setAcceptThirdPartyCookies(child, false)
            child.webChromeClient = object : WebChromeClient() {
              override fun onCloseWindow(window: WebView?) { if (window === authPopup) closeAuthPopup() }
              override fun onPermissionRequest(request: PermissionRequest) = request.deny()
            }
            child.webViewClient = object : WebViewClient() {
              override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest): Boolean =
                request.isForMainFrame && request.url.toString() != "about:blank" &&
                  handleAuthNavigation(request.url.toString())
              @Deprecated("Compatibility for WebView top-level URL loading")
              override fun shouldOverrideUrlLoading(view: WebView?, url: String?): Boolean =
                url != "about:blank" && handleAuthNavigation(url)
              override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
                if (url != "about:blank" && !allowed(url)) {
                  view?.stopLoading()
                  // Redirects may bypass shouldOverrideUrlLoading; defer
                  // destruction until this WebView callback has returned.
                  if (googleAuthentication(url)) {
                    Handler(Looper.getMainLooper()).post {
                      if (view === authPopup) {
                        closeAuthPopup()
                        showGoogleLoginGuidance()
                      }
                    }
                  } else showBrowserNotice(blockedNavigationMessage(url))
                }
              }
              override fun onReceivedError(view: WebView?, request: WebResourceRequest?, error: WebResourceError?) {
                if (view === authPopup && request?.isForMainFrame == true)
                  showBrowserNotice("The sign-in window could not load. Tap Back to ChatGPT to retry.")
              }
              override fun onReceivedHttpError(view: WebView?, request: WebResourceRequest?, response: WebResourceResponse?) {
                if (view === authPopup && request?.isForMainFrame == true &&
                    (response?.statusCode ?: 0) >= 400)
                  showBrowserNotice("The sign-in provider returned an error. Tap Back to ChatGPT to retry.")
              }
              override fun onReceivedSslError(view: WebView?, handler: SslErrorHandler, error: SslError?) {
                handler.cancel()
                showBrowserNotice("Secure sign-in failed. Tap Back to ChatGPT to retry.")
              }
            }
            authPopup = child
            browserPages?.addView(child, FrameLayout.LayoutParams(
              ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            val transport = resultMsg.obj as WebView.WebViewTransport
            transport.webView = child
            resultMsg.sendToTarget()
            showBrowserNotice(null)
            true
          } catch (_: Exception) {
            closeAuthPopup()
            showBrowserNotice("The sign-in window could not open. Tap Back to ChatGPT to retry.")
            false
          }
        }
        override fun onCloseWindow(window: WebView?) { if (window === authPopup) closeAuthPopup() }
        override fun onPermissionRequest(request: PermissionRequest) = request.deny()
        override fun onGeolocationPermissionsShowPrompt(
          origin: String?, callback: GeolocationPermissions.Callback?
        ) { callback?.invoke(origin, false, false) }
      }
      web.webViewClient = object : WebViewClient() {
        override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest): Boolean {
          if (!request.isForMainFrame) return false
          return handleAuthNavigation(request.url.toString())
        }
        @Deprecated("Compatibility for WebView top-level URL loading")
        override fun shouldOverrideUrlLoading(view: WebView?, url: String?): Boolean {
          return handleAuthNavigation(url)
        }
        override fun onPageStarted(view: WebView?, url: String?, favicon: android.graphics.Bitmap?) {
          pageGeneration++
          lastSessionCheckAtElapsed = 0L
          linkedPageGeneration = -1
          linkedAtElapsed = 0L
          signInPageGeneration = -1
          signInAtElapsed = 0L
          if (googleAuthentication(url)) {
            view?.stopLoading()
            showGoogleLoginGuidance()
          } else if (!allowed(url)) {
            view?.stopLoading()
            showBrowserNotice(blockedNavigationMessage(url))
          } else showBrowserNotice(null)
        }
        override fun onPageFinished(view: WebView?, url: String?) {
          if (view === browser && isChatGptHome(url)) {
            if (autoSignInActive) scheduleAutoSignInCheck(1500L)
            else if (!visible) refreshHiddenSessionIfDue()
          }
        }
        override fun onReceivedError(view: WebView?, request: WebResourceRequest?, error: WebResourceError?) {
          if (view === browser && request?.isForMainFrame == true)
            showBrowserNotice("This page could not load. Tap Back to ChatGPT to recover.")
        }
        override fun onReceivedHttpError(view: WebView?, request: WebResourceRequest?, response: WebResourceResponse?) {
          if (view === browser && request?.isForMainFrame == true &&
              (response?.statusCode ?: 0) >= 400)
            showBrowserNotice("The sign-in page returned an error. Tap Back to ChatGPT to retry.")
        }
        override fun onReceivedSslError(view: WebView?, handler: SslErrorHandler, error: SslError?) {
          pageGeneration++
          linkedPageGeneration = -1
          linkedAtElapsed = 0L
          signInPageGeneration = -1
          signInAtElapsed = 0L
          handler?.cancel()
          showBrowserNotice("Secure connection failed. Tap Back to ChatGPT to recover.")
        }
      }
      web.setDownloadListener { _, _, _, _, _ -> } // No implicit downloads.
      val root = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        setBackgroundColor(Color.rgb(19, 25, 35))
      }
      val controls = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
      val site = TextView(activity).apply {
        text = "ChatGPT · private browser"
        setTextColor(Color.WHITE)
        textSize = 13f
        setPadding(16, 10, 12, 2)
      }
      controls.addView(site)
      val navigation = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        setPadding(8, 0, 8, 4)
      }
      navigation.addView(Button(activity).apply {
        text = "← Back to ChatGPT"
        isAllCaps = false
        setOnClickListener { returnToChatGpt() }
      }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1.15f))
      navigation.addView(Button(activity).apply {
        text = "Back to Setup"
        isAllCaps = false
        setOnClickListener {
          autoSignInActive = false
          signInHandler.removeCallbacks(autoSignInCheck)
          window?.hide()
          visible = false
        }
      }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
      controls.addView(navigation)
      val notice = TextView(activity).apply {
        textSize = 12f
        setTextColor(Color.WHITE)
        setBackgroundColor(Color.rgb(77, 50, 36))
        setPadding(14, 12, 14, 12)
        visibility = View.GONE
      }
      controls.addView(notice)
      browserNotice = notice
      root.addView(controls)
      val pages = FrameLayout(activity)
      pages.addView(web, FrameLayout.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
      root.addView(pages, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
      val dialog = Dialog(activity).apply {
        setContentView(root)
        setCancelable(false)
        setCanceledOnTouchOutside(false)
      }
      browser = web
      browserPages = pages
      window = dialog
      if (showOnCreate) showOwned()
      web.loadUrl(HOME)
      if (showOnCreate) startAutoSignInCheck()
    } catch (error: Exception) {
      runCatching { web.destroy() }
      throw error
    }
  }

  private fun destroyOwned() {
    closeAuthPopup()
    browserPages = null
    visible = false
    browserNotice = null
    signInGuidance = null
    googleGuideShowing = false
    autoSignInActive = false
    signInHandler.removeCallbacks(autoSignInCheck)
    lastSessionCheckAtElapsed = 0L
    pageGeneration++
    linkedPageGeneration = -1
    linkedAtElapsed = 0L
    signInPageGeneration = -1
    signInAtElapsed = 0L
    lease = null
    val previous = browser
    browser = null
    val dialog = window
    window = null
    if (previous != null) {
      (previous.parent as? ViewGroup)?.removeView(previous)
      previous.stopLoading()
      previous.destroy()
    }
    dialog?.dismiss()
    // Preserve ONLY the plugin-owned profile across explicit stops so sign-in
    // remains possible. Never clear Default, Chrome or another application's data.
  }
}
