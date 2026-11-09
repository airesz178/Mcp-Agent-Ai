package ai.mcpagent.app

/**
 * Pure, independently testable argument policy for the Termux RUN_COMMAND
 * bridge. No shell, stdin, interpolation, app-private path or arbitrary argv
 * can be supplied by a renderer/model. The Termux user creates any source
 * files under their own Termux home, independently of Mcp Agent Ai.
 */
internal data class TermuxRunSpec(
  val program: String,
  val action: String,
  val executable: String,
  val workingDirectory: String,
  val arguments: Array<String>,
  val humanLabel: String
)

internal data class TermuxRunOutcome(
  val exitCode: Int,
  val success: Boolean,
  val stdout: String,
  val stderr: String,
  val stdoutOriginalLength: Long,
  val stderrOriginalLength: Long
)

internal object TermuxRunPolicy {
  const val HOME = "/data/data/com.termux/files/home"
  private const val PREFIX = "/data/data/com.termux/files/usr/bin"
  private const val MAX_ARG_BYTES = 4096
  const val MAX_RESULT_BYTES = 16 * 1024
  private val PROJECT = Regex("^[a-z][a-z0-9_-]{0,39}$")
  private val SCRIPT = Regex("^[A-Za-z0-9][A-Za-z0-9_-]{0,62}\\.(?:js|mjs|cjs)$")
  private val USER = Regex("^[a-z_][a-z0-9_-]{0,31}$")
  private val HOST = Regex("^[a-zA-Z0-9][a-zA-Z0-9.-]{0,251}$")

  fun versionSupportsResult(versionName: String?): Boolean {
    // Fail closed on unknown/preview version formats. 0.109 is the documented
    // minimum for PendingIntent command results.
    val parts = Regex("^([0-9]+)\\.([0-9]+)(?:\\.([0-9]+))?$")
      .matchEntire(versionName ?: "") ?: return false
    val major = parts.groupValues[1].toIntOrNull() ?: return false
    val minor = parts.groupValues[2].toIntOrNull() ?: return false
    return major > 0 || (major == 0 && minor >= 109)
  }

  private fun noExtra(value: String?, name: String) {
    require(value == null) { "$name is unsupported for this action" }
  }

  private fun selectedProject(value: String?): String {
    require(value != null && PROJECT.matches(value)) { "Invalid Termux project slug" }
    return "$HOME/projects/$value"
  }

  private fun script(value: String?): String {
    require(value != null && SCRIPT.matches(value)) { "Only one Termux-owned JavaScript filename is allowed" }
    return "./$value"
  }

  private fun selectedHost(value: String?): String {
    require(value != null && HOST.matches(value) && value.length <= 253 &&
      !value.contains("..") && value.split('.').all {
        it.isNotEmpty() && it.length <= 63 && it.first().isLetterOrDigit() &&
          it.last().isLetterOrDigit()
      }) { "Invalid SSH target" }
    return value
  }

  fun build(
    program: String, action: String, project: String? = null,
    entry: String? = null, host: String? = null, username: String? = null,
    port: Int? = null
  ): TermuxRunSpec {
    require(program in setOf("node", "npm", "ssh")) { "Unsupported Termux executable" }
    require(action.length in 1..12 && action.all { it.isLowerCase() }) {
      "Unsupported Termux action"
    }
    var dir = HOME
    val argv: List<String> = when (program to action) {
      "node" to "version" -> {
        noExtra(project, "project"); noExtra(entry, "entry")
        listOf("--version")
      }
      "node" to "check", "node" to "run" -> {
        dir = selectedProject(project)
        val file = script(entry)
        if (action == "check") listOf("--check", file) else listOf("--", file)
      }
      "npm" to "version" -> {
        noExtra(project, "project"); noExtra(entry, "entry")
        listOf("--version")
      }
      "npm" to "list" -> {
        dir = selectedProject(project); noExtra(entry, "entry")
        listOf("ls", "--depth=0", "--offline", "--json")
      }
      "npm" to "install" -> {
        dir = selectedProject(project); noExtra(entry, "entry")
        // Offline-only packages and no lifecycle scripts: never install a
        // network package or run an install-time shell hook implicitly.
        listOf("install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund")
      }
      "ssh" to "version" -> {
        noExtra(project, "project"); noExtra(entry, "entry")
        listOf("-V")
      }
      "ssh" to "test" -> {
        noExtra(project, "project"); noExtra(entry, "entry")
        val user = username?.takeIf { USER.matches(it) }
          ?: throw IllegalArgumentException("Invalid SSH username")
        val selected = selectedHost(host)
        val sshPort = port?.takeIf { it in 1..65535 }
          ?: throw IllegalArgumentException("Invalid SSH port")
        // Only fixed remote command 'true'. No terminal/PTY, password prompt,
        // remote script, port forwarding, proxy/jump or user-supplied options.
        listOf(
          "-F", "/dev/null", "-T", "-oBatchMode=yes",
          "-oPasswordAuthentication=no", "-oKbdInteractiveAuthentication=no",
          "-oPreferredAuthentications=publickey", "-oStrictHostKeyChecking=yes",
          "-oForwardAgent=no", "-oForwardX11=no",
          "-oClearAllForwardings=yes", "-oPermitLocalCommand=no",
          "-oProxyCommand=none", "-oProxyJump=none",
          "-oConnectTimeout=8", "-oConnectionAttempts=1",
          "-p", sshPort.toString(), "$user@$selected", "true"
        )
      }
      else -> throw IllegalArgumentException("Unsupported Termux action")
    }
    if (program != "ssh" || action == "version") {
      noExtra(host, "host"); noExtra(username, "username")
      require(port == null) { "SSH port not accepted for this action" }
    }
    require(argv.size <= 24 &&
      argv.sumOf { it.toByteArray(Charsets.UTF_8).size } <= MAX_ARG_BYTES) {
      "Termux command argument limit exceeded"
    }
    return TermuxRunSpec(program, action, "$PREFIX/$program", dir,
      argv.toTypedArray(), "$program $action")
  }

