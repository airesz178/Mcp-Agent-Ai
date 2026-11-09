package ai.mcpagent.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.Process

/**
 * Termux-style session foreground lifetime for the in-process Android runtime.
 *
 * It deliberately shares the app's default process with Tauri/Rust. While
 * Android permits this foreground service to run, it keeps that process in a
 * foreground-service importance class even if the Activity is backgrounded.
 * It does NOT restart the native runtime without a Tauri Activity, retry MCP
 * calls, run an undisclosed job or change OS permissions. The optional partial
 * wake lock is acquired ONLY by tapping the notification action.
 *
 * The explicit Exit action terminates this session and app process; saved app
 * data is never erased. Android ultimately controls process lifetime and this
 * is not a guarantee of uninterrupted 24/7 work.
 */
class ForegroundRuntimeService : Service() {
  companion object {
    const val ACTION_START = "ai.mcpagent.app.foreground.START"
    const val ACTION_HIDE = "ai.mcpagent.app.foreground.HIDE"
    const val ACTION_STOP = "ai.mcpagent.app.foreground.STOP"
    const val ACTION_EXIT = "ai.mcpagent.app.foreground.EXIT"
    const val ACTION_WAKE_LOCK = "ai.mcpagent.app.foreground.WAKE_LOCK"
    const val ACTION_WAKE_UNLOCK = "ai.mcpagent.app.foreground.WAKE_UNLOCK"
    private const val CHANNEL_ID = "mcp_agent_ai_sessions_v1"
    private const val NOTIFICATION_ID = 4982
    @Volatile private var foregroundActive = false
    @Volatile private var wakeLockHeld = false
    @Volatile private var activityVisible = true

    fun isRunning(): Boolean = foregroundActive
    fun isWakeLockHeld(): Boolean = wakeLockHeld
    fun setActivityVisible(visible: Boolean) { activityVisible = visible }
    fun notificationEnabled(context: Context): Boolean =
      (context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
        .areNotificationsEnabled()
  }

  private var partialWakeLock: PowerManager.WakeLock? = null
  private val main = Handler(Looper.getMainLooper())
  override fun onBind(intent: Intent?): IBinder? = null

  private fun notification(): Notification {
    val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      manager.createNotificationChannel(NotificationChannel(
        CHANNEL_ID, "Mcp Agent Ai sessions", NotificationManager.IMPORTANCE_DEFAULT
      ).apply {
        description = "Active Mcp Agent Ai session, Exit and optional CPU wake lock"
        setSound(null, null)
        enableVibration(false)
        setShowBadge(false)
      })
    }
    val open = Intent(this, MainActivity::class.java).apply {
      flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
    }
    val exit = Intent(this, ForegroundRuntimeService::class.java).setAction(ACTION_EXIT)
    val wake = Intent(this, ForegroundRuntimeService::class.java).setAction(
      if (wakeLockHeld) ACTION_WAKE_UNLOCK else ACTION_WAKE_LOCK)
    val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
    val openPending = PendingIntent.getActivity(this, 4982, open, flags)
    val exitPending = PendingIntent.getService(this, 4983, exit, flags)
    val wakePending = PendingIntent.getService(this,
      if (wakeLockHeld) 4985 else 4984, wake, flags)
    val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
      Notification.Builder(this, CHANNEL_ID)
    else Notification.Builder(this)
    return builder
      .setSmallIcon(android.R.drawable.ic_menu_manage)
      .setContentTitle("Mcp Agent Ai")
      .setContentText(if (wakeLockHeld) "1 session (wake lock held)" else "1 session")
      .setContentIntent(openPending)
      .setCategory(Notification.CATEGORY_SERVICE)
      .setOnlyAlertOnce(true)
      .setOngoing(true)
      .setShowWhen(false)
      .addAction(Notification.Action.Builder(
        android.R.drawable.ic_delete, "Exit", exitPending).build())
      .addAction(Notification.Action.Builder(
        android.R.drawable.ic_lock_idle_lock,
        if (wakeLockHeld) "Release wakelock" else "Acquire wakelock",
        wakePending).build())
      .build()
  }

  private fun acquireWakeLock() {
    if (wakeLockHeld || !foregroundActive) return
    try {
      val manager = getSystemService(Context.POWER_SERVICE) as PowerManager
      partialWakeLock = manager.newWakeLock(
        PowerManager.PARTIAL_WAKE_LOCK, "ai.mcpagent.app:session").apply {
        setReferenceCounted(false)
        acquire()
      }
      wakeLockHeld = partialWakeLock?.isHeld == true
    } catch (_: RuntimeException) {
      wakeLockHeld = false
      partialWakeLock = null
    }
    updateNotification()
  }

  private fun releaseWakeLock() {
    try {
      if (partialWakeLock?.isHeld == true) partialWakeLock?.release()
    } catch (_: RuntimeException) {}
    partialWakeLock = null
    wakeLockHeld = false
  }

  private fun updateNotification() {
    if (foregroundActive)
      (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
        .notify(NOTIFICATION_ID, notification())
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    when (intent?.action) {
      ACTION_START -> {
        try {
          val visible = notification()
          if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION_ID, visible,
              ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
          } else {
            startForeground(NOTIFICATION_ID, visible)
          }
          foregroundActive = true
          // START can arrive after a rapid pause/resume. Never leave the
          // notification visible while the Activity is back on screen.
          if (activityVisible) hideForegroundNotification()
        } catch (_: RuntimeException) {
          // No fake "running" receipt after an OS permission/type failure.
          foregroundActive = false
          stopSelf(startId)
        }
      }
      ACTION_HIDE -> {
        // A delayed resume intent must not hide the notification if the
        // Activity has already been backgrounded again.
        if (activityVisible) hideForegroundNotification()
      }
      ACTION_STOP -> stopRunning(startId)
      ACTION_WAKE_LOCK -> {
        if (foregroundActive) acquireWakeLock() else stopSelf(startId)
      }
      ACTION_WAKE_UNLOCK -> {
        if (foregroundActive) {
          releaseWakeLock()
          updateNotification()
        } else stopSelf(startId)
      }
      ACTION_EXIT -> {
        // Unlike merely dismissing a notification, Termux's Exit ends the
        // actual process/session. No app data, permissions or OS settings are
        // cleared. Never relaunch from a background receiver.
        stopRunning(startId)
        main.postDelayed({ Process.killProcess(Process.myPid()) }, 180L)
      }
      else -> {
        // Deliberately NOT_STICKY. The native Tauri/Rust runtime is initialized
        // by the Activity and cannot be truthfully recreated by Service alone.
        stopRunning(startId)
      }
    }
    return START_NOT_STICKY
  }

  private fun hideForegroundNotification() {
    if (!foregroundActive) return
    foregroundActive = false
    stopForeground(STOP_FOREGROUND_REMOVE)
    // Keep the started service and any user-held wake lock. The next
    // Activity pause promotes this same session back into foreground mode.
  }

  private fun stopRunning(startId: Int) {
    releaseWakeLock()
    foregroundActive = false
    stopForeground(STOP_FOREGROUND_REMOVE)
    stopSelf(startId)
  }

  override fun onDestroy() {
    releaseWakeLock()
    foregroundActive = false
    super.onDestroy()
  }
}
