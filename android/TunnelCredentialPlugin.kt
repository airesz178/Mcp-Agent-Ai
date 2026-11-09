package ai.mcpagent.app

import android.app.Activity
import android.net.Uri
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.browser.customtabs.CustomTabsIntent
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.nio.charset.StandardCharsets
import java.security.KeyStore
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
class TunnelCredentialStoreArgs {
  var apiKey: String = ""
  var userInitiated: Boolean = false
}

@InvokeArg
class TunnelCredentialConsentArgs {
  var userInitiated: Boolean = false
}

@InvokeArg
class TunnelPlatformPageArgs {
  var kind: String = ""
  var userInitiated: Boolean = false
}

/** Android 8+ non-exportable AES-256-GCM Keystore credential storage.
 *
 * Only the IV and authenticated ciphertext are written to app-private
 * SharedPreferences. There is no plaintext fallback, no logging, no browser
 * WebView sign-in, no automatic tunnel connection and no exposed key in status.
 * The readForConnect command is an internal Kotlin -> Rust bridge only: Rust
 * never forwards its result through any Tauri JS command or status response.
 */
@TauriPlugin
class TunnelCredentialPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val KEYSTORE = "AndroidKeyStore"
    private const val ALIAS = "ai.mcpagent.app.openai_tunnel_aes256gcm_v1"
    private const val PREFS = "mcp_openai_tunnel_sealed_v1"
    private const val IV = "iv_b64"
    private const val CIPHERTEXT = "ciphertext_b64"
    private const val GCM_TRANSFORMATION = "AES/GCM/NoPadding"
    private const val MAX_KEY_LENGTH = 512
    private const val MIN_KEY_LENGTH = 8
    private const val GCM_IV_BYTES = 12
    private const val GCM_TAG_BITS = 128
    private const val MAX_CIPHERTEXT_BYTES = MAX_KEY_LENGTH + GCM_TAG_BITS / 8
    private const val TUNNELS_URL = "https://platform.openai.com/settings/organization/tunnels"
    private const val KEYS_URL = "https://platform.openai.com/settings/organization/api-keys"
    private const val CHATGPT_PLUGINS_URL = "https://chatgpt.com/plugins"
  }

  // One finite worker thread keeps Keystore and SharedPreferences operations
  // ordered without blocking the Android Activity's UI thread.
  private val worker = ThreadPoolExecutor(
    1, 1, 0L, TimeUnit.MILLISECONDS, ArrayBlockingQueue<Runnable>(2),
    { job -> Thread(job, "mcp-tunnel-credentials").apply { isDaemon = true } },
    ThreadPoolExecutor.AbortPolicy()
  )

  private fun preferences() = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
  private fun keystore(): KeyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }

  private fun existingKey(): SecretKey? {
    val key = keystore().getKey(ALIAS, null) ?: return null
    return key as? SecretKey
      ?: throw SecurityException("Invalid Android Keystore entry")
  }

  private fun keyForStore(): SecretKey {
    existingKey()?.let { return it }
    val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
    generator.init(KeyGenParameterSpec.Builder(
      ALIAS,
      KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
    ).setKeySize(256)
      .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
      .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
      .setRandomizedEncryptionRequired(true)
      .build())
    return generator.generateKey()
  }

  private fun validKey(key: String): Boolean =
    key.length in MIN_KEY_LENGTH..MAX_KEY_LENGTH &&
      key.all { it.code in 0x21..0x7e }

  private fun sealedPresent(): Boolean {
    val prefs = preferences()
    return prefs.contains(IV) || prefs.contains(CIPHERTEXT)
  }

  /** Reject corrupted/missing Keystore ciphertext; never return a partial key. */
  private fun unseal(): String? {
    val prefs = preferences()
    if (!prefs.contains(IV) && !prefs.contains(CIPHERTEXT)) return null
    val nonceB64 = prefs.getString(IV, null)
      ?: throw SecurityException("Tunnel credential is incomplete")
    val ciphertextB64 = prefs.getString(CIPHERTEXT, null)
      ?: throw SecurityException("Tunnel credential is incomplete")
    if (nonceB64.length > 64 || ciphertextB64.length > 1024)
      throw SecurityException("Tunnel ciphertext is invalid")
    val nonce = Base64.decode(nonceB64, Base64.NO_WRAP)
    val encrypted = Base64.decode(ciphertextB64, Base64.NO_WRAP)
    if (nonce.size != GCM_IV_BYTES ||
      encrypted.size !in (MIN_KEY_LENGTH + GCM_TAG_BITS / 8)..MAX_CIPHERTEXT_BYTES)
      throw SecurityException("Tunnel ciphertext is invalid")
    val key = existingKey()
      ?: throw SecurityException("Tunnel credential cannot be decrypted")
    val cipher = Cipher.getInstance(GCM_TRANSFORMATION)
    cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, nonce))
    val plaintext = cipher.doFinal(encrypted)
    try {
      val result = String(plaintext, StandardCharsets.US_ASCII)
      if (!validKey(result)) throw SecurityException("Tunnel credential is invalid")
      return result
    } finally {
      plaintext.fill(0)
    }
  }

  /** Return boolean health only. Never publish ciphertext, IV or plaintext. */
  private fun healthy(): Boolean = try {
    unseal() != null
  } catch (_: Exception) {
    false
  }

  private fun statusReceipt(stored: Boolean): JSObject = JSObject().apply {
    put("stored", stored)
    put("secure", true)
  }

  private fun execute(invoke: Invoke, action: () -> JSObject) {
    try {
      worker.execute {
        try {
          invoke.resolve(action())
        } catch (_: Exception) {
          // Never include a platform exception, plaintext key, path or stack
          // trace in a plugin error visible to the Rust bridge.
          invoke.reject("Android secure tunnel credential operation unavailable")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android secure tunnel credential operation busy")
    }
  }

  @Command
  fun status(invoke: Invoke) {
    execute(invoke) { statusReceipt(healthy()) }
  }

  @Command
  fun store(invoke: Invoke) {
    val args = try { invoke.parseArgs(TunnelCredentialStoreArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid tunnel credential request"); return }
    if (!args.userInitiated || !validKey(args.apiKey)) {
      invoke.reject("Explicit user action and a valid tunnel key are required")
      return
    }
    execute(invoke) {
      // Never silently rotate an invalid/missing Keystore binding and thereby
      // hide failed decryption; the user must explicitly remove it first.
      if (sealedPresent() && !healthy()) {
        throw SecurityException("Stored tunnel credential is inaccessible")
      }
      val plaintext = args.apiKey.toByteArray(StandardCharsets.US_ASCII)
      try {
        val cipher = Cipher.getInstance(GCM_TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, keyForStore())
        val nonce = cipher.iv
        if (nonce.size != GCM_IV_BYTES) {
          throw SecurityException("Unexpected Android Keystore nonce")
        }
        val encrypted = cipher.doFinal(plaintext)
        if (encrypted.size > MAX_CIPHERTEXT_BYTES) {
          throw SecurityException("Invalid tunnel encrypted payload")
        }
        val committed = preferences().edit()
          .putString(IV, Base64.encodeToString(nonce, Base64.NO_WRAP))
          .putString(CIPHERTEXT, Base64.encodeToString(encrypted, Base64.NO_WRAP))
          .commit()
        if (!committed || !healthy()) {
          throw SecurityException("Tunnel credential was not stored securely")
        }
        statusReceipt(true)
      } finally {
        plaintext.fill(0)
      }
    }
  }

  @Command
  fun remove(invoke: Invoke) {
    val args = try { invoke.parseArgs(TunnelCredentialConsentArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid tunnel credential request"); return }
    if (!args.userInitiated) {
      invoke.reject("Explicit user action required to remove tunnel key")
      return
    }
    execute(invoke) {
      if (!preferences().edit().remove(IV).remove(CIPHERTEXT).commit()) {
        throw SecurityException("Tunnel credential removal failed")
      }
      // Removing the Keystore alias makes even old ciphertext undecryptable.
      val store = keystore()
      if (store.containsAlias(ALIAS)) store.deleteEntry(ALIAS)
      statusReceipt(false)
    }
  }

  @Command
  fun readForConnect(invoke: Invoke) {
    val args = try { invoke.parseArgs(TunnelCredentialConsentArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid tunnel credential request"); return }
    if (!args.userInitiated) {
      invoke.reject("Explicit user action required to use stored tunnel key")
      return
    }
    execute(invoke) {
      val key = unseal()
        ?: throw SecurityException("No tunnel credential stored")
      // This response is ONLY consumed inside Rust's private mobile plugin
      // handle and is NEVER returned from a registered Tauri UI command.
      JSObject().apply { put("apiKey", key) }
    }
  }

  @Command
  fun openPlatformPage(invoke: Invoke) {
    val args = try { invoke.parseArgs(TunnelPlatformPageArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid OpenAI Platform page request"); return }
    val url = when (args.kind) {
      "tunnels" -> TUNNELS_URL
      "api_keys" -> KEYS_URL
      "chatgpt_plugins" -> CHATGPT_PLUGINS_URL
      else -> { invoke.reject("Unsupported OpenAI Platform destination"); return }
    }
    if (!args.userInitiated) {
      invoke.reject("Explicit user action required to open OpenAI Platform")
      return
    }
    activity.runOnUiThread {
      try {
        // Custom Tabs, not an embedded WebView. There is no arbitrary URL input
        // and browser sign-in stays owned by the chosen Android browser.
        CustomTabsIntent.Builder().build().launchUrl(activity, Uri.parse(url))
        invoke.resolve(JSObject().apply {
          put("opened", true)
          put("kind", args.kind)
        })
      } catch (_: Exception) {
        invoke.reject("OpenAI Platform browser could not be opened")
      }
    }
  }
}
