package ai.mcpagent.preview;

import android.content.Context;
import android.content.SharedPreferences;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.time.Instant;
import java.util.HashMap;
import java.util.Map;
import java.util.UUID;

import javax.net.ssl.HttpsURLConnection;

/** Android provider runtime. Keys live only in memory and HTTPS endpoints are fixed. */
final class MobileApi {
    private static final int MAX_RESPONSE_BYTES = 1024 * 1024;
    private static final String STORE = "mcp_agent_ai";
    private final SharedPreferences prefs;
    private final Map<String, String> credentials = new HashMap<>();

    MobileApi(Context context) {
        prefs = context.getSharedPreferences(STORE, Context.MODE_PRIVATE);
    }

    synchronized String request(String raw) {
        try {
            if (raw == null || raw.length() > 52_000) return error("Request too large");
            JSONObject input = new JSONObject(raw);
            String route = input.optString("route", "");
            JSONObject body = input.optJSONObject("body");
            if ("/api/state".equals(route)) return state().toString();
            if ("/api/key".equals(route)) return key(body).toString();
            if ("/api/chat".equals(route)) return chat(body).toString();
            return error("Unsupported operation");
        } catch (ProviderError failure) {
            return error(failure.getMessage());
        } catch (JSONException failure) {
            return error("Invalid request");
        } catch (Exception failure) {
            // Never return provider bodies, raw network failures, or API credentials.
            return error("Request failed. Check the connection, model ID and provider settings.");
        }
    }

    private static boolean provider(String name) {
        return "openai".equals(name) || "anthropic".equals(name) || "openrouter".equals(name);
    }

    private JSONObject state() throws JSONException {
        JSONArray keys = new JSONArray();
        for (String key : credentials.keySet()) keys.put(key);
        return new JSONObject()
            .put("version", "0.2.2-preview")
            .put("platform", "android")
            .put("modules", new JSONObject())
            .put("availableModules", new JSONArray())
            .put("skillsAvailable", false)
            .put("skills", new JSONArray())
            .put("sessions", sessions())
            .put("providers", new JSONArray().put("openai").put("anthropic").put("openrouter"))
            .put("workspace", "Android app-private storage")
            .put("cloud", "no background jobs")
            .put("hasKeys", keys);
    }

    private JSONObject key(JSONObject input) throws JSONException, ProviderError {
        if (input == null) throw new ProviderError("Invalid key request");
        String provider = input.optString("provider");
        if (!provider(provider)) throw new ProviderError("Unsupported provider");
        String supplied = input.optString("key", "");
        if (supplied.length() > 4096 || supplied.indexOf('\n') >= 0 || supplied.indexOf('\r') >= 0) {
            throw new ProviderError("Invalid API key");
        }
        if (supplied.trim().isEmpty()) credentials.remove(provider);
        else credentials.put(provider, supplied.trim());
        return new JSONObject().put("savedInMemory", credentials.containsKey(provider));
    }

    private JSONArray sessions() {
        try { return new JSONArray(prefs.getString("sessions_v2", "[]")); }
        catch (JSONException failure) { return new JSONArray(); }
    }

    private JSONObject chat(JSONObject input) throws Exception {
        if (input == null) throw new ProviderError("Invalid chat request");
        String provider = input.optString("provider", "");
        if (!provider(provider)) throw new ProviderError("Unsupported provider");
        String key = credentials.get(provider);
        if (key == null) throw new ProviderError("Connect an API key for " + provider + " first");
        String model = input.optString("model", "");
        if (!model.matches("[A-Za-z0-9][A-Za-z0-9._:/-]{0,119}")) throw new ProviderError("Enter a valid model ID");
        String prompt = input.optString("prompt", "").trim();
        if (prompt.isEmpty() || prompt.length() > 20_000) throw new ProviderError("Message must be 1–20,000 characters");
        String requestedSession = input.isNull("sessionId") ? null : input.optString("sessionId", null);
        if (requestedSession != null) {
            try { UUID.fromString(requestedSession); }
            catch (IllegalArgumentException failure) { throw new ProviderError("Invalid conversation ID"); }
        }
        JSONArray existing = sessions();
        JSONObject previous = null;
        JSONArray remaining = new JSONArray();
        for (int i = 0; i < existing.length(); i++) {
            JSONObject s = existing.optJSONObject(i);
            if (s == null) continue;
            if (requestedSession != null && requestedSession.equals(s.optString("id"))) previous = s;
            else remaining.put(s);
        }
        if (requestedSession != null && previous == null) throw new ProviderError("Conversation not found");
        if (previous != null && (!provider.equals(previous.optString("provider")) ||
                !model.equals(previous.optString("model")))) {
            throw new ProviderError("Start a new chat to change provider or model");
        }

        JSONArray context = new JSONArray();
        JSONArray history = previous == null ? new JSONArray() : previous.optJSONArray("messages");
        if (history == null) history = new JSONArray();
        for (int i = Math.max(0, history.length() - 12); i < history.length(); i++) {
            JSONObject message = history.optJSONObject(i);
            if (message == null) continue;
            String role = message.optString("role", "");
            if (!"user".equals(role) && !"assistant".equals(role)) continue;
            String value = message.optString("text", "");
            if (value.length() > 20_000) continue;
            context.put(new JSONObject().put("role", role).put("content", value));
        }
        context.put(new JSONObject().put("role", "user").put("content", prompt));
        String answer = requestProvider(provider, key, model, context);
        if (answer.trim().isEmpty() || answer.length() > 20_000) throw new ProviderError("Provider returned no usable text");

        JSONArray messages = new JSONArray();
        for (int i = Math.max(0, history.length() - 38); i < history.length(); i++) messages.put(history.get(i));
        messages.put(new JSONObject().put("role", "user").put("text", prompt));
        messages.put(new JSONObject().put("role", "assistant").put("text", answer));
        String id = requestedSession == null ? UUID.randomUUID().toString() : requestedSession;
        JSONObject session = new JSONObject().put("id", id).put("at", Instant.now().toString())
            .put("provider", provider).put("model", model).put("messages", messages)
            .put("actions", new JSONArray());
        JSONArray merged = new JSONArray().put(session);
        for (int i = 0; i < remaining.length() && i < 24; i++) merged.put(remaining.get(i));
        if (!prefs.edit().putString("sessions_v2", merged.toString()).commit()) {
            throw new ProviderError("Could not save conversation");
        }
        return new JSONObject().put("sessionId", id).put("answer", answer).put("actions", new JSONArray());
    }

