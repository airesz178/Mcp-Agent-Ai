package ai.mcpagent.app

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.SystemClock
import android.system.Os
import android.system.OsConstants
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.nio.charset.CodingErrorAction
import java.nio.ByteBuffer
import java.nio.charset.StandardCharsets
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

@InvokeArg
class AppTerminalRunArgs {
  lateinit var requestId: String
  lateinit var command: String
  var name: String? = null
  var userInitiated: Boolean = false
}

@InvokeArg
class AppTerminalCancelArgs {
  lateinit var requestId: String
  var userInitiated: Boolean = false
}

@InvokeArg
class TermuxUserActionArgs {
  var userInitiated: Boolean = false
}

/**
 * Explicitly invoked read-only Android terminal, not a general system shell.
 *
 * Scope: filesDir/terminal-public, a dedicated app-private folder that never
 * contains chat history, credentials, provider keys, or app control files.
 * This plugin never copies files into it or opens the parent app data directory.
 *
 * pwd / ls: real ProcessBuilder("/system/bin/toybox", ...); fixed argv only.
 * stat / cat: safe OS-fd equivalents using O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC
 * and fstat, so renaming a .txt file to a symlink/FIFO cannot escape the scope.
 * No arbitrary path, shell, env interpolation, executable or option argument.
 * Single active request per app process, 5s watchdog, 16 KiB combined output.
 * The only OS permissions for the sandbox are standard app-private data access.
 * Termux handoff below requires foreground user action and cannot auto-run.
 */
