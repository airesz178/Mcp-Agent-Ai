package ai.mcpagent.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.Looper
import android.util.Base64
import android.view.WindowManager
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.OutputStream
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.max
import kotlin.math.sqrt

/**
 * Exported=false foreground service. Owns exactly one MediaProjection and one
 * VirtualDisplay, obtains one image, then revokes the projection immediately.
 * No file/network/clipboard operations or restart after process death.
 *
 * Manifest integration required (done by prime):
 *   <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
 *   <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PROJECTION" />
 *   <service android:name=".ScreenCaptureService" android:exported="false"
 *      android:foregroundServiceType="mediaProjection" />
 */
class ScreenCaptureService : Service() {
  companion object {
    const val ACTION_CAPTURE = "ai.mcpagent.app.ONE_SHOT_CAPTURE"
    const val EXTRA_REQUEST_ID = "one_shot_request_id"
    const val MAX_DIMENSION = 1280
    const val MAX_PIXELS = 921_600
    const val MAX_PNG_BYTES = 2 * 1024 * 1024
    private const val NOTIFICATION_ID = 4937
    private const val CHANNEL_ID = "one_shot_capture"
    private const val TIMEOUT_MS = 5_000L

    /** Pure sizing policy, independent of any device/session. */
    internal fun boundedSize(w: Int, h: Int): Pair<Int, Int>? {
      if (w < 1 || h < 1 || w > 16_384 || h > 16_384) return null
      val scale = minOf(1.0, MAX_DIMENSION.toDouble() / max(w, h),
        sqrt(MAX_PIXELS.toDouble() / (w.toDouble() * h.toDouble())))
      val width = max(1, (w * scale).toInt())
      val height = max(1, (h * scale).toInt())
      if (width > MAX_DIMENSION || height > MAX_DIMENSION ||
          width.toLong() * height > MAX_PIXELS) return null
      return width to height
    }
  }

  private var request: OneShotCapturePlugin.Companion.Request? = null
  private var projection: MediaProjection? = null
  private var projectionCallback: MediaProjection.Callback? = null
  private var display: VirtualDisplay? = null
  private var reader: ImageReader? = null
  private var worker: HandlerThread? = null
  private val main = Handler(Looper.getMainLooper())
  private val done = AtomicBoolean(false)
  private val imageTaken = AtomicBoolean(false)
  private val watchdog = Runnable { terminate("Screen capture timed out") }
  private val focusCheck = object : Runnable {
    override fun run() {
      if (done.get()) return
      if (request?.foreground() != true) terminate("Screen capture stopped when app lost focus")
      else main.postDelayed(this, 75L)
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (intent?.action != ACTION_CAPTURE || request != null || done.get()) {
      stopSelf()
      return START_NOT_STICKY
    }
    val id = intent.getStringExtra(EXTRA_REQUEST_ID)
    val candidate = id?.let { OneShotCapturePlugin.claim(it) }
    if (candidate == null || !candidate.foreground()) {
      candidate?.let { OneShotCapturePlugin.finish(it, failure = "Capture Activity is not foreground") }
      stopSelf()
      return START_NOT_STICKY
    }
    request = candidate
    val consent = candidate.takeConsent()
    if (consent == null) {
      terminate("Screen capture consent was already consumed")
      return START_NOT_STICKY
    }
    try {
      beginForeground()
      val metrics = screenSize()
      val size = boundedSize(metrics.first, metrics.second)
        ?: throw SecurityException("Unbounded capture dimensions")
      val width = size.first
      val height = size.second
      // The FGS MUST already have been started with mediaProjection type.
      val manager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
      val session = manager.getMediaProjection(android.app.Activity.RESULT_OK, consent)
        ?: throw SecurityException("Projection permission revoked")
      projection = session
      val callback = object : MediaProjection.Callback() {
        override fun onStop() { terminate("Android revoked screen capture") }
      }
      projectionCallback = callback
      session.registerCallback(callback, main)
      val thread = HandlerThread("one-shot-frame")
      thread.start()
      worker = thread
      val receiver = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 2)
      reader = receiver
      receiver.setOnImageAvailableListener({ source ->
        val image = try { source.acquireLatestImage() } catch (_: Exception) { null }
        if (image == null) return@setOnImageAvailableListener
        if (!imageTaken.compareAndSet(false, true)) {
          image.close()
          return@setOnImageAvailableListener
        }
        val stillFocused = !done.get() && candidate.foreground()
        val bytes = try {
          if (stillFocused) encodeBoundedPng(image, width, height) else null
        } catch (_: Exception) {
          null
        } finally {
          image.close()
        }
        if (!stillFocused) terminate("Screen capture Activity left foreground")
        else if (bytes == null) terminate("Screen capture PNG exceeds safety bounds")
        else terminate(null, Base64.encodeToString(bytes, Base64.NO_WRAP), width, height, bytes.size)
      }, Handler(thread.looper))
      // One token, one display, one frame: never retry createVirtualDisplay.
      display = session.createVirtualDisplay("McpAgentAiOneShot", width, height,
        resources.displayMetrics.densityDpi.coerceAtLeast(1),
        DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
        receiver.surface, null, Handler(thread.looper))
      main.postDelayed(watchdog, TIMEOUT_MS)
      main.post(focusCheck)
    } catch (_: Exception) {
      terminate("Android one-shot MediaProjection is unsupported or unavailable")
    }
    return START_NOT_STICKY
  }