    private static String requestProvider(String provider, String key, String model, JSONArray messages) throws Exception {
        final String endpoint;
        JSONObject request;
        if ("anthropic".equals(provider)) {
            endpoint = "https://api.anthropic.com/v1/messages";
            request = new JSONObject().put("model", model).put("max_tokens", 2048).put("messages", messages);
        } else {
            endpoint = "openrouter".equals(provider)
                ? "https://openrouter.ai/api/v1/chat/completions"
                : "https://api.openai.com/v1/chat/completions";
            request = new JSONObject().put("model", model).put("messages", messages);
        }
        byte[] body = request.toString().getBytes(StandardCharsets.UTF_8);
        if (body.length > 160_000) throw new ProviderError("Conversation exceeds request limit");
        HttpsURLConnection connection = (HttpsURLConnection) new URL(endpoint).openConnection();
        try {
            connection.setInstanceFollowRedirects(false);
            connection.setConnectTimeout(10_000);
            connection.setReadTimeout(45_000);
            connection.setRequestMethod("POST");
            connection.setRequestProperty("Content-Type", "application/json");
            connection.setRequestProperty("Accept", "application/json");
            if ("anthropic".equals(provider)) {
                connection.setRequestProperty("x-api-key", key);
                connection.setRequestProperty("anthropic-version", "2023-06-01");
            } else connection.setRequestProperty("Authorization", "Bearer " + key);
            connection.setDoOutput(true);
            connection.setFixedLengthStreamingMode(body.length);
            try (OutputStream stream = connection.getOutputStream()) { stream.write(body); }
            int code = connection.getResponseCode();
            if (code != 200) {
                if (code == 401 || code == 403) throw new ProviderError("Provider rejected the API key (" + code + ")");
                if (code == 429) throw new ProviderError("Provider rate limit reached (429)");
                throw new ProviderError("Provider returned HTTP " + code);
            }
            if (connection.getContentLengthLong() > MAX_RESPONSE_BYTES) throw new ProviderError("Provider response too large");
            ByteArrayOutputStream output = new ByteArrayOutputStream();
            try (InputStream stream = connection.getInputStream()) {
                byte[] chunk = new byte[8192];
                int read;
                while ((read = stream.read(chunk)) != -1) {
                    if (output.size() + read > MAX_RESPONSE_BYTES) throw new ProviderError("Provider response too large");
                    output.write(chunk, 0, read);
                }
            }
            JSONObject response = new JSONObject(new String(output.toByteArray(), StandardCharsets.UTF_8));
            if ("anthropic".equals(provider)) {
                JSONArray content = response.optJSONArray("content");
                if (content == null) throw new ProviderError("Provider returned invalid response");
                StringBuilder answer = new StringBuilder();
                for (int i = 0; i < content.length(); i++) {
                    JSONObject item = content.optJSONObject(i);
                    if (item != null && "text".equals(item.optString("type"))) {
                        answer.append(item.optString("text", ""));
                    }
                }
                return answer.toString();
            }
            JSONArray choices = response.optJSONArray("choices");
            if (choices == null || choices.length() == 0) throw new ProviderError("Provider returned invalid response");
            JSONObject message = choices.getJSONObject(0).optJSONObject("message");
            if (message == null) throw new ProviderError("Provider returned invalid response");
            return message.optString("content", "");
        } finally {
            connection.disconnect();
        }
    }

    private static String error(String message) {
        try { return new JSONObject().put("error", message).toString(); }
        catch (JSONException impossible) { return "{\"error\":\"Request failed\"}"; }
    }

    private static final class ProviderError extends Exception {
        ProviderError(String message) { super(message); }
    }
}
