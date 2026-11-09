package ai.mcpagent.preview;

import android.app.Activity;
import android.graphics.Color;
import android.net.Uri;
import android.os.Bundle;
import android.webkit.JavascriptInterface;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

import org.json.JSONObject;
import java.io.ByteArrayOutputStream;
import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;

/** Android uses the shared frontend with native HTTPS provider calls and local history. */
public final class MainActivity extends Activity {
    private static final String ORIGIN = "https://appassets.androidplatform.net";
    private WebView webView;
    private final ExecutorService requests = Executors.newSingleThreadExecutor();

    @Override public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        getWindow().setStatusBarColor(Color.rgb(12, 17, 27));
        getWindow().setNavigationBarColor(Color.rgb(12, 17, 27));

        FrameLayout root = new FrameLayout(this);
        root.setFitsSystemWindows(true);
        webView = new WebView(this);
        root.addView(webView, new FrameLayout.LayoutParams(-1, -1));
        setContentView(root);

        webView.setBackgroundColor(Color.rgb(12, 17, 27));
        WebSettings settings = webView.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(false);
        settings.setAllowFileAccess(false);
        settings.setAllowContentAccess(false);
        settings.setJavaScriptCanOpenWindowsAutomatically(false);
        settings.setMixedContentMode(WebSettings.MIXED_CONTENT_NEVER_ALLOW);
        WebView.setWebContentsDebuggingEnabled(false);

        MobileApi api = new MobileApi(getApplicationContext());
        webView.addJavascriptInterface(new Object() {
            @JavascriptInterface public void dispatch(String id, String json) {
                if (id == null || !id.matches("[0-9]{1,12}")) return;
                try {
                    requests.execute(() -> {
                        String response = api.request(json);
                        runOnUiThread(() -> {
                            if (webView != null) webView.evaluateJavascript(
                                "window.mcpNativeResult(" + JSONObject.quote(id) + "," +
                                    JSONObject.quote(response) + ")", null);
                        });
                    });
                } catch (RejectedExecutionException ignored) {
                    // Activity is closing and may no longer receive callbacks.
                }
            }
        }, "McpAndroidBridge");

        webView.setWebViewClient(new WebViewClient() {
            @Override public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
                // No external navigation, including to another same-origin HTML document.
                return true;
            }
            @Override public WebResourceResponse shouldInterceptRequest(WebView view, WebResourceRequest request) {
                Uri uri = request.getUrl();
                if (!"https".equals(uri.getScheme()) || !("appassets.androidplatform.net".equals(uri.getHost()))) {
                    return denied();
                }
                String file = uri.getPath();
                if ("/app.css".equals(file) || "/app.js".equals(file)) {
                    try {
                        InputStream stream = getAssets().open(file.substring(1));
                        return new WebResourceResponse(file.endsWith(".css") ? "text/css" : "text/javascript", "UTF-8", stream);
                    } catch (IOException ignored) { return denied(); }
                }
                return denied();
            }
        });
        try (InputStream input = getAssets().open("index.html")) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] chunk = new byte[8192];
            int read;
            while ((read = input.read(chunk)) != -1) {
                out.write(chunk, 0, read);
                if (out.size() > 256 * 1024) throw new IOException("UI asset too large");
            }
            webView.loadDataWithBaseURL(ORIGIN + "/", new String(out.toByteArray(), StandardCharsets.UTF_8),
                "text/html", "UTF-8", null);
        } catch (IOException error) {
            webView.loadData("<p>Bundled UI is unavailable.</p>", "text/html", "UTF-8");
        }
    }

    private static WebResourceResponse denied() {
        return new WebResourceResponse("text/plain", "UTF-8", 403, "Blocked",
            java.util.Collections.emptyMap(), new ByteArrayInputStream(new byte[0]));
    }

    @Override protected void onDestroy() {
        requests.shutdownNow();
        if (webView != null) {
            webView.removeJavascriptInterface("McpAndroidBridge");
            webView.destroy();
            webView = null;
        }
        super.onDestroy();
    }
}
