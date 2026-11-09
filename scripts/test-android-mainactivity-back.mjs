// Narrow regression checks for the generated Android MainActivity Back bridge.
// Run after: node scripts/patch-tauri-android.mjs
// Device Back dispatch is intentionally NOT claimed by these static/DOM tests.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
import vm from 'node:vm';

const root = fileURLToPath(new URL('..', import.meta.url));
const activityPath = path.join(root, 'src-tauri/gen/android/app/src/main/java/ai/mcpagent/app/MainActivity.kt');
const patchPath = path.join(root, 'scripts/patch-tauri-android.mjs');
const [source, patchSource] = await Promise.all([
  readFile(activityPath, 'utf8'),
  readFile(patchPath, 'utf8'),
]);

test('native Back hooks only the primary Wry WebView with one AndroidX callback', () => {
  assert.equal((source.match(/override fun onWebViewCreate\(webView: WebView\)/g) || []).length, 1);
  assert.equal((source.match(/onBackPressedDispatcher\.addCallback\(this,/g) || []).length, 1);
  assert.equal((source.match(/registerMcpAndroidBack\(\)/g) || []).length, 2);
  assert.ok(source.includes('mcpMainWebView = WeakReference(webView)'));
  assert.ok(source.includes('mcpMainWebView?.get() === webView'));
  assert.ok(source.includes('if (webView == null || !isMcpAppAssetUrl(initialUrl))'));
  assert.ok(source.includes('uri.host == "tauri.localhost"'));
  assert.ok(source.includes('uri.host == "localhost" && uri.scheme == "tauri"'));
  assert.ok(source.includes('uri.userInfo == null && uri.port == -1'));
  assert.ok(source.indexOf('if (webView == null || !isMcpAppAssetUrl(initialUrl))') <
    source.indexOf('webView.evaluateJavascript(mcpBackScript)'));
  assert.ok(source.includes('super@MainActivity.onBackPressed()'));
  assert.ok(source.includes('isEnabled = false'));
  assert.ok(source.includes('if (receipt == "false"'));
  assert.ok(source.includes('override fun onStop()'));
  assert.ok(source.includes('override fun onDestroy()'));
  assert.ok(!source.includes('loadUrl("javascript:'));
  assert.ok(!source.includes('findViewById<WebView>'));
});

test('main Activity dispatch has a cancelable synchronous defaultPrevented receipt', () => {
  const match = source.match(/private val mcpBackScript\s*=\s*"""([\s\S]*?)"""/);
  assert.ok(match, 'generated Kotlin must contain one fixed back dispatch script');
  assert.equal((source.match(/'mcp-android-back'/g) || []).length, 1);
  assert.ok(match[1].includes("new Event('mcp-android-back',{cancelable:true})"));
  const rootChat = new EventTarget();
  assert.equal(vm.runInNewContext(match[1], {Event, window:rootChat}), false);
  let count = 0;
  const setup = new EventTarget();
  setup.addEventListener('mcp-android-back', event => {
    assert.equal(event.cancelable, true);
    count += 1;
    event.preventDefault();
  });
  assert.equal(vm.runInNewContext(match[1], {Event, window:setup}), true);
  assert.equal(count, 1, 'one Back must dispatch exactly one event');
});

test('regeneration stays guarded, app-origin only and exactly once', () => {
  assert.ok(patchSource.includes('Generated MainActivity Back handler drifted'));
  assert.ok(patchSource.includes('Generated MainActivity Back integration must be unique'));
  assert.ok(patchSource.includes('fixedActivity !== activity'));
  assert.ok(patchSource.includes('String.raw'));
  assert.ok(patchSource.includes('private fun registerMcpAndroidBack()'));
  assert.ok(patchSource.includes('mcpBackInFlight'));
  assert.ok(patchSource.includes('Handler(Looper.getMainLooper()).postDelayed'));
});
