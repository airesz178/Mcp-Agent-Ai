package ai.mcpagent.app

import android.app.Activity
import android.app.Application
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.speech.RecognizerIntent
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.concurrent.atomic.AtomicReference

@InvokeArg
class SpeechDictationArgs {
  var requestId: String = ""
  var userInitiated: Boolean = false
}

/**
 * Explicit foreground handoff to Android's ACTION_RECOGNIZE_SPEECH UI.
 *
 * This app never requests RECORD_AUDIO, opens its own microphone, receives
 * audio, runs a SpeechRecognizer listener, saves a transcript, auto-sends
 * recognized text, or starts any background component. The selected Android
 * speech provider may process audio online; that is controlled by the OS
 * service, not by this plugin.
 */
@TauriPlugin
class SpeechDictationPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val MAX_TEXT_BYTES = 2000
    private const val SOURCE = "Android speech recognizer"
    private const val FOCUS_RETRIES = 15
    private const val FOCUS_DELAY_MS = 70L
    private const val MAX_EXTERNAL_DURATION_MS = 120_000L
    private val uuidV4 = Regex(
      "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
    )
    private val active = AtomicReference<Pending?>(null)

    internal fun validRequestId(id: String): Boolean = uuidV4.matches(id)

    // Visible transcription only. Reject controls, malformed surrogate
    // pairs, bidi/zero-width format characters, and variation selectors.
    // Trim whitespace at both edges; never mutate internal recognized text.
    internal fun boundedSpeechText(raw: String): String? {
      if (raw.isEmpty() || raw.length > MAX_TEXT_BYTES) return null
      // Trim ordinary leading/trailing spaces only. A newline/tab/control at
      // either edge must be rejected, not silently erased by String.trim().
      val text = raw.trim(' ')
      if (text.isEmpty() || text.length > MAX_TEXT_BYTES) return null
      var pos = 0
      while (pos < text.length) {
        val c = text[pos]
        if (Character.isHighSurrogate(c)) {
          if (pos + 1 >= text.length || !Character.isLowSurrogate(text[pos + 1])) return null
        } else if (Character.isLowSurrogate(c)) {
          return null
        }
        val code = Character.codePointAt(text, pos)
        if (Character.isISOControl(code) ||
          Character.getType(code) == Character.FORMAT.toInt() ||
          code and 0xFFFE == 0xFFFE ||
          code in 0xFDD0..0xFDEF ||
          code in 0xE000..0xF8FF ||
          code in 0xF0000..0xFFFFD || code in 0x100000..0x10FFFD ||
          code in 0xE0000..0xE0FFF ||
          code in 0xFE00..0xFE0F || code in 0xE0100..0xE01EF ||
          (Character.isWhitespace(code) && code != 0x20) ||
          code == 0x00A0 || code == 0x034F ||
          code in 0x115F..0x1160 || code in 0x17B4..0x17B5 ||
          code in 0x180B..0x180F || code in 0x200B..0x200F ||
          code in 0x202A..0x202E || code in 0x2060..0x206F ||
          code == 0x2800 || code == 0x3164 || code == 0xFFA0 ||
          code in 0xFFF9..0xFFFB) return null
        pos += Character.charCount(code)
      }
      if (text.toByteArray(Charsets.UTF_8).size !in 1..MAX_TEXT_BYTES) return null
      return text
    }
  }

  private class Pending(
    val owner: SpeechDictationPlugin,
    val id: String,
    val invoke: Invoke
  ) {
    var launched: Boolean = false
    var expiry: Runnable? = null
  }

  private val main = Handler(Looper.getMainLooper())
  private var destroyed = false
  private fun supported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
  private fun foreground(): Boolean =
    !destroyed && !activity.isFinishing && !activity.isDestroyed && activity.hasWindowFocus()

  private fun speechIntent(): Intent =
    Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
      putExtra(
        RecognizerIntent.EXTRA_LANGUAGE_MODEL,
        RecognizerIntent.LANGUAGE_MODEL_FREE_FORM
      )
      putExtra(RecognizerIntent.EXTRA_MAX_RESULTS, 1)
    }

  private fun serviceAvailable(): Boolean {
    if (!supported()) return false
    // Conservative, truthful visibility of an ACTION_RECOGNIZE_SPEECH Activity.
    // On Android 11+ package-visibility restrictions can make a genuine
    // external handler invisible unless the manifest includes a <queries>
    // declaration; never claim availability without seeing a handler.
    return try {
      activity.packageManager.resolveActivity(
        speechIntent(), PackageManager.MATCH_DEFAULT_ONLY
      ) != null
    } catch (_: Exception) {
      false
    }
  }

  private fun release(request: Pending): Boolean {
    if (active.get() !== request) return false
    if (!active.compareAndSet(request, null)) return false
    request.expiry?.let { main.removeCallbacks(it) }
    request.expiry = null
    return true
  }

  private fun rejectPending(request: Pending, reason: String) {
    if (release(request)) request.invoke.reject(reason)
  }

  private fun resultReceipt(
    request: Pending, recognized: Boolean, cancelled: Boolean, text: String
  ): JSObject = JSObject().apply {
    put("requestId", request.id)
    put("recognized", recognized)
    put("cancelled", cancelled)
    put("text", text)
    put("source", SOURCE)
    put("autoSent", false)
  }

  private fun complete(request: Pending, recognizedText: String?) {
    if (!release(request)) return
    request.invoke.resolve(
      resultReceipt(
        request,
        recognized = recognizedText != null,
        cancelled = recognizedText == null,
        text = recognizedText ?: ""
      )
    )
  }

  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, savedInstanceState: Bundle?) = Unit
    override fun onActivityStarted(a: Activity) = Unit
    override fun onActivityResumed(a: Activity) = Unit
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) = Unit
    override fun onActivityPaused(a: Activity) = Unit
    override fun onActivityStopped(a: Activity) = Unit

    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        destroyed = true
        active.get()?.takeIf { it.owner === this@SpeechDictationPlugin }?.let {
          rejectPending(it, "Speech dictation stopped: Activity destroyed")
        }
        a.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }

  init { activity.application.registerActivityLifecycleCallbacks(lifecycle) }

  /**
   * status {supported,available,listening,source,autoSent}.
   * available reflects actual PackageManager visibility of an OS handler.
   * listening means one OS activity request is pending; there is no in-app
   * recording or audio listener.
   */
  @Command
  fun status(invoke: Invoke) {
    main.post {
      val available = serviceAvailable()
      val request = active.get()
      if (request != null && (!available || request.owner.destroyed)) {
        rejectPending(request, "Android speech recognizer became unavailable")
      }
      invoke.resolve(JSObject().apply {
        put("supported", supported())
        put("available", available)
        put("listening", active.get() != null)
        put("source", SOURCE)
        put("autoSent", false)
      })
    }
  }

  @Command
  fun recognize(invoke: Invoke) {
    val args = try { invoke.parseArgs(SpeechDictationArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid speech dictation request"); return }
    if (!args.userInitiated || !validRequestId(args.requestId)) {
      invoke.reject("Speech dictation requires a user action and canonical UUID v4")
      return
    }
    main.post {
      if (!foreground() || !supported() || !serviceAvailable()) {
        invoke.reject("Android speech recognizer unavailable or app not foreground")
        return@post
      }
      val request = Pending(this, args.requestId, invoke)
      if (!active.compareAndSet(null, request)) {
        invoke.reject("Another speech dictation request is already active")
        return@post
      }
      try {
        // Only the OS recognition Activity can access the microphone. The
        // current Activity intentionally pauses/stops during this handoff.
        startActivityForResult(invoke, speechIntent(), "speechResult")
        request.launched = true
        // One bounded safety expiry, not a periodic microphone or transcript
        // background task. The OS recognizer UI itself owns its recording.
        val expiry = Runnable {
          rejectPending(request, "Android speech dictation did not return a result")
        }
        request.expiry = expiry
        // Avoid keeping a completed Invoke alive unnecessarily when a very
        // fast external result arrives before expiry was registered.
        if (active.get() === request) main.postDelayed(expiry, MAX_EXTERNAL_DURATION_MS)
      } catch (_: Exception) {
        rejectPending(request, "Android speech recognizer could not be opened")
      }
    }
  }

  @ActivityCallback
  private fun speechResult(invoke: Invoke, result: ActivityResult) {
    main.post {
      val request = active.get()?.takeIf {
        it.owner === this && it.invoke === invoke && it.launched
      } ?: return@post
      if (destroyed || activity.isFinishing || activity.isDestroyed) {
        rejectPending(request, "Speech dictation Activity is no longer available")
        return@post
      }
      if (result.resultCode == Activity.RESULT_CANCELED) {
        deliverWhenForeground(request, null, FOCUS_RETRIES)
        return@post
      }
      if (result.resultCode != Activity.RESULT_OK) {
        rejectPending(request, "Android speech recognizer returned an invalid result")
        return@post
      }
      val matches = try {
        result.data?.getStringArrayListExtra(RecognizerIntent.EXTRA_RESULTS)
      } catch (_: Exception) {
        null
      }
      val first = matches?.firstOrNull()
      val clean = first?.let(::boundedSpeechText)
      if (clean == null) {
        rejectPending(request, "Android speech recognizer returned invalid or oversized text")
        return@post
      }
      deliverWhenForeground(request, clean, FOCUS_RETRIES)
    }
  }

  private fun deliverWhenForeground(
    request: Pending, recognizedText: String?, retries: Int
  ) {
    if (active.get() !== request) return
    if (destroyed || activity.isFinishing || activity.isDestroyed) {
      rejectPending(request, "Speech dictation Activity was destroyed")
      return
    }
    // ActivityResult can precede Android's onResume and focus restoration.
    // Never expose recognized text to a background Activity.
    if (!foreground()) {
      if (retries <= 0) {
        rejectPending(request, "Speech dictation returned while app was backgrounded")
      } else {
        main.postDelayed({
          deliverWhenForeground(request, recognizedText, retries - 1)
        }, FOCUS_DELAY_MS)
      }
      return
    }
    complete(request, recognizedText)
  }
}
