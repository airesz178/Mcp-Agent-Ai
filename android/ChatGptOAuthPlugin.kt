package ai.mcpagent.app

import android.app.Activity
import android.content.Context
import android.net.Uri
import android.os.Handler
import android.os.Looper
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
import org.json.JSONArray
import org.json.JSONObject
import java.io.BufferedReader
import java.io.InputStreamReader
import java.math.BigInteger
import java.net.HttpURLConnection
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketTimeoutException
import java.net.URL
import java.net.URLEncoder
import java.nio.charset.StandardCharsets
import java.security.KeyFactory
import java.security.KeyStore
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.Signature
import java.security.spec.RSAPublicKeySpec
import java.util.UUID
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
class ChatGptOAuthArgs {
  var userInitiated: Boolean = false
}

class ChatGptChatTurn {
  var role: String = ""
  var content: String = ""
}

@InvokeArg
class ChatGptChatArgs {
  var requestId: String = ""
  var prompt: String = ""
  var context: List<ChatGptChatTurn> = emptyList()
  var userInitiated: Boolean = false
}

/**
 * Official Sign in with ChatGPT for a locally run open-source Android app.
 *
 * Uses a PUBLIC OAuth client with PKCE and a device-local loopback callback.
 * It never reads Chrome's cookies, drives ChatGPT's private backend API, or
 * copies a ChatGPT website session into the Android WebView. Only bounded
 * status booleans cross the Tauri bridge; credentials are encrypted with an
 * AndroidKeyStore key inaccessible to JavaScript.
 */