@TauriPlugin
class AppTerminalPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val TOYBOX = "/system/bin/toybox"
    private const val SANDBOX = "terminal-public"
    private const val TERMUX_PACKAGE = "com.termux"
    private const val TERMUX_ACTIVITY = "com.termux.app.TermuxActivity"
    private const val TERMUX_INSTALL_URL = "https://f-droid.org/packages/com.termux/"
    private const val MAX_OUTPUT = 16 * 1024
    private const val MAX_DURATION_MS = 5_000L
    private val busy = AtomicBoolean(false)
    private val job = AtomicReference<Running?>(null)
    private val executor = Executors.newSingleThreadExecutor { r ->
      Thread(r, "cos-app-terminal").apply { isDaemon = true }
    }
    private val watchdog = Executors.newSingleThreadScheduledExecutor { r ->
      Thread(r, "cos-app-terminal-timeout").apply { isDaemon = true }
    }
    // Filenames are one non-hidden .txt segment; shell metacharacters disallowed.
    private val SAFE_NAME = Regex("^[A-Za-z0-9][A-Za-z0-9._-]{0,59}\\.txt$")
    private val UUID_PATTERN =
      Regex("^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
    private val RESERVED = Regex("(?i)(secret|token|key|history|session|credential|config|private|project|auth|password|backup)")
  }

  private class Running(val id: String) {
    val cancelled = AtomicBoolean(false)
    val timedOut = AtomicBoolean(false)
    val process = AtomicReference<Process?>(null)
    fun stop() {
      cancelled.set(true)
      process.get()?.destroyForcibly()
    }
  }

  private fun supported(): Boolean =
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
      File(TOYBOX).isFile && File(TOYBOX).canExecute()

  private fun root(): File {
    val parent = activity.filesDir.canonicalFile
    if (!parent.isDirectory || java.nio.file.Files.isSymbolicLink(parent.toPath()))
      throw SecurityException("Private app-data directory unavailable")
    val dir = File(parent, SANDBOX)
    if (!dir.exists() && !dir.mkdir()) throw SecurityException("Cannot prepare terminal directory")
    if (!dir.isDirectory || java.nio.file.Files.isSymbolicLink(dir.toPath()) ||
      dir.canonicalFile != dir.absoluteFile ||
      dir.parentFile?.canonicalFile != parent
    ) throw SecurityException("Terminal directory is not private and canonical")
    dir.setReadable(false, false)
    dir.setWritable(false, false)
    dir.setExecutable(false, false)
    if (!dir.setReadable(true, true) || !dir.setWritable(true, true) ||
      !dir.setExecutable(true, true)) throw SecurityException("Cannot restrict terminal directory")
    return dir
  }

  private fun checkedName(name: String?): String {
    val value = name ?: throw SecurityException("A .txt filename is required")
    if (value.length > 64 || !SAFE_NAME.matches(value) || RESERVED.containsMatchIn(value))
      throw SecurityException("Only simple non-sensitive .txt filenames are permitted")
    return value
  }

  private fun validId(id: String) = UUID_PATTERN.matches(id)

  private fun entry(root: File, name: String): File {
    val file = File(root, checkedName(name))
    // The sandbox has no child directories; an exact one-component name only.
    if (file.parentFile != root || java.nio.file.Files.isSymbolicLink(file.toPath()))
      throw SecurityException("Links and nested paths are forbidden")
    return file
  }

  private fun statusObject(): JSObject = JSObject().apply {
    put("supported", supported())
    put("permissionRequired", false)
    put("permissionGranted", supported()) // app-private access, NOT broad storage
    put("scope", "Dedicated app-private terminal-public folder")
    put("running", busy.get())
    put("maxOutputBytes", MAX_OUTPUT)
    put("maxDurationMs", MAX_DURATION_MS)
    put("maxConcurrency", 1)
    put("commands", listOf("pwd", "ls", "stat", "cat"))
    put("userInitiatedRequired", true)
    put("autoRun", false)
  }

  @Command
  fun status(invoke: Invoke) { invoke.resolve(statusObject()) }

  // A distinct, real terminal owned by Termux. Android apps cannot attach to
  // another app's PTY or execute its private binaries. This handoff never
  // represents the read-only app sandbox as an interactive Linux shell.
  private fun termuxLaunchIntent(): Intent? {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return null
    return try {
      val launch = activity.packageManager.getLaunchIntentForPackage(TERMUX_PACKAGE)
        ?: return null
      // Accept Termux's public launcher only; do not invoke an arbitrary
      // application-supplied activity or component returned by a resolver.
      val component = launch.component ?: return null
      if (component.packageName != TERMUX_PACKAGE ||
          component.className != TERMUX_ACTIVITY) return null
      Intent(Intent.ACTION_MAIN).apply {
        setClassName(TERMUX_PACKAGE, TERMUX_ACTIVITY)
        addCategory(Intent.CATEGORY_LAUNCHER)
      }
    } catch (_: Exception) {
      null
    }
  }

  private fun termuxInstalled(): Boolean = try {
    activity.packageManager.getPackageInfo(TERMUX_PACKAGE, 0)
    true
  } catch (_: Exception) {
    false
  }

  private fun termuxStatusObject(): JSObject {
    val launchable = termuxLaunchIntent() != null
    return JSObject().apply {
      put("supported", Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
      put("installed", termuxInstalled())
      put("launchable", launchable)
      put("mode", "external-termux-app")
      put("inAppShellAvailable", false)
      put("setupVerified", false)
      put("installUrl", TERMUX_INSTALL_URL)
      put("installIsUserManaged", true)
      put("userInitiatedRequired", true)
      put("requiresRunCommandPermission", false)
      put("commandsAvailableInApp", false)
      put("note", if (launchable)
        "Open Termux to initialize and use its real interactive shell. Shell setup is managed inside Termux."
        else "Install Termux via the official F-Droid listing, then open it to initialize its shell.")
    }
  }

  @Command
  fun termuxStatus(invoke: Invoke) { invoke.resolve(termuxStatusObject()) }

  private fun termuxAction(invoke: Invoke, openInstalled: Boolean) {
    val args = try { invoke.parseArgs(TermuxUserActionArgs::class.java) } catch (_: Exception) {
      invoke.reject("Invalid Termux request")
      return
    }
    if (!args.userInitiated) {
      invoke.reject("Opening Termux requires an explicit user action")
      return
    }
    activity.runOnUiThread {
      if (activity.isFinishing || activity.isDestroyed || !activity.hasWindowFocus()) {
        invoke.reject("Termux setup requires the app in the foreground")
        return@runOnUiThread
      }
      val target = if (openInstalled) termuxLaunchIntent() else Intent(
        Intent.ACTION_VIEW, Uri.parse(TERMUX_INSTALL_URL)
      ).addCategory(Intent.CATEGORY_BROWSABLE)
      if (target == null) {
        invoke.reject("Termux is not installed or its launcher is not visible")
        return@runOnUiThread
      }
      try {
        activity.startActivity(target)
        invoke.resolve(JSObject().apply {
          put("opened", true)
          put("destination", if (openInstalled) "termux" else "f-droid")
          put("installedByApp", false)
          put("shellStartedByApp", false)
          put("statusMustBeRefreshed", true)
        })
      } catch (_: ActivityNotFoundException) {
        invoke.reject("No app can open the Termux destination")
      } catch (_: SecurityException) {
        invoke.reject("Android blocked the Termux handoff")
      } catch (_: Exception) {
        invoke.reject("Termux could not be opened")
      }
    }
  }

  @Command
  fun termuxInstall(invoke: Invoke) { termuxAction(invoke, false) }

  @Command
  fun termuxOpen(invoke: Invoke) { termuxAction(invoke, true) }

  private fun checkedCommand(cmd: String, name: String?): String? {
    if (cmd != cmd.trim() || cmd.length > 4 || cmd !in listOf("pwd", "ls", "stat", "cat"))
      throw SecurityException("Unsupported terminal command")
    if (cmd == "pwd" || cmd == "ls") {
      if (name != null) throw SecurityException("This command does not accept arguments")
      return null
    }
    return checkedName(name)
  }

  @Command
  fun run(invoke: Invoke) {
    val args = try { invoke.parseArgs(AppTerminalRunArgs::class.java) } catch (_: Exception) {
      invoke.reject("Invalid terminal request")
      return
    }
    if (!args.userInitiated || !validId(args.requestId)) {
      invoke.reject("Terminal requires explicit user initiation and a canonical request UUID")
      return
    }
    if (!supported() || activity.isFinishing || activity.isDestroyed || !activity.hasWindowFocus()) {
      invoke.reject("Foreground Android terminal is unavailable")
      return
    }
    val safeName = try { checkedCommand(args.command, args.name) } catch (_: Exception) {
      invoke.reject("Terminal command or filename is not allowed")
      return
    }
    if (!busy.compareAndSet(false, true)) {
      invoke.reject("Terminal already has an active request")
      return
    }
    val current = Running(args.requestId)
    job.set(current)
    try {
      executor.execute {
        val started = SystemClock.elapsedRealtime()
        val timeout = watchdog.schedule({
          current.timedOut.set(true)
          current.stop()
        }, MAX_DURATION_MS, TimeUnit.MILLISECONDS)
        val lifecycle = watchdog.scheduleAtFixedRate({
          // A command started in the foreground must not continue in background
          // after Activity handoff/closure, even before the five-second deadline.
          if (activity.isFinishing || activity.isDestroyed || !activity.hasWindowFocus())
            current.stop()
        }, 150L, 150L, TimeUnit.MILLISECONDS)
        try {
          val dir = root()
          if (current.cancelled.get()) throw InterruptedException("Terminal stopped")
          val output = when (args.command) {
            "pwd", "ls" -> launchToybox(args.command, dir, current)
            "stat", "cat" -> readViaFd(args.command, entry(dir, safeName!!), current)
            else -> throw SecurityException("Unsupported command")
          }
          if (current.timedOut.get() || SystemClock.elapsedRealtime() - started > MAX_DURATION_MS)
            throw InterruptedException("Terminal exceeded five seconds")
          if (current.cancelled.get()) throw InterruptedException("Terminal stopped")
          invoke.resolve(JSObject().apply {
            put("requestId", current.id)
            put("command", args.command)
            put("output", output)
            put("readOnly", true)
            put("scope", "terminal-public")
            put("exitCode", 0)
          })
        } catch (_: Exception) {
          invoke.reject(
            if (current.timedOut.get()) "Terminal timed out"
            else if (current.cancelled.get()) "Terminal cancelled"
            else "Terminal command was rejected or could not complete safely"
          )
        } finally {
          timeout.cancel(false)
          lifecycle.cancel(false)
          current.process.getAndSet(null)?.destroyForcibly()
          job.compareAndSet(current, null)
          busy.set(false)
        }
      }
    } catch (_: Exception) {
      job.compareAndSet(current, null)
      busy.set(false)
      invoke.reject("Terminal worker unavailable")
    }
  }

  @Command
  fun cancel(invoke: Invoke) {
    val args = try { invoke.parseArgs(AppTerminalCancelArgs::class.java) } catch (_: Exception) {
      invoke.reject("Invalid terminal cancellation")
      return
    }
    if (!args.userInitiated || !validId(args.requestId)) {
      invoke.reject("Cancellation requires user initiation and a request UUID")
      return
    }
    val active = job.get()
    if (active == null || active.id != args.requestId) {
      invoke.reject("Matching active terminal request not found")
      return
    }
    active.stop()
    invoke.resolve(JSObject().apply {
      put("requestId", args.requestId)
      put("cancelRequested", true)
    })
  }

  private fun launchToybox(cmd: String, dir: File, running: Running): String {
    if (cmd == "ls") {
      val entries = dir.listFiles() ?: throw SecurityException("Cannot list isolated terminal")
      if (entries.size > 128 || entries.any {
        !SAFE_NAME.matches(it.name) || RESERVED.containsMatchIn(it.name) ||
          java.nio.file.Files.isSymbolicLink(it.toPath()) || !it.isFile
      }) throw SecurityException("Terminal contains an unexpected entry")
    }
    val args = if (cmd == "pwd") listOf(TOYBOX, "pwd") else listOf(TOYBOX, "ls", "-1", ".")
    val pb = ProcessBuilder(args).directory(dir).redirectErrorStream(true)
    pb.environment().clear()
    val proc = pb.start()
    running.process.set(proc)
    if (running.cancelled.get()) proc.destroyForcibly()
    val bytes = readBounded(proc.inputStream, running)
    if (!proc.waitFor(100, TimeUnit.MILLISECONDS) || proc.exitValue() != 0)
      throw SecurityException("Terminal exited unsuccessfully")
    return decode(bytes)
  }

  private fun readBounded(input: InputStream, running: Running): ByteArray {
    val out = ByteArrayOutputStream()
    val buffer = ByteArray(2048)
    input.use { stream ->
      while (true) {
        if (running.cancelled.get()) throw InterruptedException("Stopped")
        val count = stream.read(buffer)
        if (count < 0) break
        if (count > MAX_OUTPUT - out.size()) throw SecurityException("Terminal output limit exceeded")
        out.write(buffer, 0, count)
      }
    }
    return out.toByteArray()
  }

  private fun decode(bytes: ByteArray): String = StandardCharsets.UTF_8.newDecoder()
    .onMalformedInput(CodingErrorAction.REPORT)
    .onUnmappableCharacter(CodingErrorAction.REPORT)
    .decode(ByteBuffer.wrap(bytes)).toString()

  private fun readViaFd(cmd: String, file: File, running: Running): String {
    // O_NOFOLLOW protects the *opened* inode against a final-component symlink
    // swap after validation; O_NONBLOCK forbids FIFO/device indefinite waits.
    val flags = OsConstants.O_RDONLY or OsConstants.O_NOFOLLOW or
      OsConstants.O_NONBLOCK or OsConstants.O_CLOEXEC
    val fd = Os.open(file.absolutePath, flags, 0)
    try {
      val info = Os.fstat(fd)
      if (info.st_mode and OsConstants.S_IFMT != OsConstants.S_IFREG ||
        info.st_nlink != 1L ||
        info.st_size > MAX_OUTPUT || info.st_size < 0)
        throw SecurityException("Only bounded regular text files may be inspected")
      if (cmd == "stat") return "file: ${file.name}\nsize: ${info.st_size} bytes\ntype: regular file\n"
      val out = ByteArrayOutputStream()
      val buf = ByteArray(2048)
      while (true) {
        if (running.cancelled.get()) throw InterruptedException("Stopped")
        val got = Os.read(fd, buf, 0, minOf(buf.size, MAX_OUTPUT - out.size() + 1))
        if (got == 0) break
        if (got > MAX_OUTPUT - out.size()) throw SecurityException("Output limit exceeded")
        out.write(buf, 0, got)
      }
      return decode(out.toByteArray())
    } finally {
      Os.close(fd)
    }
  }
}
