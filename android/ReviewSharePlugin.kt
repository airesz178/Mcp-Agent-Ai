package ai.mcpagent.app

import android.app.Activity
import android.content.Intent
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class ReviewShareMarkdownArgs {
  var text: String = ""
  var userInitiated: Boolean = false
}

/**
 * Opens Android's system share chooser only after an explicit caller gesture.
 * The outbound payload is plain text: no URI, attachment, file, persistence,
 * clipboard access, background work or intent receiver in this plugin.
 */
@TauriPlugin
class ReviewSharePlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val MAX_MARKDOWN_BYTES = 128 * 1024
    private const val SESSION_HEADER = "# Mcp Agent Ai conversation\n\nProvider: "

    // Common key formats. Redaction is best-effort; users must review text
    // before sharing because arbitrary prose can still contain private data.
    private val directSecrets = listOf(
      Regex("\\b(?:sk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_]{16,}|github_pat_[A-Za-z0-9_]{16,}|glpat-[A-Za-z0-9_-]{16,}|xox[baprs]-[A-Za-z0-9-]{12,}|AIza[0-9A-Za-z_-]{20,})\\b"),
      Regex("\\b(Bearer\\s+)[A-Za-z0-9._~+/=-]{8,}", RegexOption.IGNORE_CASE),
      Regex("(?i)(\\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|password|secret|token|authorization)\\b[\\\"']?\\s*[:=]\\s*[\\\"']?)([^\\s\\\"';,`]{6,})")
    )

    internal fun redactCredentials(markdown: String): String {
      var text = directSecrets[0].replace(markdown, "[REDACTED]")
      text = directSecrets[1].replace(text) { match -> match.groupValues[1] + "[REDACTED]" }
      text = directSecrets[2].replace(text) { match -> match.groupValues[1] + "[REDACTED]" }
      return text
    }

    internal fun validateMarkdown(markdown: String): Boolean {
      // One UTF-16 code unit always contributes at least one UTF-8 byte, so
      // this is a cheap bound before encoding to confirm actual byte size.
      if (markdown.length > MAX_MARKDOWN_BYTES || !markdown.startsWith(SESSION_HEADER) ||
        !markdown.contains("\nModel: ") || !markdown.contains("\n\n## ") ||
        markdown.any { it == '\u0000' || (it.isISOControl() && it !in "\r\n\t") }) return false

      var pos = 0
      while (pos < markdown.length) {
        val ch = markdown[pos]
        if (Character.isHighSurrogate(ch)) {
          if (++pos >= markdown.length || !Character.isLowSurrogate(markdown[pos])) return false
        } else if (Character.isLowSurrogate(ch)) return false
        pos++
      }
      return markdown.toByteArray(Charsets.UTF_8).size <= MAX_MARKDOWN_BYTES
    }
  }

  /** Caller must pass userInitiated=true for this exact UI action. */
  @Command
  fun shareMarkdown(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(ReviewShareMarkdownArgs::class.java)
    } catch (_: Exception) {
      invoke.reject("Invalid Markdown share request")
      return
    }
    if (!args.userInitiated) {
      invoke.reject("Sharing requires an explicit user action")
      return
    }
    if (!validateMarkdown(args.text)) {
      invoke.reject("Invalid or oversized conversation Markdown (128 KiB max)")
      return
    }
    val safeText = redactCredentials(args.text)
    if (!validateMarkdown(safeText)) {
      invoke.reject("Could not prepare bounded conversation Markdown")
      return
    }
    activity.runOnUiThread {
      try {
        val send = Intent(Intent.ACTION_SEND).apply {
          type = "text/plain"
          putExtra(Intent.EXTRA_TEXT, safeText)
        }
        activity.startActivity(Intent.createChooser(send, "Share conversation as text"))
        invoke.resolve(JSObject().apply { put("opened", true) })
      } catch (_: Exception) {
        invoke.reject("Android share chooser is unavailable")
      }
    }
  }
}