@TauriPlugin
class ChatGptOAuthPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val AUTH = "https://auth.openai.com"
    private const val AUTHORIZE = "$AUTH/api/accounts/authorize"
    private const val TOKEN = "$AUTH/api/accounts/oauth/token"
    private const val JWKS = "$AUTH/.well-known/jwks.json"
    private const val RESOURCE = "https://api.openai.com/v1"
    private const val CATALOG = "$RESOURCE/models"
    private const val RESPONSES = "$RESOURCE/responses"
    private const val SPENT_ID_PREFIX = "chat_spent_v1_"
    private const val MAX_CHAT_ANSWER_BYTES = 20_000
    private const val PREFS = "mcp_chatgpt_oauth_v1"
    private const val KEY_ALIAS = "mcp_chatgpt_oauth_credentials_v1"
    private const val CALLBACK_PATH = "/auth/callback"
    private const val REQUESTED_SCOPES =
      "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
    private const val LOGIN_TIMEOUT_MS = 180_000L
    private const val HTTP_RESPONSE_MAX = 128 * 1024
  }

  private val prefs by lazy {
    activity.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
  }
  private val executor = Executors.newSingleThreadExecutor { command ->
    Thread(command, "ChatGptOAuth").apply { isDaemon = true }
  }
  private val ui = Handler(Looper.getMainLooper())
  private val random = SecureRandom()
  private val busy = AtomicBoolean(false)
  private val lock = Any()
  private var pending: Attempt? = null
  private var record: Record? = null
  private var loaded = false
  @Volatile private var linked = false
  @Volatile private var lastError: String? = null
  @Volatile private var nextRefreshAt: Long = 0L

  private fun chatReceipt(id: String, state: String, answer: String? = null,
                          model: String? = null, reason: String? = null): JSObject = JSObject().apply {
    put("requestId", id)
    put("state", state)
    if (answer != null) put("answer", answer)
    if (model != null) put("model", model)
    if (reason != null) put("reason", reason)
  }

  private class ChatFailure(val safeReason: String) : IllegalStateException()

  private fun httpChatReason(status: Int): String = when (status) {
    400 -> "http_400"
    401 -> "http_401"
    403 -> "http_403"
    404 -> "http_404"
    422 -> "http_422"
    429 -> "http_429"
    in 500..599 -> "http_5xx"
    else -> "http_other"
  }

  private fun providerChatReason(code: String?): String = when (code) {
    "subscription_sharing_usage_limit_exceeded" -> "subscription_sharing_usage_limit_exceeded"
    "usage_unavailable" -> "usage_unavailable"
    "model_not_found" -> "model_not_found"
    else -> "provider_failure"
  }

  private fun classifyChatFailure(error: Exception): String = when (error) {
    is ChatFailure -> error.safeReason
    is OAuthHttpException -> httpChatReason(error.status)
    is SocketTimeoutException -> "network_timeout"
    is java.net.UnknownHostException, is java.net.ConnectException,
    is java.net.SocketException, is javax.net.ssl.SSLException -> "network_error"
    else -> "unknown_failure"
  }

  /** Call under lock. Committed before the first possible outbound chat POST. */
  private fun spendChatRequest(id: String): Boolean = synchronized(lock) {
    val key = SPENT_ID_PREFIX + id
    if (prefs.contains(key)) return@synchronized false
    prefs.edit().putBoolean(key, true).commit()
  }

  private fun selectChatModel(token: String): String {
    val listing = requestJson(CATALOG, null, token)
    val entries = listing.optJSONArray("models")
      ?: throw IllegalStateException("OpenAI model catalog missing")
    require(entries.length() <= 10_000) { "OpenAI model catalog too large" }
    for (i in 0 until entries.length()) {
      val entry = entries.optJSONObject(i) ?: continue
      if (entry.optString("visibility") != "list") continue
      val model = entry.optString("slug")
      if (model.matches(Regex("^[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}$")) &&
          model.startsWith("gpt-")) return model
    }
    throw ChatFailure("model_unavailable")
  }

  private fun appendChatAnswer(answer: StringBuilder, piece: String) {
    require(answer.toString().toByteArray(StandardCharsets.UTF_8).size +
      piece.toByteArray(StandardCharsets.UTF_8).size <= MAX_CHAT_ANSWER_BYTES) {
      "Chat answer exceeded safe limits"
    }
    answer.append(piece)
  }

  private fun readSseLine(input: java.io.InputStream, max: Int = 64 * 1024): String? {
    val bytes = java.io.ByteArrayOutputStream()
    while (true) {
      val next = input.read()
      if (next == -1) return if (bytes.size() == 0) null else
        String(bytes.toByteArray(), StandardCharsets.UTF_8)
      if (next == 10) return String(bytes.toByteArray(), StandardCharsets.UTF_8)
      if (next != 13) {
        require(bytes.size() < max) { "Oversized chat stream event" }
        bytes.write(next)
      }
    }
  }

  private fun streamChat(token: String, model: String, payload: JSONObject): String {
    val uri = URL(RESPONSES)
    require(uri.protocol == "https" && uri.host == "api.openai.com" &&
      uri.port == -1 && uri.path == "/v1/responses")
    val connection = (uri.openConnection() as HttpURLConnection).apply {
      requestMethod = "POST"
      instanceFollowRedirects = false
      connectTimeout = 10_000
      readTimeout = 45_000
      doOutput = true
      setRequestProperty("Authorization", "Bearer $token")
      setRequestProperty("Content-Type", "application/json")
      setRequestProperty("Accept", "text/event-stream")
      setRequestProperty("Cache-Control", "no-store")
    }
    try {
      connection.outputStream.use {
        it.write(payload.toString().toByteArray(StandardCharsets.UTF_8))
      }
      val status = connection.responseCode
      if (status != 200) {
        // Read only a bounded error envelope, and expose only fixed codes.
        val errorJson = try {
          val bytes = connection.errorStream?.use { readBounded(it, 16 * 1024) }
          JSONObject(String(bytes ?: ByteArray(0), StandardCharsets.UTF_8))
        } catch (_: Exception) { JSONObject() }
        val providerCode = errorJson.optJSONObject("error")?.optString("code")
        val known = providerChatReason(providerCode)
        throw ChatFailure(if (known != "provider_failure") known else httpChatReason(status))
      }
      val mediaType = connection.contentType?.substringBefore(';')?.trim()?.lowercase()
      // Some Android HTTP stacks/providers omit Content-Type on a streamed
      // response. A missing header is not proof of failure: read the body but
      // accept it ONLY if the strict SSE parser below observes a completed
      // Responses event with a nonempty assistant answer. Explicit JSON/HTML
      // media types remain unsupported and fail closed.
      if (mediaType != null && mediaType.isNotEmpty() && mediaType != "text/event-stream") {
        // HTTP 200 is not enough to claim a model response. Only the documented
        // SSE protocol can supply a trustworthy response.completed proof.
        // Classify the MIME category, never return raw provider body/headers.
        val kind = when (mediaType) {
          "application/json", "application/problem+json" -> "invalid_stream_json"
          "text/html" -> "invalid_stream_html"
          null, "" -> "invalid_stream_missing"
          else -> "invalid_stream_other"
        }
        throw ChatFailure(kind)
      }
      val answer = StringBuilder()
      var complete = false
      var terminal = false
      var totalBytes = 0
      var eventType = ""
      val data = StringBuilder()
      connection.inputStream.use { input ->
        while (!terminal) {
          val line = readSseLine(input) ?: break
          totalBytes += line.toByteArray(StandardCharsets.UTF_8).size + 1
          require(totalBytes <= 2 * 1024 * 1024) { "Chat stream exceeded safe limits" }
          if (line.isEmpty()) {
            if (data.isNotEmpty()) {
              val event = JSONObject(data.toString())
              val kind = event.optString("type", eventType)
              when (kind) {
                "response.output_text.delta" -> {
                  val delta = event.getString("delta")
                  appendChatAnswer(answer, delta)
                }
                "response.completed" -> {
                  val response = event.optJSONObject("response")
                  if (response?.optString("status") != "completed")
                    throw ChatFailure("invalid_completion")
                  if (answer.isEmpty()) {
                    val output = response.optJSONArray("output") ?: JSONArray()
                    for (i in 0 until output.length()) {
                      val pieces = output.optJSONObject(i)?.optJSONArray("content") ?: continue
                      for (j in 0 until pieces.length()) {
                        val part = pieces.optJSONObject(j) ?: continue
                        if (part.optString("type") == "output_text") {
                          val piece = part.optString("text")
                          appendChatAnswer(answer, piece)
                        }
                      }
                    }
                  }
                  complete = true
                  terminal = true
                }
                "response.failed" -> throw ChatFailure(providerChatReason(
                  event.optJSONObject("response")?.optJSONObject("error")?.optString("code")
                    ?: event.optJSONObject("error")?.optString("code")))
                "response.incomplete" -> throw ChatFailure("response_incomplete")
                "response.cancelled" -> throw ChatFailure("response_cancelled")
                "error" -> throw ChatFailure(providerChatReason(
                  event.optJSONObject("error")?.optString("code")))
              }
            }
            eventType = ""
            data.setLength(0)
          } else if (line.startsWith("event:")) {
            eventType = line.substringAfter(':').trim()
          } else if (line.startsWith("data:")) {
            if (data.isNotEmpty()) data.append('\n')
            data.append(line.substringAfter(':').trimStart())
          }
        }
      }
      if (!complete) throw ChatFailure("stream_incomplete")
      if (answer.isEmpty()) throw ChatFailure("empty_answer")
      return answer.toString()
    } finally { connection.disconnect() }
  }

  @Command
  fun chat(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptChatArgs::class.java) }
      catch (_: Exception) {
        invoke.resolve(chatReceipt("", "failed_before_dispatch", reason = "invalid_request")); return
      }
    val id = args.requestId
    val validated = try {
      require(id == UUID.fromString(id).toString())
      require(args.userInitiated)
      val promptBytes = args.prompt.toByteArray(StandardCharsets.UTF_8)
      require(promptBytes.isNotEmpty() && promptBytes.size <= 16_384)
      require(args.context.size <= 16)
      var contextBytes = 0
      val input = JSONArray()
      args.context.forEach { turn ->
        require(turn.role in setOf("user", "assistant"))
        val bytes = turn.content.toByteArray(StandardCharsets.UTF_8)
        require(bytes.isNotEmpty())
        contextBytes += bytes.size
        require(contextBytes <= 65_536)
        input.put(JSONObject().put("role", turn.role).put("content", turn.content))
      }
      input.put(JSONObject().put("role", "user").put("content", args.prompt))
      input
    } catch (_: Exception) {
      invoke.resolve(chatReceipt(id, "failed_before_dispatch", reason = "invalid_request")); return
    }
    if (pending != null || !busy.compareAndSet(false, true)) {
      invoke.resolve(chatReceipt(id, "failed_before_dispatch", reason = "busy")); return
    }
    executor.execute {
      var dispatched = false
      var outcome = chatReceipt(id, "failed_before_dispatch")
      try {
        val current = loadRecord()
        require(current != null && current.linked && current.canUsePlan()) {
          "ChatGPT account is not linked"
        }
        val valid = refreshIfNeeded(current)
        require(valid.linked && valid.canUsePlan())
        val model = selectChatModel(valid.accessToken)
        val body = JSONObject()
          .put("model", model)
          .put("input", validated)
          .put("store", false)
          .put("stream", true)
        require(body.toString().toByteArray(StandardCharsets.UTF_8).size <= 90_000)
        require(spendChatRequest(id)) { "Duplicate chat request ID" }
        // Once persisted, any failure is uncertain, including an interrupted POST.
        dispatched = true
        val answer = streamChat(valid.accessToken, model, body)
        outcome = chatReceipt(id, "completed", answer, model)
      } catch (error: Exception) {
        outcome = chatReceipt(id, if (dispatched) "uncertain" else "failed_before_dispatch",
          reason = classifyChatFailure(error))
      } finally {
        busy.set(false)
        val finalReceipt = outcome
        ui.post { invoke.resolve(finalReceipt) }
      }
    }
  }

  private data class Attempt(
    val server: ServerSocket,
    val state: String,
    val nonce: String,
    val verifier: String,
    val clientId: String?,
    val redirectUri: String,
    val started: Long
  )

  private data class Record(
    val clientId: String,
    val subject: String,
    val idToken: String,
    val accessToken: String,
    val refreshToken: String,
    val scopes: String,
    val expiresAt: Long,
    val linked: Boolean
  ) {
    fun canUsePlan() = scopes.split(' ').contains("chatgpt.tokens.use.direct") &&
      scopes.split(' ').contains("resource.invoke")
    fun toJson() = JSONObject()
      .put("clientId", clientId).put("subject", subject)
      .put("idToken", idToken).put("accessToken", accessToken)
      .put("refreshToken", refreshToken).put("scopes", scopes)
      .put("expiresAt", expiresAt).put("linked", linked)
    companion object {
      fun fromJson(json: JSONObject) = Record(
        json.getString("clientId"), json.getString("subject"),
        json.getString("idToken"), json.getString("accessToken"),
        json.optString("refreshToken"), json.getString("scopes"),
        json.getLong("expiresAt"), json.optBoolean("linked", false)
      )
    }
  }

  private fun randomToken(bytes: Int): String {
    val buffer = ByteArray(bytes)
    random.nextBytes(buffer)
    return Base64.encodeToString(buffer, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
  }

  private fun cryptoKey(): SecretKey {
    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
    val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    generator.init(KeyGenParameterSpec.Builder(KEY_ALIAS,
      KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
      .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
      .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
      .setKeySize(256).build())
    return generator.generateKey()
  }

  private fun storeRecord(value: Record?) {
    synchronized(lock) {
      if (value == null) {
        prefs.edit().remove("credentials").apply()
        record = null
        linked = false
        return
      }
      val cipher = Cipher.getInstance("AES/GCM/NoPadding")
      cipher.init(Cipher.ENCRYPT_MODE, cryptoKey())
      val plain = value.toJson().toString().toByteArray(StandardCharsets.UTF_8)
      val ciphertext = cipher.doFinal(plain)
      val encoded = Base64.encodeToString(cipher.iv + ciphertext, Base64.NO_WRAP)
      if (!prefs.edit().putString("credentials", encoded).commit()) {
        throw IllegalStateException("Could not store authorized account")
      }
      record = value
      linked = value.linked
      loaded = true
    }
  }

  private fun loadRecord(): Record? = synchronized(lock) {
    if (loaded) return record
    loaded = true
    val saved = prefs.getString("credentials", null) ?: return null
    try {
      val data = Base64.decode(saved, Base64.NO_WRAP)
      require(data.size in 29..64_000)
      val cipher = Cipher.getInstance("AES/GCM/NoPadding")
      cipher.init(Cipher.DECRYPT_MODE, cryptoKey(), GCMParameterSpec(128, data.copyOfRange(0, 12)))
      val json = JSONObject(String(cipher.doFinal(data.copyOfRange(12, data.size)),
        StandardCharsets.UTF_8))
      record = Record.fromJson(json)
      linked = record?.linked == true
    } catch (_: Exception) {
      record = null
      linked = false
      lastError = "Stored ChatGPT authorization could not be restored. Please sign in again."
    }
    record
  }

  private fun hostId(): String {
    val saved = prefs.getString("host_id", null)
    if (saved?.matches(Regex("^urn:uuid:[0-9a-fA-F-]{36}$")) == true) return saved
    val created = "urn:uuid:" + UUID.randomUUID().toString()
    if (!prefs.edit().putString("host_id", created).commit())
      throw IllegalStateException("Could not initialize the device's OAuth host identity")
    return created
  }

  private fun snapshot(): JSObject {
    val current = loadRecord()
    val waiting = synchronized(lock) { pending != null }
    val active = current != null && current.expiresAt > System.currentTimeMillis()
    val state = when {
      waiting -> "authorizing"
      active && linked -> "linked"
      active -> "verified"
      busy.get() -> "authorizing"
      lastError != null -> "error"
      else -> "signed_out"
    }
    val verified = active && !waiting
    return JSObject().apply {
      put("supported", true)
      put("state", state)
      put("pending", waiting || (busy.get() && !active))
      put("signInVerified", verified)
      put("sessionLinked", verified && linked)
      put("planEnabled", verified && current?.canUsePlan() == true)
      put("message", when {
        waiting -> "Waiting for OpenAI account authorization in Chrome."
        busy.get() -> "Checking the authorized ChatGPT account."
        verified && linked -> "ChatGPT account and plan permission verified for Mcp Agent Ai."
        verified && current?.canUsePlan() == true ->
          "OpenAI verified your account. Bring Session can check the permitted plan connection."
        verified -> "Your ChatGPT identity was verified, but permission to use the plan was not granted."
        else -> lastError ?: "Connect with ChatGPT to authorize this app. Chrome login alone is not app authorization."
      })
    }
  }

  private fun onUi(invoke: Invoke, callback: () -> Unit) {
    activity.runOnUiThread {
      try { callback() }
      catch (_: Exception) { invoke.reject("ChatGPT account operation failed") }
    }
  }

  @Command
  fun status(invoke: Invoke) = onUi(invoke) {
    loadRecord()
    maybeRefresh()
    invoke.resolve(snapshot())
  }

  @Command
  fun start(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptOAuthArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid ChatGPT sign-in request"); return }
    if (!args.userInitiated) { invoke.reject("ChatGPT sign-in requires a user action"); return }
    onUi(invoke) {
      if (busy.get()) {
        invoke.reject("ChatGPT account verification is already in progress"); return@onUi
      }
      // An explicit retry may cancel an abandoned browser authorization.
      synchronized(lock) { pending?.server?.close(); pending = null }
      val previous = loadRecord()
      // Use an exact IPv4 loopback listener, never 0.0.0.0. No LAN access.
      val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
      server.soTimeout = 2500
      val verifier = randomToken(64)
      val state = randomToken(32)
      val nonce = randomToken(32)
      val redirect = "http://127.0.0.1:" + server.localPort + CALLBACK_PATH
      val attempt = Attempt(server, state, nonce, verifier, previous?.clientId,
        redirect, System.currentTimeMillis())
      // Resolve host identity before claiming a live attempt. If persistence
      // fails, close the listener rather than stranding a Pending UI state.
      val deviceHost = try { hostId() }
        catch (error: Exception) { server.close(); throw error }
      synchronized(lock) { pending = attempt }
      lastError = null
      val challenge = Base64.encodeToString(
        MessageDigest.getInstance("SHA-256").digest(verifier.toByteArray(StandardCharsets.US_ASCII)),
        Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
      val builder = Uri.parse(AUTHORIZE).buildUpon()
        .appendQueryParameter("client_id", previous?.clientId ?: "dynamic_agent_client")
        .appendQueryParameter("ext_agent_host_id", deviceHost)
        .appendQueryParameter("response_type", "code")
        .appendQueryParameter("redirect_uri", redirect)
        .appendQueryParameter("scope", REQUESTED_SCOPES)
        .appendQueryParameter("resource", RESOURCE)
        .appendQueryParameter("state", state)
        .appendQueryParameter("nonce", nonce)
        .appendQueryParameter("code_challenge_method", "S256")
        .appendQueryParameter("code_challenge", challenge)
      if (previous == null) builder.appendQueryParameter("agent_name_hint", "Mcp Agent Ai")
      else builder.appendQueryParameter("id_token_hint", previous.idToken)
      val url = builder.build()
      executor.execute { acceptCallback(attempt) }
      try {
        CustomTabsIntent.Builder().setShowTitle(true)
          .setShareState(CustomTabsIntent.SHARE_STATE_OFF).build()
          .launchUrl(activity, url)
        invoke.resolve(snapshot())
      } catch (_: Exception) {
        synchronized(lock) { if (pending === attempt) pending = null }
        server.close()
        lastError = "Could not launch Chrome for ChatGPT authorization."
        invoke.reject(lastError!!)
      }
    }
  }

  @Command
  fun bring(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptOAuthArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid ChatGPT connection request"); return }
    if (!args.userInitiated) { invoke.reject("Bring Session requires a user action"); return }
    onUi(invoke) {
      val current = loadRecord()
      if (pending != null || current == null || !current.canUsePlan() || busy.get()) {
        invoke.reject("Authorize your ChatGPT plan for Mcp Agent Ai before bringing it"); return@onUi
      }
      if (!busy.compareAndSet(false, true)) {
        invoke.reject("ChatGPT authorization is already being checked"); return@onUi
      }
      executor.execute {
        try {
          val valid = refreshIfNeeded(current)
          require(valid.canUsePlan()) { "ChatGPT plan permission was not granted" }
          // An authenticated, read-only API request proves that the OAuth
          // credential is usable by THIS app; no website session is imported.
          val response = requestJson(CATALOG, null, valid.accessToken)
          require(response.optJSONArray("models") != null) {
            "OpenAI did not confirm access to the model catalog"
          }
          storeRecord(valid.copy(linked = true))
          lastError = null
          ui.post { invoke.resolve(snapshot()) }
        } catch (error: Exception) {
          // Admission checks can fail due to serving-region or policy rules.
          // Neither 401 nor 403 alone proves a refresh token was revoked.
          lastError = "OpenAI could not verify ChatGPT plan access. Check the grant and retry."
          ui.post { invoke.reject(lastError!!) }
        } finally { busy.set(false) }
      }
    }
  }

  @Command
  fun signout(invoke: Invoke) {
    val args = try { invoke.parseArgs(ChatGptOAuthArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid sign-out request"); return }
    if (!args.userInitiated) { invoke.reject("Sign-out requires a user action"); return }
    onUi(invoke) {
      synchronized(lock) { pending?.server?.close(); pending = null }
      if (!busy.compareAndSet(false, true)) {
        invoke.reject("ChatGPT authorization is busy; retry disconnect"); return@onUi
      }
      val previous = loadRecord()
      executor.execute {
        var revoked = true
        try {
          if (previous?.refreshToken?.isNotBlank() == true) {
            val discovery = https(AUTH + "/.well-known/openid-configuration", "GET", null, null)
            val revocation = discovery.getString("revocation_endpoint")
            val uri = Uri.parse(revocation)
            require(uri.scheme == "https" && uri.host == "auth.openai.com" &&
              uri.port == -1 && uri.userInfo == null)
            requestForm(revocation, mapOf(
              "token" to previous.refreshToken,
              "token_type_hint" to "refresh_token",
              "client_id" to previous.clientId
            ))
          }
        } catch (_: Exception) { revoked = false }
        finally {
          try { storeRecord(null) } catch (_: Exception) { /* Empty local record still wins. */ }
          busy.set(false)
          lastError = if (revoked) null else
            "Signed out locally. OpenAI could not confirm remote token revocation."
          ui.post { invoke.resolve(snapshot()) }
        }
      }
    }
  }

  private fun maybeRefresh() {
    val current = loadRecord() ?: return
    if (current.expiresAt > System.currentTimeMillis() + 120_000L) return
    if (System.currentTimeMillis() < nextRefreshAt) return
    if (pending != null || !busy.compareAndSet(false, true)) return
    executor.execute {
      try {
        refreshIfNeeded(current)
        nextRefreshAt = 0L
        lastError = null
      } catch (_: Exception) {
        // An offline or temporary server failure isn't a confirmed logout.
        // A terminal OAuth invalid_grant is handled inside refreshIfNeeded.
        nextRefreshAt = System.currentTimeMillis() + 30_000L
        if (lastError?.contains("revoked", ignoreCase = true) != true)
          lastError = "Could not renew ChatGPT access. Retry when online."
      } finally { busy.set(false) }
    }
  }

  private fun refreshIfNeeded(previous: Record): Record {
    if (previous.expiresAt > System.currentTimeMillis() + 120_000L) return previous
    require(previous.refreshToken.isNotBlank()) { "ChatGPT session must be reauthorized" }
    val response = try {
      requestForm(TOKEN, mapOf(
        "grant_type" to "refresh_token",
        "client_id" to previous.clientId,
        "refresh_token" to previous.refreshToken,
        "resource" to RESOURCE
      ))
    } catch (error: OAuthHttpException) {
      if (error.status == 400 && error.invalidGrant) {
        // The provider has conclusively rejected this renewable session.
        storeRecord(null)
        lastError = "ChatGPT authorization expired or was revoked. Sign in again."
      }
      throw error
    }
    val updated = Record(
      previous.clientId, previous.subject, previous.idToken,
      response.getString("access_token"),
      response.getString("refresh_token"),
      response.optString("scope", previous.scopes),
      System.currentTimeMillis() + response.getLong("expires_in").coerceIn(1L, 3600L) * 1000L,
      previous.linked
    )
    // New ID tokens, when returned on refresh, must match the same subject.
    val replacement = response.optString("id_token")
    val verified = if (replacement.isNotBlank()) {
      // Refresh responses do not necessarily have a new nonce. Check the same
      // signed issuer/audience/subject/expiry, without requiring a new nonce.
      val claims = verifyIdToken(replacement, previous.clientId, null, updated.accessToken)
      require(claims.getString("sub") == previous.subject) {
        "ChatGPT account identity changed during refresh"
      }
      updated.copy(idToken = replacement)
    } else updated
    storeRecord(verified)
    return verified
  }

  private class OAuthHttpException(val status: Int, val invalidGrant: Boolean) :
    IllegalStateException("OpenAI authorization was rejected")

  private fun readBounded(stream: java.io.InputStream, max: Int): ByteArray {
    val output = java.io.ByteArrayOutputStream()
    val buffer = ByteArray(4096)
    while (true) {
      val read = stream.read(buffer)
      if (read < 0) break
      require(output.size() + read <= max) { "Authorization response exceeded safe limits" }
      output.write(buffer, 0, read)
    }
    return output.toByteArray()
  }

  private fun https(url: String, method: String, body: String?, bearer: String?): JSONObject {
    val parsed = URL(url)
    require(parsed.protocol == "https" && parsed.port == -1 &&
      parsed.host in setOf("auth.openai.com", "api.openai.com"))
    val connection = (parsed.openConnection() as java.net.HttpURLConnection).apply {
      requestMethod = method
      instanceFollowRedirects = false
      connectTimeout = 10_000
      readTimeout = 12_000
      setRequestProperty("Accept", "application/json")
      setRequestProperty("Cache-Control", "no-store")
      if (bearer != null) setRequestProperty("Authorization", "Bearer " + bearer)
      if (body != null) {
        doOutput = true
        setRequestProperty("Content-Type", "application/x-www-form-urlencoded")
      }
    }
    try {
      if (body != null) connection.outputStream.use {
        it.write(body.toByteArray(StandardCharsets.UTF_8))
      }
      val status = connection.responseCode
      val limit = if (parsed.host == "api.openai.com" && parsed.path == "/v1/models")
        1024 * 1024 else HTTP_RESPONSE_MAX
      val bytes = (if (status in 200..299) connection.inputStream
                   else connection.errorStream)?.use { readBounded(it, limit) }
        ?: ByteArray(0)
      val response = try { JSONObject(String(bytes, StandardCharsets.UTF_8)) }
        catch (_: Exception) { JSONObject() }
      if (status !in 200..299) {
        val code = response.optString("error")
        throw OAuthHttpException(status, code == "invalid_grant" ||
          response.optJSONObject("error")?.optString("code") == "invalid_grant")
      }
      return response
    } finally { connection.disconnect() }
  }

  private fun requestJson(url: String, body: String?, bearer: String?): JSONObject =
    https(url, if (body == null) "GET" else "POST", body, bearer)

  private fun requestForm(url: String, fields: Map<String, String>): JSONObject {
    val content = fields.entries.joinToString("&") {
      URLEncoder.encode(it.key, "UTF-8") + "=" + URLEncoder.encode(it.value, "UTF-8")
    }
    return https(url, "POST", content, null)
  }

  private fun decodeJwtPart(segment: String): ByteArray {
    require(segment.length in 4..24_000 && segment.matches(Regex("^[a-zA-Z0-9_-]+$")))
    return Base64.decode(segment, Base64.URL_SAFE or Base64.NO_WRAP)
  }

  private fun verifyIdToken(token: String, audience: String, nonce: String?,
                            accessToken: String): JSONObject {
    require(token.length in 100..30_000)
    val chunks = token.split('.')
    require(chunks.size == 3)
    val header = JSONObject(String(decodeJwtPart(chunks[0]), StandardCharsets.UTF_8))
    require(header.optString("alg") == "RS256" && header.optString("typ") in
      setOf("", "JWT", "jwt"))
    val kid = header.getString("kid")
    require(kid.length in 1..256)
    val keys = https(JWKS, "GET", null, null).getJSONArray("keys")
    var matched = false
    for (index in 0 until keys.length().coerceAtMost(40)) {
      val key = keys.getJSONObject(index)
      if (key.optString("kid") != kid || key.optString("kty") != "RSA") continue
      if (key.has("use") && key.optString("use") != "sig") continue
      if (key.has("alg") && key.optString("alg") != "RS256") continue
      val modulus = BigInteger(1, decodeJwtPart(key.getString("n")))
      val exponent = BigInteger(1, decodeJwtPart(key.getString("e")))
      val publicKey = KeyFactory.getInstance("RSA")
        .generatePublic(RSAPublicKeySpec(modulus, exponent))
      val signature = Signature.getInstance("SHA256withRSA")
      signature.initVerify(publicKey)
      signature.update((chunks[0] + "." + chunks[1]).toByteArray(StandardCharsets.US_ASCII))
      require(signature.verify(decodeJwtPart(chunks[2]))) {
        "Invalid signed OpenAI account identity"
      }
      matched = true
      break
    }
    require(matched) { "Unknown OpenAI signing key" }
    val claims = JSONObject(String(decodeJwtPart(chunks[1]), StandardCharsets.UTF_8))
    val now = System.currentTimeMillis() / 1000L
    require(claims.optString("iss") == AUTH && claims.optString("sub").isNotBlank() &&
      claims.getLong("exp") > now - 5 &&
      claims.getLong("iat") < now + 60 &&
      (!claims.has("nbf") || claims.getLong("nbf") <= now + 60))
    val audienceClaim = claims.get("aud")
    val matchesAudience = when (audienceClaim) {
      is String -> audienceClaim == audience
      is JSONArray -> (0 until audienceClaim.length()).any {
        audienceClaim.optString(it) == audience
      }
      else -> false
    }
    require(matchesAudience) { "ChatGPT token was issued for a different application" }
    if (nonce != null) require(claims.optString("nonce") == nonce) {
      "ChatGPT OAuth nonce mismatch"
    }
    val atHash = claims.optString("at_hash")
    if (atHash.isNotBlank()) {
      val digest = MessageDigest.getInstance("SHA-256")
        .digest(accessToken.toByteArray(StandardCharsets.US_ASCII))
      val expected = Base64.encodeToString(digest.copyOfRange(0, 16),
        Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
      require(MessageDigest.isEqual(expected.toByteArray(StandardCharsets.US_ASCII),
        atHash.toByteArray(StandardCharsets.US_ASCII))) {
        "ChatGPT access token does not match the signed identity"
      }
    }
    return claims
  }

  private fun readHttpLine(input: java.io.InputStream, limit: Int): String {
    val bytes = java.io.ByteArrayOutputStream()
    while (bytes.size() <= limit) {
      val value = input.read()
      if (value == -1) break
      if (value == 10) break
      if (value != 13) bytes.write(value)
    }
    require(bytes.size() <= limit) { "Overlong loopback HTTP header" }
    return bytes.toString("US-ASCII")
  }

  private fun returnPage(message: String): String {
    // No authorization codes, state, PKCE or credentials enter this page.
    return "<!doctype html><html><head><meta charset='utf-8'>" +
      "<meta name='referrer' content='no-referrer'>" +
      "<meta name='viewport' content='width=device-width, initial-scale=1'>" +
      "<script>setTimeout(function(){location.href='mcpagentai://auth-complete'},600)</script>" +
      "</head>" +
      "<body style='font-family:sans-serif;padding:2rem;background:#10151f;color:white'>" +
      "<h2>Mcp Agent Ai</h2><p>" + message + "</p>" +
      "<p><a style='color:#a4bcff' href='mcpagentai://auth-complete'>" +
      "Return to Mcp Agent Ai</a></p></body></html>"
  }

  private fun sendPage(socket: Socket, code: Int, message: String) {
    val body = returnPage(message).toByteArray(StandardCharsets.UTF_8)
    val header = "HTTP/1.1 " + code + (if (code == 200) " OK" else " Bad Request") +
      "\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\n" +
      "Referrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n" +
      "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; " +
      "base-uri 'none'; form-action 'none'\r\n" +
      "Content-Length: " + body.size + "\r\nConnection: close\r\n\r\n"
    socket.getOutputStream().apply {
      write(header.toByteArray(StandardCharsets.US_ASCII))
      write(body)
      flush()
    }
  }

  private fun acceptCallback(attempt: Attempt) {
    var accepted = false
    try {
      while (!accepted && System.currentTimeMillis() - attempt.started < LOGIN_TIMEOUT_MS &&
          synchronized(lock) { pending === attempt }) {
        try {
          attempt.server.accept().use { client ->
            client.soTimeout = 4000
            if (!client.inetAddress.isLoopbackAddress) return@use
            val input = client.getInputStream()
            val request = readHttpLine(input, 8192).split(" ")
            var host = ""
            for (header in 0 until 40) {
              val line = readHttpLine(input, 4096)
              if (line.isBlank()) break
              if (line.startsWith("Host:", ignoreCase = true))
                host = line.substringAfter(':').trim()
            }
            if (request.size != 3 || request[0] != "GET" ||
                host != "127.0.0.1:" + attempt.server.localPort ||
                request[1].length > 6000) {
              sendPage(client, 400, "Invalid local callback.")
              return@use
            }
            val uri = Uri.parse("http://127.0.0.1:" + attempt.server.localPort + request[1])
            if (uri.path != CALLBACK_PATH || uri.fragment != null) {
              sendPage(client, 400, "Invalid local callback destination.")
              return@use
            }
            val incoming = uri.getQueryParameter("state") ?: ""
            if (!MessageDigest.isEqual(incoming.toByteArray(StandardCharsets.US_ASCII),
                  attempt.state.toByteArray(StandardCharsets.US_ASCII))) {
              sendPage(client, 400, "Authorization attempt did not match.")
              return@use
            }
            accepted = true // A valid state consumes the attempt exactly once.
            if (uri.getQueryParameter("error") != null) {
              lastError = "ChatGPT account authorization was cancelled."
              sendPage(client, 200, "Authorization was cancelled.")
              return@use
            }
            val code = uri.getQueryParameter("code") ?: ""
            val issued = uri.getQueryParameter("client_id")
            val clientId = if (attempt.clientId == null) issued else attempt.clientId
            require(clientId != null &&
              clientId.matches(Regex("^oaiapp_[A-Za-z0-9_-]{4,200}$")) &&
              (attempt.clientId == null || issued == null || issued == attempt.clientId) &&
              code.length in 10..4096) { "Invalid OpenAI authorization callback" }
            busy.set(true)
            val response = requestForm(TOKEN, mapOf(
              "grant_type" to "authorization_code",
              "client_id" to clientId,
              "code" to code,
              "code_verifier" to attempt.verifier,
              "redirect_uri" to attempt.redirectUri,
              "resource" to RESOURCE
            ))
            val access = response.getString("access_token")
            val idToken = response.getString("id_token")
            require(response.optString("token_type").equals("Bearer", true) &&
              access.length in 20..16_000 &&
              response.getLong("expires_in") > 0)
            val claims = verifyIdToken(idToken, clientId, attempt.nonce, access)
            val old = loadRecord()
            if (attempt.clientId != null) {
              require(old != null && old.clientId == clientId &&
                old.subject == claims.getString("sub")) {
                "A different ChatGPT account cannot replace this saved connection"
              }
            }
            require(synchronized(lock) { pending === attempt }) {
              "The authorization attempt was cancelled"
            }
            val result = Record(clientId, claims.getString("sub"), idToken, access,
              response.optString("refresh_token"), response.optString("scope"),
              System.currentTimeMillis() +
                response.getLong("expires_in").coerceIn(1L, 3600L) * 1000L, false)
            storeRecord(result)
            lastError = null
            sendPage(client, 200, if (result.canUsePlan())
              "ChatGPT account verified. Return to the app and tap Bring Session."
              else "Your identity was verified, but plan access was not approved.")
          }
        } catch (_: java.net.SocketTimeoutException) {
          // Bound the listener lifetime while waiting for the user to consent.
        } catch (_: Exception) {
          if (accepted) lastError = "ChatGPT authorization could not be verified. Try signing in again."
        }
      }
      if (!accepted && synchronized(lock) { pending === attempt })
        lastError = "ChatGPT sign-in timed out. Please try again."
    } finally {
      attempt.server.close()
      synchronized(lock) { if (pending === attempt) pending = null }
      busy.set(false)
    }
  }
}
