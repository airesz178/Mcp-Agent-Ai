package ai.mcpagent.app

import android.app.Activity
import android.os.Build
import android.os.Environment
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.nio.file.Files
import java.util.Locale
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import org.json.JSONArray

@InvokeArg
class MainStorageListArgs {
  var path: List<String> = emptyList()
}

/** Read-only listing of the Android device's primary shared-storage volume.
 * All-files access is granted by Android Settings, never by this plugin.
 * Android/data and Android/obb remain inaccessible even after that grant.
 * SAF access is a separate capability managed by DocumentTreePlugin.
 */
@TauriPlugin
class MainStoragePlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val MAX_DEPTH = 16
    private const val MAX_ENTRIES = 128
    private const val MAX_NAME_LENGTH = 255
    private const val SELECTION_PREFS = "mcp_primary_storage_selection_v1"
    private const val SELECTION_KEY = "selected_relative_path"
    private const val MAX_STORED_PATH_LENGTH = 8192
  }

  private val listingWorker = ThreadPoolExecutor(
    1, 1, 0L, TimeUnit.MILLISECONDS, ArrayBlockingQueue<Runnable>(2),
    { task -> Thread(task, "mcp-main-storage-read").apply { isDaemon = true } },
    ThreadPoolExecutor.AbortPolicy()
  )

  private fun supported() = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
  private fun granted() = supported() && Environment.isExternalStorageManager()

  private fun validSegment(value: String): Boolean =
    value.isNotEmpty() && value.length <= MAX_NAME_LENGTH &&
      value != "." && value != ".." && !value.contains('/') &&
      !value.contains('\\') && value.none { Character.isISOControl(it) } &&
      value.indices.none { i ->
        Character.isHighSurrogate(value[i]) &&
          (i + 1 >= value.length || !Character.isLowSurrogate(value[i + 1])) ||
          Character.isLowSurrogate(value[i]) &&
          (i == 0 || !Character.isHighSurrogate(value[i - 1]))
      }

  /** Android's app-specific data and expansion directories are not shared
   * folders; do not bypass scoped-storage limitations through a path alias. */
  private fun restricted(path: List<String>): Boolean {
    if (path.isEmpty()) return false
    if (path[0].equals(".android_secure", ignoreCase = true)) return true
    return path.size >= 2 && path[0].equals("Android", ignoreCase = true) &&
      (path[1].equals("data", ignoreCase = true) ||
        path[1].equals("obb", ignoreCase = true))
  }

  private fun root(): File = Environment.getExternalStorageDirectory().canonicalFile

  private fun isInside(root: File, child: File): Boolean {
    val prefix = root.path.trimEnd(File.separatorChar) + File.separator
    return child.path == root.path || child.path.startsWith(prefix)
  }

  /** Canonical path checking is per segment, so a symlink pointing outside
   * (or into restricted Android directories) is never followed. */
  private fun resolveDirectory(base: File, path: List<String>): File {
    if (path.size > MAX_DEPTH || path.any { !validSegment(it) } || restricted(path))
      throw SecurityException("Invalid shared-storage path")
    var current = base
    for (segment in path) {
      val next = File(current, segment)
      if (Files.isSymbolicLink(next.toPath()))
        throw SecurityException("Linked folders are not accessible")
      val canonical = next.canonicalFile
      if (!isInside(base, canonical) || canonical.path != next.absolutePath)
        throw SecurityException("Shared-storage path is outside its root")
      current = canonical
    }
    if (!current.isDirectory || !current.canRead())
      throw SecurityException("Shared-storage folder cannot be read")
    return current
  }

  private fun selectionReceipt(path: List<String>, selected: Boolean): JSObject =
    JSObject().apply {
      val active = selected && path.isNotEmpty()
      put("path", JSONArray(if (active) path else emptyList<String>()))
      put("selected", active)
    }

  private fun selectedPath(): List<String>? {
    val stored = activity.getSharedPreferences(SELECTION_PREFS, Activity.MODE_PRIVATE)
      .getString(SELECTION_KEY, null) ?: return null
    if (stored.length > MAX_STORED_PATH_LENGTH) return null
    return try {
      val array = JSONArray(stored)
      // The primary root [] is always the unselected default. A persisted
      // selection can only refer to a real child folder.
      if (array.length() == 0 || array.length() > MAX_DEPTH) return null
      val path = ArrayList<String>(array.length())
      for (index in 0 until array.length()) {
        val segment = array.get(index)
        if (segment !is String || !validSegment(segment)) return null
        path.add(segment)
      }
      if (restricted(path)) null else path
    } catch (_: Exception) { null }
  }

  /** Root [] always means no selected folder. The selected flag is true only
   * after the current OS grant and filesystem access have been checked again,
   * including after an app restart.
   */
  @Command
  fun selection(invoke: Invoke) {
    try {
      listingWorker.execute {
        try {
          val prefs = activity.getSharedPreferences(SELECTION_PREFS, Activity.MODE_PRIVATE)
          val hadSelection = prefs.contains(SELECTION_KEY)
          val path = if (hadSelection) selectedPath() else null
          val accessible = path != null && granted() && try {
            resolveDirectory(root(), path)
            granted()
          } catch (_: Exception) { false }
          if (!accessible) {
            // An inaccessible or malformed stored choice must never resurrect
            // as selected after a later permission change.
            if (hadSelection) prefs.edit().remove(SELECTION_KEY).commit()
            invoke.resolve(selectionReceipt(emptyList(), false))
          } else {
            invoke.resolve(selectionReceipt(path!!, true))
          }
        } catch (_: Exception) {
          invoke.resolve(selectionReceipt(emptyList(), false))
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android primary storage selection is busy")
    }
  }

  @Command
  fun select(invoke: Invoke) {
    val path = try { invoke.parseArgs(MainStorageListArgs::class.java).path.toList() }
      catch (_: Exception) { invoke.reject("Invalid primary storage selection"); return }
    if (path.isEmpty() || path.size > MAX_DEPTH ||
      path.any { !validSegment(it) } || restricted(path)) {
      invoke.reject("Invalid primary storage selection")
      return
    }
    try {
      listingWorker.execute {
        var wroteNewSelection = false
        try {
          if (!granted()) throw SecurityException("Android All files access is not granted")
          val base = root()
          resolveDirectory(base, path)
          if (!granted()) throw SecurityException("Android All files access was revoked")
          val prefs = activity.getSharedPreferences(SELECTION_PREFS, Activity.MODE_PRIVATE)
          if (!prefs.edit().putString(SELECTION_KEY, JSONArray(path).toString()).commit())
            throw IllegalStateException("Primary storage selection could not be saved")
          wroteNewSelection = true
          // Recheck after persistence as folders can disappear while selecting.
          if (!granted()) throw SecurityException("Android All files access was revoked")
          resolveDirectory(root(), path)
          invoke.resolve(selectionReceipt(path, true))
        } catch (_: Exception) {
          // Failed validation leaves any previously selected folder alone.
          // If the newly persisted folder fails a fresh check, remove it.
          if (wroteNewSelection) {
            activity.getSharedPreferences(SELECTION_PREFS, Activity.MODE_PRIVATE)
              .edit().remove(SELECTION_KEY).commit()
          }
          invoke.reject("Primary storage folder could not be verified or saved")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android primary storage selection is busy")
    }
  }

  @Command
  fun reset(invoke: Invoke) {
    try {
      listingWorker.execute {
        try {
          val prefs = activity.getSharedPreferences(SELECTION_PREFS, Activity.MODE_PRIVATE)
          if (!prefs.edit().remove(SELECTION_KEY).commit())
            throw IllegalStateException("Primary storage selection could not be reset")
          invoke.resolve(selectionReceipt(emptyList(), false))
        } catch (_: Exception) {
          invoke.reject("Primary storage selection could not be reset")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android primary storage selection is busy")
    }
  }

  @Command
  fun status(invoke: Invoke) {
    val available = supported()
    val allowed = available && granted()
    val base = try { if (available) root() else null }
      catch (_: Exception) { null }
    val readable = allowed && base?.isDirectory == true && base?.canRead() == true
    invoke.resolve(JSObject().apply {
      put("supported", available)
      put("granted", allowed)
      put("readable", readable)
      put("rootPath", base?.path ?: "")
    })
  }

  @Command
  fun list(invoke: Invoke) {
    val path = try { invoke.parseArgs(MainStorageListArgs::class.java).path.toList() }
      catch (_: Exception) { invoke.reject("Invalid shared-storage path"); return }
    if (path.size > MAX_DEPTH || path.any { !validSegment(it) } || restricted(path)) {
      invoke.reject("Invalid shared-storage path")
      return
    }
    if (!granted()) {
      invoke.reject("Android All files access is not granted")
      return
    }
    try {
      listingWorker.execute {
        try {
          // A revoked Android grant invalidates an already queued listing.
          if (!granted()) throw SecurityException("Android All files access was revoked")
          val base = root()
          val folder = resolveDirectory(base, path)
          val entries = folder.listFiles()
            ?: throw SecurityException("Shared-storage directory listing denied")
          // Favor folders in a bounded listing so a directory containing
          // many files does not hide its navigable child folders.
          val sorted = entries.asSequence().filter { validSegment(it.name) }
            .sortedWith(compareBy<File>({ !it.isDirectory }, { it.name.lowercase(Locale.ROOT) }, { it.name }))
            .take(MAX_ENTRIES).toList()
          val results = JSONArray()
          for (item in sorted.take(MAX_ENTRIES)) {
            val childPath = path + item.name
            val safe = try {
              !restricted(childPath) &&
                !Files.isSymbolicLink(item.toPath()) &&
                isInside(base, item.canonicalFile) &&
                item.canonicalPath == item.absolutePath
            } catch (_: Exception) { false }
            results.put(JSObject().apply {
              put("name", item.name)
              put("isDirectory", item.isDirectory)
              // Preserve Android's actual canRead() result. Never present
              // Android/data, Android/obb or symlink targets as browsable.
              put("readable", safe && item.canRead())
            })
          }
          if (!granted()) throw SecurityException("Android All files access was revoked")
          invoke.resolve(JSObject().apply {
            put("path", JSONArray(path))
            put("entries", results)
          })
        } catch (_: Exception) {
          invoke.reject("Android shared-storage folder could not be listed")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android shared-storage listing is busy")
    }
  }
}