  private fun beginForeground() {
    val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (Build.VERSION.SDK_INT >= 26) {
      manager.createNotificationChannel(NotificationChannel(CHANNEL_ID,
        "One-time screen capture", NotificationManager.IMPORTANCE_LOW))
    }
    val builder = if (Build.VERSION.SDK_INT >= 26) Notification.Builder(this, CHANNEL_ID)
      else Notification.Builder(this)
    val notification = builder
      .setSmallIcon(android.R.drawable.ic_menu_camera)
      .setContentTitle("Capturing one screen frame")
      .setContentText("Android permission applies to this capture only")
      .setOngoing(true)
      .build()
    if (Build.VERSION.SDK_INT >= 29)
      startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
    else startForeground(NOTIFICATION_ID, notification)
  }

  private fun screenSize(): Pair<Int, Int> {
    val wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
    if (Build.VERSION.SDK_INT >= 30) {
      val bounds = wm.maximumWindowMetrics.bounds
      return bounds.width() to bounds.height()
    }
    @Suppress("DEPRECATION")
    val metrics = android.util.DisplayMetrics()
    @Suppress("DEPRECATION")
    wm.defaultDisplay.getRealMetrics(metrics)
    return metrics.widthPixels to metrics.heightPixels
  }

  /** Enforces the PNG limit DURING compression, before any large allocation. */
  private class LimitedOutput : OutputStream() {
    private val out = ByteArrayOutputStream(64 * 1024)
    override fun write(b: Int) {
      if (out.size() >= MAX_PNG_BYTES) throw IOException("PNG too large")
      out.write(b)
    }
    override fun write(b: ByteArray, off: Int, len: Int) {
      if (len < 0 || off < 0 || off > b.size - len || len > MAX_PNG_BYTES - out.size())
        throw IOException("PNG too large")
      out.write(b, off, len)
    }
    fun value(): ByteArray = out.toByteArray()
  }

  private fun encodeBoundedPng(image: Image, width: Int, height: Int): ByteArray? {
    if (image.width != width || image.height != height || image.format != PixelFormat.RGBA_8888 ||
      width.toLong() * height > MAX_PIXELS) return null
    val plane = image.planes.singleOrNull() ?: return null
    val stride = width * 4
    if (plane.pixelStride != 4 || plane.rowStride < stride ||
      plane.rowStride > stride + 4096) return null
    val buffer = plane.buffer
    val required = (height - 1L) * plane.rowStride + stride
    if (required > buffer.limit()) return null
    val rgba = ByteArray(stride * height)
    for (row in 0 until height) {
      buffer.position(row * plane.rowStride)
      buffer.get(rgba, row * stride, stride)
    }
    val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
    return try {
      bitmap.copyPixelsFromBuffer(java.nio.ByteBuffer.wrap(rgba))
      val output = LimitedOutput()
      if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, output)) null
      else output.value().takeIf { it.size in 1..MAX_PNG_BYTES }
    } finally {
      rgba.fill(0)
      bitmap.recycle()
    }
  }

  private fun terminate(reason: String?, base64: String? = null, width: Int = 0,
                        height: Int = 0, count: Int = 0) {
    if (!done.compareAndSet(false, true)) return
    main.removeCallbacks(watchdog)
    main.removeCallbacks(focusCheck)
    // Close in reverse order. unregisterCallback before stop avoids recursion.
    try { display?.release() } catch (_: Exception) {}
    display = null
    try { reader?.close() } catch (_: Exception) {}
    reader = null
    val session = projection
    projection = null
    projectionCallback?.let { callback ->
      try { session?.unregisterCallback(callback) } catch (_: Exception) {}
    }
    projectionCallback = null
    try { session?.stop() } catch (_: Exception) {}
    try { worker?.quitSafely() } catch (_: Exception) {}
    worker = null
    request?.let {
      OneShotCapturePlugin.finish(it, base64, width, height, count,
        reason ?: "Screen capture ended")
    }
    request = null
    stopSelf()
  }

  override fun onDestroy() {
    terminate("Android stopped the screen capture service")
    super.onDestroy()
  }
}