  /** Public JNI boundary accepts ONE typed, single-string target rather than
   * separate freeform argv. The only slash permitted is the exact node
   * project/script separator, never a filesystem path. */
  fun fromTarget(tool: String, action: String, target: String?): TermuxRunSpec =
    when (tool to action) {
      "node" to "run", "node" to "check" -> {
        val match=Regex("^([a-z][a-z0-9_-]{0,39})/([A-Za-z0-9][A-Za-z0-9_-]{0,62}\\.(?:js|mjs|cjs))$")
          .matchEntire(target ?: "")
          ?: throw IllegalArgumentException("Node target must be project/script.js in Termux home")
        build(tool,action,project=match.groupValues[1],entry=match.groupValues[2])
      }
      "npm" to "list", "npm" to "install" ->
        build(tool,action,project=target)
      "ssh" to "test" -> {
        val match=Regex("^([a-z_][a-z0-9_-]{0,31})@([A-Za-z0-9][A-Za-z0-9.-]{0,251}):([0-9]{1,5})$")
          .matchEntire(target ?: "")
          ?: throw IllegalArgumentException("SSH target must be user@host:port")
        build(tool,action,host=match.groupValues[2],username=match.groupValues[1],
          port=match.groupValues[3].toIntOrNull())
      }
      else -> {
        require(target == null) { "Unexpected target for this Termux action" }
        build(tool,action)
      }
    }

  /** Termux may truncate stdout/stderr to 100KiB before PendingIntent.send.
   * Never call a truncated, malformed, oversized or internal-error result
   * verified. A nonzero real exit code is a completed but failed process. */
  fun verifiedResult(
    exitCode: Int?, termuxError: Int?, stdout: String?,
    stderr: String?, originalStdout: Long?, originalStderr: Long?
  ): TermuxRunOutcome {
    require(exitCode != null && termuxError == -1) {
      "Termux did not confirm a completed process"
    }
    require(originalStdout != null && originalStderr != null &&
      originalStdout in 0..MAX_RESULT_BYTES.toLong() &&
      originalStderr in 0..MAX_RESULT_BYTES.toLong() &&
      originalStdout + originalStderr <= MAX_RESULT_BYTES.toLong()) {
      "Termux result length metadata is unavailable or exceeds the limit"
    }
    val out = stdout ?: ""
    val err = stderr ?: ""
    val outBytes=out.toByteArray(Charsets.UTF_8).size
    val errBytes=err.toByteArray(Charsets.UTF_8).size
    val actualOutputSize = outBytes + errBytes
    // The original length may refer to Java String characters or UTF-8
    // bytes depending on Termux's result-sender version. Either way it must
    // match the returned untruncated value exactly, not merely be >= it.
    val intactStdout = originalStdout == out.length.toLong() ||
      originalStdout == outBytes.toLong()
    val intactStderr = originalStderr == err.length.toLong() ||
      originalStderr == errBytes.toLong()
    require(actualOutputSize <= MAX_RESULT_BYTES &&
      intactStdout && intactStderr) {
      "Termux output was truncated or invalid"
    }
    return TermuxRunOutcome(exitCode, exitCode == 0, out, err,
      originalStdout, originalStderr)
  }
}
