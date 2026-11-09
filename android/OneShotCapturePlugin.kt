package ai.mcpagent.app

import android.app.Activity
import android.app.Application
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.lang.ref.WeakReference
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

@InvokeArg
class OneShotCaptureArgs {
  var userInitiated: Boolean = false
}

/**
 * A single, user-consented screen frame. Each command opens Android's native
 * MediaProjection consent UI. Consent is never stored or shared with a second
 * session. Screen pixels exist only in memory until sent to the same invoke.
 */
@TauriPlugin
class OneShotCapturePlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private val current = AtomicReference<Request?>(null)
    private val main = Handler(Looper.getMainLooper())

    internal class Request(val activity: WeakReference<Activity>, val invoke: Invoke) {
      val id: String = UUID.randomUUID().toString()
      val completed = AtomicBoolean(false)
      @Volatile var started = false
      @Volatile var approvedIntent: Intent? = null

      fun foreground(): Boolean {
        val host = activity.get() ?: return false
        return !host.isFinishing && !host.isDestroyed && host.hasWindowFocus()
      }

      fun takeConsent(): Intent? = synchronized(this) {
        val result = approvedIntent
        approvedIntent = null
        result
      }
    }

    internal fun claim(id: String): Request? = current.get()?.takeIf {
      it.id == id && it.started && !it.completed.get()
    }

    internal fun finish(request: Request, pngBase64: String? = null,
                        width: Int = 0, height: Int = 0, pngBytes: Int = 0,
                        failure: String = "Screen capture stopped") {
      if (!request.completed.compareAndSet(false, true)) return
      current.compareAndSet(request, null)
      // A frame is delivered ONLY to the still-visible activity which requested it.
      main.post {
        if (pngBase64 != null && request.foreground()) {
          request.invoke.resolve(JSObject().apply {
            put("captured", true)
            put("mimeType", "image/png")
            put("width", width)
            put("height", height)
            put("pngBytes", pngBytes)
            put("base64", pngBase64)
          })
        } else {
          request.invoke.reject(if (pngBase64 != null) "Capture discarded when app lost focus" else failure)
        }
      }
    }

    internal fun cancel(request: Request, reason: String) {
      val host = request.activity.get()
      finish(request, failure = reason)
      if (request.started && host != null) {
        try { host.stopService(Intent(host, ScreenCaptureService::class.java)) } catch (_: Exception) {}
      }
    }
  }

  private val lifecycle = object : Application.ActivityLifecycleCallbacks {
    override fun onActivityCreated(a: Activity, savedInstanceState: Bundle?) {}
    override fun onActivityStarted(a: Activity) {}
    override fun onActivityResumed(a: Activity) {}
    override fun onActivityPaused(a: Activity) {}
    override fun onActivitySaveInstanceState(a: Activity, state: Bundle) {}
    override fun onActivityStopped(a: Activity) {
      if (a === activity) current.get()?.takeIf { it.activity.get() === a && it.started }?.let {
        cancel(it, "Capture stopped when the app left the foreground")
      }
    }
    override fun onActivityDestroyed(a: Activity) {
      if (a === activity) {
        current.get()?.takeIf { it.activity.get() === a }?.let {
          cancel(it, "Screen capture Activity destroyed")
        }
        a.application.unregisterActivityLifecycleCallbacks(this)
      }
    }
  }

  init { activity.application.registerActivityLifecycleCallbacks(lifecycle) }

  @Command
  fun status(invoke: Invoke) {
    invoke.resolve(JSObject().apply {
      put("supported", Build.VERSION.SDK_INT >= 26)
      put("running", current.get() != null)
      put("permissionGranted", false) // consent is one-use, never a durable grant
      put("userInitiatedRequired", true)
      put("maxDimension", ScreenCaptureService.MAX_DIMENSION)
      put("maxPixels", ScreenCaptureService.MAX_PIXELS)
      put("maxPngBytes", ScreenCaptureService.MAX_PNG_BYTES)
      put("mode", "one-frame")
    })
  }

  @Command
  fun capture(invoke: Invoke) {
    val args = try { invoke.parseArgs(OneShotCaptureArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid screen capture request"); return }
    if (!args.userInitiated || Build.VERSION.SDK_INT < 26 ||
      activity.isFinishing || activity.isDestroyed || !activity.hasWindowFocus()) {
      invoke.reject("Screen capture requires a visible app and explicit user action")
      return
    }
    val req = Request(WeakReference(activity), invoke)
    if (!current.compareAndSet(null, req)) {
      invoke.reject("Another screen capture is already pending")
      return
    }
    try {
      val manager = activity.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
      startActivityForResult(invoke, manager.createScreenCaptureIntent(), "captureConsentResult")
    } catch (_: Exception) {
      finish(req, failure = "Android MediaProjection consent could not be opened")
    }
  }

  @ActivityCallback
  private fun captureConsentResult(invoke: Invoke, result: ActivityResult) {
    val req = current.get()?.takeIf { it.invoke === invoke && !it.completed.get() }
    if (req == null) return
    if (result.resultCode != Activity.RESULT_OK || result.data == null) {
      finish(req, failure = "Screen capture permission denied or cancelled")
      return
    }
    // Android may deliver ActivityResult before onResume/focus. Allow a short
    // window for the originating Activity to regain focus; otherwise fail shut.
    fun startWhenForeground(remaining: Int) {
      if (current.get() !== req || req.completed.get()) return
      if (!req.foreground()) {
        if (remaining > 0 && !activity.isDestroyed && !activity.isFinishing) {
          main.postDelayed({ startWhenForeground(remaining - 1) }, 50L)
        } else finish(req, failure = "Screen capture cancelled: app is not foreground")
        return
      }
      synchronized(req) { req.approvedIntent = result.data }
      req.started = true
      try {
        val intent = Intent(activity, ScreenCaptureService::class.java)
          .setAction(ScreenCaptureService.ACTION_CAPTURE)
          .putExtra(ScreenCaptureService.EXTRA_REQUEST_ID, req.id)
        if (Build.VERSION.SDK_INT >= 26) activity.startForegroundService(intent)
        else activity.startService(intent)
      } catch (_: Exception) {
        finish(req, failure = "Android mediaProjection foreground service unavailable")
      }
    }
    main.post { startWhenForeground(12) }
  }

  @Command
  fun cancelCapture(invoke: Invoke) {
    val args = try { invoke.parseArgs(OneShotCaptureArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid capture cancellation"); return }
    if (!args.userInitiated) { invoke.reject("Cancellation requires a user action"); return }
    current.get()?.let { cancel(it, "Screen capture cancelled") }
    invoke.resolve(JSObject().apply { put("cancelled", true) })
  }
}
