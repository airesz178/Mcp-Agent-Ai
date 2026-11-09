package ai.mcpagent.app

import android.app.Activity
import android.app.NotificationManager
import android.Manifest
import android.content.Context
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.os.PowerManager
import android.provider.Settings
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class PermissionRequestArgs {
  lateinit var permission: String
}

@InvokeArg
class OptionalPermissionRequestArgs {
  var permission: String = ""
  var userInitiated: Boolean = false
}

/** Permission gate only. This plugin never reads a path, grants access, or
 * silently modifies Android app-ops. Files remain app-private in native-core. */
@TauriPlugin
class StorageAccessPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val CAMERA_REQUEST_CODE = 0x2910
    private const val MICROPHONE_REQUEST_CODE = 0x2911
    private const val LOCATION_REQUEST_CODE = 0x2912
  }

  private fun supported() = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
  private fun overlayGranted() = Settings.canDrawOverlays(activity)
  private fun notificationsGranted() =
    (activity.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
      .areNotificationsEnabled()
  private fun batteryUnrestricted() =
    (activity.getSystemService(Context.POWER_SERVICE) as PowerManager)
      .isIgnoringBatteryOptimizations(activity.packageName)
  private fun entry(supported: Boolean, granted: Boolean): JSObject {
    val item = JSObject()
    item.put("supported", supported)
    item.put("granted", supported && granted)
    return item
  }
  private fun allStatuses(): JSObject {
    val result = JSObject()
    result.put("files", entry(supported(), supported() && Environment.isExternalStorageManager()))
    result.put("overlay", entry(true, overlayGranted()))
    result.put("notifications", entry(true, notificationsGranted()))
    result.put("battery", entry(true, batteryUnrestricted()))
    return result
  }

  private fun optionalPermission(name: String): String? = when (name) {
    "camera" -> Manifest.permission.CAMERA
    "microphone" -> Manifest.permission.RECORD_AUDIO
    "location" -> Manifest.permission.ACCESS_COARSE_LOCATION
    else -> null
  }

  private fun optionalSupported(name: String): Boolean {
    val manager = activity.packageManager
    return when (name) {
      "camera" -> manager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)
      "microphone" -> manager.hasSystemFeature(PackageManager.FEATURE_MICROPHONE)
      "location" -> manager.hasSystemFeature(PackageManager.FEATURE_LOCATION) ||
        manager.hasSystemFeature(PackageManager.FEATURE_LOCATION_NETWORK)
      else -> false
    }
  }

  private fun optionalEntry(name: String): JSObject {
    val permission = optionalPermission(name)
    val available = permission != null && optionalSupported(name)
    return entry(available, available &&
      activity.checkSelfPermission(permission!!) == PackageManager.PERMISSION_GRANTED)
  }

  /** These three permissions are independent of the four legacy Setup grants.
   * A status read never prompts, and only PackageManager reports a grant. */
  @Command
  fun optionalPermissionsStatus(invoke: Invoke) {
    val result = JSObject()
    for (name in arrayOf("camera", "microphone", "location"))
      result.put(name, optionalEntry(name))
    invoke.resolve(result)
  }

  /** A direct Android runtime-permission sheet, strictly on a user gesture.
   * The opened receipt never asserts grant: callers must refresh status after
   * the Android permission sheet closes (including denial/revocation). */
  @Command
  fun optionalPermissionsRequest(invoke: Invoke) {
    val args = try { invoke.parseArgs(OptionalPermissionRequestArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid optional permission request"); return }
    val permission = optionalPermission(args.permission)
    if (!args.userInitiated || permission == null) {
      invoke.reject("Explicit user action and supported optional permission required")
      return
    }
    if (!optionalSupported(args.permission)) {
      invoke.reject("Optional permission unavailable on this device")
      return
    }
    val requestCode = when (args.permission) {
      "camera" -> CAMERA_REQUEST_CODE
      "microphone" -> MICROPHONE_REQUEST_CODE
      "location" -> LOCATION_REQUEST_CODE
      else -> { invoke.reject("Unsupported optional permission"); return }
    }
    activity.runOnUiThread {
      try {
        if (activity.isFinishing || activity.isDestroyed)
          throw IllegalStateException("Activity is unavailable")
        val alreadyGranted = activity.checkSelfPermission(permission) ==
          PackageManager.PERMISSION_GRANTED
        if (!alreadyGranted) activity.requestPermissions(arrayOf(permission), requestCode)
        val receipt = JSObject()
        receipt.put("opened", !alreadyGranted)
        invoke.resolve(receipt)
      } catch (_: Exception) {
        invoke.reject("Android optional permission prompt unavailable")
      }
    }
  }

  @Command
  fun status(invoke: Invoke) {
    val result = JSObject()
    result.put("supported", supported())
    result.put("granted", supported() && Environment.isExternalStorageManager())
    invoke.resolve(result)
  }

  @Command
  fun permissionsStatus(invoke: Invoke) {
    invoke.resolve(allStatuses())
  }

  @Command
  fun permissionsRequest(invoke: Invoke) {
    val key = try {
      invoke.parseArgs(PermissionRequestArgs::class.java).permission
    } catch (_: Exception) {
      invoke.reject("Invalid permission selection")
      return
    }
    val intent = when (key) {
      "files" -> {
        if (!supported()) {
          invoke.reject("All files access requires Android 11 or later")
          return
        }
        Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
          Uri.parse("package:${activity.packageName}"))
      }
      "overlay" -> Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
        Uri.parse("package:${activity.packageName}"))
      "notifications" -> Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
        .putExtra(Settings.EXTRA_APP_PACKAGE, activity.packageName)
      // The generic optimization list is not reliable on realme/ColorOS and
      // may omit Mcp Agent Ai entirely. Ask Android about THIS package only;
      // the system owns the confirmation dialog and the grant decision.
      "battery" -> Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
        Uri.parse("package:${activity.packageName}"))
      else -> {
        invoke.reject("Unsupported permission request")
        return
      }
    }
    try {
      try {
        activity.startActivity(intent)
      } catch (_: Exception) {
        // No alternate battery screen: the single Allow button must either
        // display Android's package-specific consent or report unavailability.
        if (key == "battery") {
          invoke.reject("Android could not open the battery exemption popup")
          return
        }
        // Other OEM Settings pages can still use a user-controlled fallback.
        val fallback = if (key == "files")
          Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)
        else Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
          Uri.parse("package:${activity.packageName}"))
        activity.startActivity(fallback)
      }
      val result = JSObject()
      result.put("opened", true)
      // Never equate opening a Settings activity with the user granting access.
      invoke.resolve(result)
    } catch (_: Exception) {
      invoke.reject("Could not open the Android permission settings")
    }
  }

  @Command
  fun request(invoke: Invoke) {
    if (!supported()) {
      invoke.reject("All files access requires Android 11 or later")
      return
    }
    try {
      val specific = Intent(
        Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
        Uri.parse("package:${activity.packageName}")
      )
      try {
        activity.startActivity(specific)
      } catch (_: ActivityNotFoundException) {
        activity.startActivity(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))
      }
      val response = JSObject()
      response.put("opened", true)
      // Opening Settings must NEVER be interpreted as user granting permission.
      response.put("granted", Environment.isExternalStorageManager())
      invoke.resolve(response)
    } catch (_: Exception) {
      invoke.reject("Could not open Android file access settings")
    }
  }
}
