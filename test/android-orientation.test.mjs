import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const source = (file) => readFileSync(path.join(root, file), 'utf8');

test('Android Tauri keeps one main WebView window even when Windows distribution is removed', () => {
  // Tauri "app.windows" is cross-platform WebView creation configuration,
  // not a Windows installer setting. [] renders a blank Android Activity.
  const config = JSON.parse(source('src-tauri/tauri.conf.json'));
  assert.deepEqual(config.app.windows?.map(window => window.label), ['main']);
  assert.equal(config.app.windows[0].title, 'Mcp Agent Ai');
  assert.equal(config.build.frontendDist, '../ui');
  assert.equal(config.bundle.active, false, 'Android must not create a desktop installer');
  const patcher = source('scripts/patch-tauri-android.mjs');
  assert.match(patcher, /Android requires exactly one Tauri main WebView window/,
    'Android prebuild must reject a blank-screen APK');
});

test('Android Chrome-like browser is one right-edge native owned drawer with real close and safe HTTPS search', () => {
  const kotlin=source('android/PrivateBrowserPlugin.kt');
  const ui=source('ui/app.ts');
  const html=source('ui/index.html');
  assert.match(kotlin,/setGravity\(Gravity\.END or Gravity\.TOP\)/);
  assert.match(kotlin,/drawerWidthDp\(screenWidthDp: Int\)/);
  assert.match(kotlin,/if \(screenWidthDp <= HANDSET_DRAWER_MAX_DP\) screenWidthDp/);
  assert.match(kotlin,/else minOf\(TABLET_DRAWER_MAX_DP/);
  assert.match(kotlin,/addressOrSearch\(/);
  assert.match(kotlin,/URLEncoder\.encode\(input, "UTF-8"\)/);
  assert.match(kotlin,/https:\/\/www\.google\.com\/search\?q=/);
  assert.match(kotlin,/setOnDismissListener \{[\s\S]*?resetBrowser\(discardRetained = false\)/,
    'Android lifecycle dismissal must preserve the owned profile receipt');
  assert.match(kotlin,/setOnDismissListener\(null\)[\s\S]*?resetBrowser\(\)/,
    'Explicit close-all must detach stale dismissal callbacks before teardown');
  assert.match(kotlin,/tabs\.values\.toList\(\)\.forEach\(::destroyTab\)/);
  assert.match(kotlin,/"native_right_drawer"/);
  assert.match(html,/id="androidBrowserDrawerToggle"/);
  assert.match(html,/data-view-action="browser-tools"/);
  assert.match(ui,/async function openAndroidBrowserDrawer\(\)/);
  assert.match(ui,/closeAndroidViewMenu\(\);\s+closeSidebar\(\);\s+closeToolMenu\(\);/);
  assert.match(ui,/url:'https:\/\/www\.google\.com\/',userInitiated:true/);
  assert.match(ui,/case 'browser': void openAndroidBrowserDrawer\(\)/);
  assert.match(ui,/case 'browser-tools': openAndroidBrowserControls\(\)/);
});

test('Android Prime has real bounded native worker lifecycle and cannot silently gain tools', () => {
  const html=source('ui/index.html');
  const ui=source('ui/app.ts');
  const rust=source('native-core/src/lib.rs');
  const store=source('native-core/src/mobile_prime.rs');
  for(const id of ['mobilePrimeSection','mobilePrimeCreateForm','mobilePrimeObjective',
    'mobilePrimeWorkerRole','mobilePrimeWorkerTask','mobilePrimeWorkerForm',
    'mobilePrimeWorkerList','mobilePrimeRefresh','mobilePrimeFinish','mobilePrimeCancel'])
    assert.match(html,new RegExp('id="'+id+'"'),id+' must be in Sub-agents');
  assert.match(ui,/api\('\/api\/mobile-prime'/);
  assert.match(ui,/action:'spawn'/);
  assert.match(ui,/action:'message'/);
  assert.match(ui,/action:'finish'/);
  assert.match(ui,/action:'cancel'/);
  assert.match(ui,/mobileAiDelegateSelection\(\)/);
  assert.match(rust,/"\/api\/mobile-prime" \| "\/api\/mobile-prime-agent"/);
  assert.match(rust,/mobile_prime_agent_request/);
  assert.match(rust,/body\["allowMcpTools"\] == true \|\| body\["allowLocalTools"\] == true/);
  assert.match(store,/const MAX_WORKERS: usize = 2/);
  assert.match(store,/Running.*Interrupted|WorkerStatus::Interrupted/s);
});

test('Android Step3 uses official Sign in with ChatGPT OAuth PKCE and requires explicit app approval before Bring Session', () => {
  const html = source('ui/index.html');
  const frontend = source('ui/app.ts');
  const kotlin = source('android/ChatGptOAuthPlugin.kt');
  const rust = source('src-tauri/src/android_chatgpt_oauth.rs');
  const commands = source('src-tauri/src/lib.rs');
  for (const id of ['androidCosBrowserChoices','androidCosBrowserOpen','androidCosBrowserBring',
    'androidCosBrowserSignInStatus']) {
    assert.match(html, new RegExp('id="' + id + '"'), id);
  }
  const browserSection = html.split('id="androidCosBrowserChoices"')[1]?.split('id="androidCosFolderPanel"')[0] || '';
  assert.equal((browserSection.match(/<button\b/g) || []).length,2,
    'Step 3 should have only the two user-requested actions');
  assert.match(browserSection, /<strong>Sign in to ChatGPT<\/strong>/);
  assert.match(browserSection, /<strong>Bring Session<\/strong>/);
  assert.match(browserSection, /Chrome|system browser/i);
  assert.match(browserSection, /Mcp Agent Ai|Approve|authoriz/i);
  assert.doesNotMatch(browserSection, /Open the private ChatGPT website browser|Chrome session cannot be brought|email\/password option|OpenRouter|Claude/i);
  assert.doesNotMatch(html,
    /id="androidCosChooseBuiltIn"|id="androidCosChooseGoogle"|id="androidCosBrowserShow"|id="androidCosBrowserCheck"|id="androidCosBrowserStop"|id="androidCosBrowserSignIn"/);
  for (const command of ['android_chatgpt_oauth_start','android_chatgpt_oauth_status',
    'android_chatgpt_oauth_bring']) assert.match(commands,new RegExp(command+',?'));
  assert.match(kotlin, /fun start\(invoke: Invoke\)/);
  assert.match(kotlin, /fun status\(invoke: Invoke\)/);
  assert.match(kotlin, /fun bring\(invoke: Invoke\)/);
  assert.match(kotlin, /ChatGptOAuthPlugin/);
  assert.match(kotlin, /val verifier = randomToken\(64\)/);
  assert.match(kotlin, /val state = randomToken\(32\)/);
  assert.match(kotlin, /val nonce = randomToken\(32\)/);
  assert.match(kotlin, /ServerSocket\(0, 1, InetAddress\.getByName\("127\.0\.0\.1"\)\)/);
  assert.match(kotlin, /"dynamic_agent_client"/);
  assert.match(kotlin, /"ext_agent_host_id"/);
  assert.match(kotlin, /"agent_name_hint", "Mcp Agent Ai"/);
  assert.match(kotlin, /"code_challenge_method", "S256"/);
  assert.match(kotlin, /"openid profile email offline_access resource\.invoke chatgpt\.tokens\.use\.direct"/);
  assert.match(kotlin, /CustomTabsIntent\.Builder\(\)/);
  assert.match(kotlin, /\.launchUrl\(activity, url\)/);
  assert.match(kotlin, /if \(!args\.userInitiated\)/);
  assert.match(kotlin, /val current = loadRecord\(\)/);
  assert.match(kotlin, /!current\.canUsePlan\(\)/);
  assert.match(kotlin, /requestJson\(CATALOG, null, valid\.accessToken\)/);
  assert.match(kotlin, /storeRecord\(valid\.copy\(linked = true\)\)/);
  assert.match(kotlin, /"AndroidKeyStore"/);
  assert.match(kotlin, /KeyProperties\.BLOCK_MODE_GCM/);
  assert.match(rust, /fn validate_status\(value: Value\)/);
  assert.match(rust, /"message,pending,planEnabled,sessionLinked,signInVerified,state,supported"|"signInVerified"/);
  assert.match(rust, /pub fn start\(/);
  assert.match(rust, /pub fn status\(/);
  assert.match(rust, /pub fn bring\(/);
  const browserHandlers = frontend.slice(frontend.indexOf("$('androidCosBrowserOpen').addEventListener"),
    frontend.indexOf("$('androidCosFolderBrowse').addEventListener"));
  assert.ok(browserHandlers.length > 200, 'Step3 handlers must exist before the single Step8 Connect handler');
  assert.match(browserHandlers, /android_chatgpt_oauth_start/);
  assert.match(browserHandlers, /android_chatgpt_oauth_bring/);
  assert.doesNotMatch(browserHandlers, /android_chatgpt_bring_session|changeOwnedChatgpt\('open'\)/,
    'Official account approval must not fall back to isolated WebView login');
  assert.match(frontend, /android_chatgpt_oauth_status/);
  assert.match(frontend, /validateAndroidCosOAuthStatus/);
  assert.match(frontend, /status\.planEnabled === true/);
  assert.match(frontend, /androidCosSetupResult\('browser'\)/);
  assert.match(frontend, /requirements\.every\(item => item\.verified \|\| item\.notNeeded\)/);
  assert.match(frontend, /chooseAndroidCosSetupStep\('folder'\)/);
  assert.doesNotMatch(frontend, /androidCosAdvancedFooter|androidCosTunnelAdvanced/,
    'Android Setup must not retain the removed Advanced tunnel panel');
  assert.doesNotMatch(browserHandlers, /document\.cookie|__Secure-next-auth\.session-token|accessToken|refreshToken/);
});

test('Separate Android ChatGPT website browser retains its own controlled chat lease, never receives OAuth secrets', () => {
  const kotlin = source('android/BackgroundChatGptPlugin.kt');
  const frontend = source('ui/app.ts');
  assert.match(kotlin, /text = "← Back to ChatGPT"/);
  assert.match(kotlin, /setOnClickListener \{ returnToChatGpt\(\) \}/);
  assert.match(kotlin, /private fun returnToChatGpt\(\)/);
  assert.match(kotlin, /text = "Back to Setup"/);
  assert.match(kotlin, /WebViewCompat\.setProfile\(web, PROFILE\)/);
  assert.match(kotlin, /private fun sessionLinked\(\): Boolean|private fun sessionLinked\(\): Boolean =/);
  assert.match(kotlin, /fun bringSession\(invoke: Invoke\)/);
  assert.match(kotlin, /fun checkSession\(invoke: Invoke\)/);
  assert.match(kotlin, /private fun verifySignIn\(/);
  assert.match(kotlin, /fun prepare\(/);
  assert.match(kotlin, /fun send\(/);
  assert.match(kotlin, /fun observe\(/);
  assert.match(kotlin, /appleAuthentication\(url\)/);
  assert.match(kotlin, /host == "appleid\.apple\.com" \|\| host == "idmsa\.apple\.com"/);
  assert.match(frontend, /androidOwnedChatgptLinked\(\)/);
  assert.doesNotMatch(kotlin, /getCookie\(|put\("accessToken"|put\("refreshToken"|document\.cookie/,
    'The website WebView must never export cookies or receive native OAuth credentials');
});

test('Android ChatGPT Send uses approved SIWC Responses API without MCP Plugin or website Prepare', () => {
  const frontend = source('ui/app.ts');
  const native = source('android/ChatGptOAuthPlugin.kt');
  const tauri = source('src-tauri/src/lib.rs');
  const oauth = source('src-tauri/src/android_chatgpt_oauth.rs');
  const core = source('native-core/src/lib.rs');
  const submit = frontend.slice(frontend.indexOf('async function submitAuthorizedChatgpt('),
    frontend.indexOf("$('chatForm').addEventListener('submit'"));
  assert.ok(submit.length > 600, 'Dedicated SIWC Send implementation required');
  assert.match(submit, /androidCosOAuthLinked\(\)/);
  assert.doesNotMatch(submit, /window\.confirm\(/,
    'Chat Send already provides one user-initiated action; do not interrupt every turn with a second confirmation');
  assert.match(submit, /'chatgpt_oauth_chat'/);
  assert.match(submit, /chatgpt_oauth_turn/);
  assert.doesNotMatch(submit, /chatgpt_browser_prepare|chatgpt_browser_send|androidOpenAiTunnelVerified|androidCosSetupResult\('mcpPlugin'\)/);
  assert.match(frontend, /if \(nativeAndroidRuntime\) await submitAuthorizedChatgpt\(prompt\)/);
  assert.match(native, /fun chat\(invoke: Invoke\)/);
  assert.match(native, /chatgpt\.tokens\.use\.direct/);
  assert.match(native, /"models"/);
  assert.match(native, /"visibility"/);
  assert.match(native, /"slug"/);
  assert.match(native, /"\/v1\/responses"|"\$RESOURCE\/responses"/);
  assert.match(native, /\.put\("store", false\)/);
  assert.match(native, /\.put\("stream", true\)/);
  assert.match(native, /"response\.completed"/);
  assert.match(native, /mediaType != null && mediaType\.isNotEmpty\(\) && mediaType != "text\/event-stream"/,
    'Absent Content-Type may be parsed only by the strict completed SSE verifier');
  assert.match(native, /spendChatRequest\(id\)/);
  assert.match(tauri, /async fn chatgpt_oauth_chat\(/);
  assert.match(oauth, /pub fn chat\(/);
  assert.match(core, /pub fn persist_oauth_chat_turn\(/);
  assert.match(core, /"chatgpt_oauth_turn"/);
  assert.match(core, /pub fn claim_oauth_chat_request\(/);
});

function launcherActivity(xml) {
  const matches = [...xml.matchAll(/<activity\b[^>]*>/g)]
    .map(match => match[0])
    .filter(tag => tag.includes('android:name=".MainActivity"'));
  assert.equal(matches.length, 1, 'Android package must have one main launcher activity');
  return matches[0];
}

test('Android standalone launcher requests fixed portrait without changing device rotation', () => {
  const manifest = source('android/app/src/main/AndroidManifest.xml');
  assert.match(launcherActivity(manifest), /android:screenOrientation="portrait"/);
  const patcher = source('scripts/patch-tauri-android.mjs');
  assert.match(patcher, /android:screenOrientation="portrait"/);
  assert.match(patcher, /SCREEN_ORIENTATION_PORTRAIT/);
  assert.match(patcher, /android:windowSoftInputMode="adjustResize"/);
  assert.doesNotMatch(patcher, /settings put system accelerometer_rotation/);
  assert.doesNotMatch(patcher, /settings put system user_rotation/);
});

test('Generated Tauri Android activity stays portrait on launch and on resume', () => {
  const generatedManifest = path.join(root, 'src-tauri/gen/android/app/src/main/AndroidManifest.xml');
  const generatedActivity = path.join(
    root, 'src-tauri/gen/android/app/src/main/java/ai/mcpagent/app/MainActivity.kt');
  // Fresh environments without Tauri-generated sources can still run Node tests;
  // a native Android build requires these files and re-applies the guarded patch.
  if (!existsSync(generatedManifest) || !existsSync(generatedActivity)) return;
  const tag = launcherActivity(readFileSync(generatedManifest, 'utf8'));
  assert.equal((tag.match(/android:screenOrientation=/g) || []).length, 1);
  assert.match(tag, /android:screenOrientation="portrait"/);
  assert.match(tag, /android:windowSoftInputMode="adjustResize"/);
  const activity = readFileSync(generatedActivity, 'utf8');
  assert.match(activity, /override fun onCreate\(savedInstanceState: Bundle\?\)/);
  assert.match(activity, /override fun onResume\(\)/);
  assert.equal((activity.match(/requestedOrientation = ActivityInfo\.SCREEN_ORIENTATION_PORTRAIT/g) || []).length, 2);
  assert.match(activity, /if \(requestedOrientation != ActivityInfo\.SCREEN_ORIENTATION_PORTRAIT\)/);
});

test('Android foreground runtime behaves like Termux: auto background session and two notification actions', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const patcher = source('scripts/patch-tauri-android.mjs');
  const plugin = source('android/ForegroundRuntimePlugin.kt');
  const service = source('android/ForegroundRuntimeService.kt');
  const rust = source('src-tauri/src/android_foreground_runtime.rs');
  const lib = source('src-tauri/src/lib.rs');
  assert.doesNotMatch(html, /id="androidForegroundRuntime(?:Section|Title|Status|Feedback|Start|Stop)"/,
    'Android General must not expose Background Session settings, status or buttons');
  assert.doesNotMatch(html, /<h2[^>]*>Background session<\/h2>/i);
  assert.doesNotMatch(ui, /androidForegroundRuntime|android_foreground_runtime_(?:start|stop|status)/,
    'Renderer must not poll or control native background sessions from any Settings tab');
  const setup = html.split('id="androidCosSetupWizard"')[1]?.split('id="storageAccessSection"')[0] || '';
  assert.doesNotMatch(setup, /androidForegroundRuntime/);
  assert.match(lib, /android_foreground_runtime_status/);
  assert.match(lib, /android_foreground_runtime::init\(\)/);
  assert.match(rust, /android_foreground_runtime_status/);
  assert.match(patcher, /ForegroundRuntimePlugin\.kt/);
  assert.match(patcher, /ForegroundRuntimeService\.kt/);
  assert.match(patcher, /android\.permission\.FOREGROUND_SERVICE/);
  assert.match(patcher, /android\.permission\.FOREGROUND_SERVICE_SPECIAL_USE/,
    'Android 14+ requires a foreground-service type permission');
  assert.match(patcher, /android\.permission\.WAKE_LOCK/,
    'Termux-style partial wake lock needs WAKE_LOCK manifest permission');
  assert.match(patcher, /ForegroundRuntimeService/);
  assert.match(patcher, /android:exported="false"/);
  assert.match(patcher, /android:stopWithTask="false"/);
  assert.match(plugin, /ForegroundRuntimeService/);
  assert.match(plugin, /override fun onActivityPaused\(/,
    'The native Android Activity pause callback must start the foreground session');
  assert.match(plugin, /override fun onActivityResumed\(/,
    'Returning to the Activity must hide its session notification');
  assert.match(plugin, /registerActivityLifecycleCallbacks\(/,
    'A native lifecycle callback must remain registered even when the renderer is suspended');
  assert.match(plugin, /startForegroundService\(/);
  const resume = plugin.slice(plugin.indexOf('override fun onActivityResumed('),
    plugin.indexOf('override fun onActivitySaveInstanceState('));
  assert.match(resume, /setActivityVisible\(true\)/);
  assert.match(resume, /ForegroundRuntimeService\.ACTION_HIDE/,
    'Resuming must tell native service to remove the notification');
  const pause = plugin.slice(plugin.indexOf('override fun onActivityPaused('),
    plugin.indexOf('\n  }\n\n  init {'));
  assert.match(pause, /setActivityVisible\(false\)/);
  assert.match(pause, /ForegroundRuntimeService\.ACTION_START/);
  assert.doesNotMatch(pause, /ForegroundRuntimeService\.isRunning\(\)/,
    'Each pause must restore the notification even if an earlier service instance exists');
  assert.match(service, /startForeground\(/, 'Service must post a real Android foreground notification');
  assert.match(service, /const val ACTION_HIDE/);
  assert.match(service, /@Volatile private var activityVisible/);
  assert.match(service, /if \(activityVisible\) hideForegroundNotification\(\)/,
    'A delayed START after resume must never leave an in-app notification visible');
  assert.match(service, /ACTION_HIDE\s*->\s*\{[^}]*if \(activityVisible\) hideForegroundNotification\(\)/,
    'A delayed HIDE after another pause must not suppress its background notification');
  const hide = service.slice(service.indexOf('private fun hideForegroundNotification()'),
    service.indexOf('private fun stopRunning('));
  assert.match(hide, /stopForeground\(STOP_FOREGROUND_REMOVE\)/);
  assert.doesNotMatch(hide, /stopSelf\(|releaseWakeLock\(/,
    'Hiding a notification on resume must preserve the started session and any user-held wake lock');
  assert.match(service, /setContentTitle\("Mcp Agent Ai"\)/,
    'Notification title must match Termux-style Mcp Agent Ai exactly');
  assert.match(service, /1 session/);
  assert.match(service, /1 session \(wake lock held\)/);
  assert.match(service, /PendingIntent\.getService\(/);
  assert.match(service, /PendingIntent\.FLAG_IMMUTABLE/);
  assert.match(service, /"Exit"/);
  assert.match(service, /"Acquire wakelock"/);
  assert.match(service, /"Release wakelock"/);
  assert.equal((service.match(/\.addAction\(/g) || []).length, 2,
    'Notification must expose exactly two actions, Exit + dynamic Acquire/Release wakelock');
  assert.match(service, /PowerManager\.PARTIAL_WAKE_LOCK/);
  assert.match(service, /\bacquire\(/);
  assert.match(service, /\.release\(/);
  assert.match(service, /ACTION_EXIT/);
  assert.match(service, /ACTION_WAKE_LOCK/);
  assert.match(service, /ACTION_WAKE_UNLOCK/);
  assert.match(service, /ACTION_EXIT\s*->\s*\{[^}]*stopRunning\(startId\)[^}]*Process\.killProcess\(Process\.myPid\(\)\)/,
    'Exit must end the service and its app process so the paused lifecycle cannot silently restart it');
  assert.match(service, /onDestroy\(/);
  assert.match(service, /START_NOT_STICKY/,
    'Android must not revive the session after process removal or device restart');
  assert.doesNotMatch(service, /START_STICKY|START_REDELIVER_INTENT|BOOT_COMPLETED/);
  assert.doesNotMatch(patcher, /RECEIVE_BOOT_COMPLETED|android\.intent\.action\.BOOT_COMPLETED/);
  assert.doesNotMatch(source('ui/app.css'), /#androidForegroundRuntimeSection/,
    'Removed General section must not retain CSS selectors');
});

test('Android battery shows only package-scoped OS consent, never a second app-info action', () => {
  const kotlin = source('android/StorageAccessPlugin.kt');
  const patcher = source('scripts/patch-tauri-android.mjs');
  const bridge = source('src-tauri/src/storage_access.rs');
  const html = source('ui/index.html');
  assert.match(patcher, /android\.permission\.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS/);
  assert.match(kotlin, /"battery" -> Intent\(Settings\.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS/);
  assert.match(kotlin, /Settings\.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS/);
  assert.match(kotlin, /Uri\.parse\("package:\$\{activity\.packageName\}"\)/);
  assert.doesNotMatch(kotlin, /"battery_details"/);
  assert.doesNotMatch(bridge, /"battery_details"/);
  assert.doesNotMatch(html, /id="batteryAppInfo"/);
  assert.equal((html.match(/data-permission-request="battery"/g) || []).length, 1);
  assert.doesNotMatch(kotlin, /"battery" -> Intent\(Settings\.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS\)/);
  assert.match(kotlin, /\.isIgnoringBatteryOptimizations\(activity\.packageName\)/);
  const generatedManifest = path.join(root, 'src-tauri/gen/android/app/src/main/AndroidManifest.xml');
  if (existsSync(generatedManifest)) {
    const xml = readFileSync(generatedManifest, 'utf8');
    // Once the Android generation patch has run, this permission must exist
    // exactly once; merely opening the OS dialog is not granting exemption.
    const count = (xml.match(/android\.permission\.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS/g) || []).length;
    assert.ok(count <= 1, 'Battery exemption manifest permission must be unique');
  }
});

test('One-frame screen capture is absent from Setup and from the packaged Android bridge', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const tauri = source('src-tauri/src/lib.rs');
  const patcher = source('scripts/patch-tauri-android.mjs');
  for (const id of ['screenCaptureOne', 'screenCapturePreview', 'screenCaptureState', 'screenCaptureCancel']) {
    assert.doesNotMatch(html, new RegExp(`id="${id}"`));
    assert.doesNotMatch(ui, new RegExp(`\\$\\('${id}'\\)`));
  }
  assert.doesNotMatch(tauri, /android_screen_capture::/);
  assert.doesNotMatch(ui, /['"]screen_capture_one_frame['"]/);
  assert.doesNotMatch(ui, /['"]screen_capture_status['"]/);
  const stagedKotlin = patcher.split('for (const file of [')[1]?.split(']) {')[0] ?? '';
  assert.doesNotMatch(stagedKotlin, /ScreenCaptureService|OneShotCapturePlugin/);
  assert.match(patcher, /'ScreenCaptureService\.kt', 'OneShotCapturePlugin\.kt'/);
  const generated = 'src-tauri/gen/android/app/src/main/';
  const manifest = path.join(root, generated, 'AndroidManifest.xml');
  if (existsSync(manifest)) {
    const xml = readFileSync(manifest, 'utf8');
    assert.doesNotMatch(xml, /FOREGROUND_SERVICE_MEDIA_PROJECTION|ScreenCaptureService|mediaProjection/);
    const dir = path.join(root, generated, 'java/ai/mcpagent/app');
    assert.equal(existsSync(path.join(dir, 'ScreenCaptureService.kt')), false);
    assert.equal(existsSync(path.join(dir, 'OneShotCapturePlugin.kt')), false);
  }
  assert.doesNotMatch(ui, /accessibility_list_windows|accessibility_snapshot|accessibility_click/);
});

test('Accessibility access stays while only its lower helper text and selected-window controls are removed', () => {
  const html = source('ui/index.html');
  const app = source('ui/app.ts');
  const handler = source('src-tauri/src/lib.rs');
  const patcher = source('scripts/patch-tauri-android.mjs');
  for (const id of ['accessibilityControls','accessibilityListWindows','accessibilityWindowOptions',
    'accessibilitySnapshot','accessibilitySnapshotOutput','accessibilityActionForm',
    'accessibilityFeedback','storageAccessFeedback','androidCosAllowNext','refreshAndroidPermissions']) {
    assert.doesNotMatch(html, new RegExp(`id="${id}"`));
    assert.doesNotMatch(app, new RegExp(`\\$\\('${id}'\\)`));
  }
  assert.equal((html.match(/data-permission-row="/g) || []).length, 5);
  assert.match(html, /id="accessibilityPermissionState"/);
  assert.match(html, /id="accessibilityManage"/);
  assert.match(html, /id="accessibilityReconnectHint"/);
  assert.match(app, /'accessibility_status'/);
  assert.match(app, /'accessibility_request_settings'/);
  assert.match(app, /needsReconnect/);
  assert.match(app, /Reconnect needed/);
  assert.match(app, /androidAccessibilityRefreshPending/);
  assert.match(source('android/AgentAccessibilityService.kt'), /boundToSystem/);
  assert.match(source('android/AccessibilityHealthProvider.kt'), /AgentAccessibilityService\.isConnected\(\)/);
  assert.match(patcher, /android:process=":accessibility"/);
  assert.match(patcher, /AccessibilityHealthProvider/);
  assert.doesNotMatch(html, /Statuses come from Android|Allow next missing permission|Refresh permission status/);
  const accessRow = html.split('data-permission-row="accessibility"')[1]?.split('</div>\n            </div>')[0] ?? '';
  assert.match(accessRow, /Accessibility service/);
  assert.doesNotMatch(accessRow, /<p\b|selected-window|Find active windows/i);
  for (const command of ['list_windows','select_window','snapshot','click','fill','scroll']) {
    assert.doesNotMatch(handler, new RegExp(`android_accessibility::accessibility_${command}\\b`));
    assert.doesNotMatch(app, new RegExp(`['"]accessibility_${command}['"]`));
  }
  assert.match(handler, /mod android_accessibility/);
  assert.match(handler, /android_accessibility::accessibility_status/);
  assert.match(handler, /android_accessibility::accessibility_request_settings/);
  assert.match(patcher, /AgentAccessibilityService\.kt', 'AccessibilityToolsPlugin\.kt'/);
  assert.match(patcher, /copyFile\(path\.join\(root, 'android', 'accessibility_config\.xml'/);
  // The old selected-window Setup controls stay removed, but the separate,
  // approval-gated MCP app automation now needs OS-declared capabilities.
  const accessibilityConfig = source('android/accessibility_config.xml');
  assert.match(accessibilityConfig, /android:canRetrieveWindowContent="true"/);
  assert.match(accessibilityConfig, /android:canPerformGestures="true"/);
  assert.match(accessibilityConfig, /android:canTakeScreenshot="true"/);
  const appAutomationService = source('android/AgentAccessibilityService.kt');
  assert.match(appAutomationService, /hasNodePermission\(\)/);
  assert.match(appAutomationService, /hasGesturePermission\(\)/);
  assert.match(appAutomationService, /hasScreenshotPermission\(\)/);
});

test('Android Accessibility watchdog continuously supervises OS rebinding and delays Reconnect while recovering', () => {
  const service = source('android/AgentAccessibilityService.kt');
  const provider = source('android/AccessibilityHealthProvider.kt');
  const plugin = source('android/AccessibilityToolsPlugin.kt');
  const ui = source('ui/app.ts');
  const html = source('ui/index.html');
  assert.match(service, /onServiceConnected\(\)/);
  assert.match(service, /connectedSinceElapsedMs/);
  assert.match(provider, /connectedSinceElapsedMs/);
  assert.match(plugin, /ContentObserver/);
  assert.match(plugin, /ActivityLifecycleCallbacks/);
  assert.match(plugin, /REBIND_GRACE_MS\s*=\s*45_000L/);
  assert.match(plugin, /unboundSinceElapsedMs/);
  assert.match(plugin, /WATCH_RECOVERING_MS/);
  assert.match(plugin, /WATCH_CONNECTED_MS/);
  assert.match(plugin, /WATCH_USER_ACTION_MS/);
  assert.match(plugin, /main\.postDelayed\(this, delay\)/);
  assert.doesNotMatch(plugin, /WATCH_MAX_CHECKS|watchChecksRemaining/,
    'Foreground Accessibility watchdog must continue after the old 16-check budget');
  assert.match(plugin, /observeLiveStatus\(\)/);
  assert.match(plugin, /accessibilityGloballyDisabled\(\)/);
  assert.match(plugin, /!globallyDisabled && connectedByServiceProcess\(\)/);
  assert.match(plugin, /now - unboundSinceElapsedMs >= REBIND_GRACE_MS/);
  assert.match(plugin, /else if \(globallyDisabled\)/);
  assert.match(plugin, /onActivityPaused\(a: Activity\)/);
  assert.match(ui, /lastAndroidAccessibilitySetupPollAt/);
  assert.match(ui, /androidCosSetupSelected === 'access' \? 2_500 : 12_000/);
  assert.match(ui, /const recovering = status\.enabled && !status\.connected && !status\.needsReconnect/);
  assert.match(ui, /'Recovering…'/);
  assert.match(ui, /recovering \? 'Checking…' : status\.enabled \? 'Reconnect' : 'Allow'/);
  assert.match(ui, /button\.disabled = !status\.supported \|\| \(status\.enabled && \(status\.connected \|\| recovering\)\)/);
  assert.match(ui, /status\.needsReconnect \? 'Reconnect needed' : 'Recovering…'/);
  assert.match(ui, /badge\.dataset\.connected = String\(status\.supported && status\.enabled && status\.connected\)/);
  assert.doesNotMatch(plugin, /Settings\.Secure\.(?:putInt|putString)|WRITE_SECURE_SETTINGS|disableSelf\(|setComponentEnabledSetting\(|bindService\(/);
  assert.doesNotMatch(html, /STEP 01 \/ 09|Make the features you use ready\./);
  assert.equal((html.match(/data-permission-row="/g) || []).length, 5);
});

test('Android Setup keeps only native OpenAI Secure MCP Tunnel, removing Advanced, Cloudflare and local tunnel UI', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  assert.match(html, /id="androidCosTunnelMethod"/);
  assert.match(html, /id="androidOpenAiTunnelId"/);
  assert.match(html, /id="androidTunnelSaveId"/);
  assert.match(html, /id="androidCosChatgptConnect"/);
  assert.match(html, /id="androidCosTunnelDisconnect"/);
  assert.doesNotMatch(html, /id="androidCosMcpPluginSkip"|id="androidOpenAiTunnelStandalone"/);
  for (const removedId of [
    'androidCosAdvancedFooter','androidCosTunnelAdvanced',
    'androidCosOpenAdvanced','androidCosAdvancedShortcut'
  ]) assert.doesNotMatch(html,new RegExp(`id="${removedId}"`),`Removed Android tunnel UI: ${removedId}`);
  const compat = html.split('id="androidTunnelLegacyCompat"')[1]
    ?.split('id="androidCosKeyPanel"')[0] || '';
  assert.match(html, /id="androidTunnelLegacyCompat" hidden inert aria-hidden="true"/,
    'Legacy controls must be wrapped in an explicitly hidden compatibility container');
  assert.match(compat, /id="androidOpenAiTunnelMethodSelect"/);
  assert.match(compat, /<option value="openai">OpenAI Secure MCP Tunnel<\/option>/);
  assert.doesNotMatch(compat, /<option value="cloudflared"|<option value="manual"/,
    'No alternative method may remain selectable in Android Setup');
  for (const legacyId of ['androidLocalMcpControls','androidLocalMcpStart','androidLocalMcpStop',
    'androidOpenAiTunnelRuntime','androidCosTunnelLifecycle','androidCosAlternativeMethodStatus'])
    assert.match(compat,new RegExp(`id="${legacyId}"`),
      'The retained bridge-only binding must remain inside the hidden wrapper: ' + legacyId);
  assert.doesNotMatch(html, /Advanced — method, program, startup|Cloudflare quick tunnel|Local only — I run my own tunnel|tunnel-client\.exe/);
  assert.doesNotMatch(ui, /\$\('androidCosTunnelAdvanced'\)|\$\('androidCosAdvancedFooter'\)/,
    'The renderer must not reference eliminated tunnel UI nodes');
  const method = html.split('id="androidCosTunnelMethod"')[1]?.split('</div>\n            </div>')[0] || '';
  assert.doesNotMatch(method, /tunnel-client\.exe|C:\\|id="androidTunnelBrowse"|>Browse<\/button>/);
  assert.match(ui, /androidOpenAiTunnelSnapshot\.toolCallsVerified > 0/);
  assert.doesNotMatch(ui, /androidOpenAiTunnelSkipped|androidCosMcpPluginSkip|androidOpenAiTunnelStandalone/,
    'Android Setup must not have a bypass for real Core Plugin pairing');
  assert.match(ui, /android_openai_tunnel_status/);
  assert.match(ui, /android_openai_tunnel_connect_stored/);
  assert.match(ui, /android_openai_tunnel_disconnect/);
  // Removal is presentation-only; the bounded localhost dispatcher remains
  // separately testable until its native API is intentionally retired.
  const rust = source('src-tauri/src/android_local_mcp.rs');
  assert.match(rust, /TcpListener::bind\("127\.0\.0\.1:0"\)/);
  assert.match(rust, /let secret = format!/);
  assert.match(rust, /super::android_openai_tunnel::handle_rpc/);
  assert.doesNotMatch(rust, /Command::new\(|spawn\(binary\)/);
});

test('All ten Android Setup stages are ordered and independently viewable with real controls or explicit unsupported state', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const css = source('ui/app.css');
  const order = ['access','extension','browser','folder','tools','tunnel','key','connect','mcpPlugin','ready'];
  assert.deepEqual([...html.matchAll(/data-cos-setup-step="([^"]+)"/g)].map(match => match[1]), order);
  assert.match(ui, /const androidCosSetupOrder = \['access','extension','browser','folder','tools','tunnel','key','connect','mcpPlugin','ready'\] as const/);
  assert.match(html, /id="androidCosSetupProgress"[^>]+aria-valuemax="10"/);
  assert.match(ui, /of \$\{androidCosSetupOrder\.length\} completed/);
  for (const [step,panel] of Object.entries({
    extension:'androidCosExtensionControls',browser:'androidCosBrowserChoices',
    folder:'androidCosFolderPanel',tools:'androidCosAccessPanel',tunnel:'androidCosTunnelMethod',
    key:'androidCosKeyPanel',connect:'androidCosConnectPanel',mcpPlugin:'androidCosMcpPluginPanel',
    ready:'androidCosReadyPanel'
  })) {
    assert.match(html,new RegExp(`data-cos-setup-step="${step}"`));
    assert.match(html,new RegExp(`id="${panel}"`));
  }
  const accessPanel = html.split('id="androidCosAccessPanel"')[1]?.split('id="androidCosTunnelMethod"')[0] || '';
  const tunnelPanel = html.split('id="androidCosTunnelMethod"')[1]?.split('id="androidCosKeyPanel"')[0] || '';
  assert.match(accessPanel,/id="androidMcpAccessPresets"/);
  assert.doesNotMatch(accessPanel,/id="androidMcpAccessNext"|Continue to Tunnel/);
  assert.match(ui,/configureAndroidMcpAccess\(target\.value as AndroidMcpPreset,[\s\S]*?true\)/);
  assert.doesNotMatch(accessPanel,/id="androidOpenAiTunnelId"|id="androidOpenAiTunnelKey"|id="androidLocalMcpControls"/);
  assert.match(tunnelPanel,/id="androidOpenAiTunnelId"/);
  assert.match(tunnelPanel,/id="androidCosChatgptPanel"/);
  assert.doesNotMatch(tunnelPanel,/id="androidMcpAccessPresets"/);
  assert.match(ui, /button\.disabled = false/);
  assert.doesNotMatch(ui, /if \(androidCosSetupOrder\.indexOf\(step\) > reachableIndex\) return/);
  for (const id of ['androidCosChatgptConnect','androidCosFolderSet','androidCosFolderReset','androidCosFolderBrowse',
    'androidCosFolderNext','androidCosMainState','androidCosFolderEntries','androidCosFolderUp',
    'androidCosChatgptTunnel','androidCosReadyChecklist']) assert.match(html,new RegExp(`id="${id}"`));
  assert.match(ui, /submitOwnedBrowserChat\(/);
  assert.match(ui, /androidCosFivePermissionsConfirmed\(\)/);
  assert.match(ui, /android_main_storage_status/);
  assert.match(ui, /android_main_storage_list/);
  assert.match(ui, /android_main_storage_selection_status/);
  assert.match(ui, /android_main_storage_select/);
  assert.match(ui, /android_main_storage_reset/);
  assert.match(ui, /androidMainStorageVerified\(\)/);
  assert.match(ui, /document_tree_request/);
  assert.doesNotMatch(html.split('id="androidCosFolderPanel"')[1]?.split('id="androidCosTunnelMethod"')[0] || '',
    /id="androidCosFolderFiles"|id="androidCosFolderPicked"|Refresh folder receipt|Open Files and choose Android folder|Android workspace and folder|Browse storage|Android's folder picker/);
  assert.match(css, /#androidCosSetupStepAction\[hidden\] \{ display:none; \}/);
  assert.doesNotMatch(html.split('id="androidCosTunnelMethod"')[1]?.split('id="androidCosKeyPanel"')[0] || '',
    /id="pickBinary"|tunnel-client\.exe|startAtLogin|id="autoConnect"|minimizeToTray/);
  assert.match(html, /id="androidCosReadyChecklist"/);
  assert.match(html, /id="androidCosMcpPluginStatus"/);
  assert.doesNotMatch(html, /id="androidCosMcpPluginSkip"|Continue without remote MCP Plugin/);
  assert.doesNotMatch(html, /id="androidCosMcpPluginRefresh"/);
  assert.doesNotMatch(ui, /\$\('androidCosMcpPluginRefresh'\)/);
  assert.doesNotMatch(html, /id="androidCosMcpPluginConnect"|id="androidCosMcpPluginDisconnect"/);
});

test('All Android Setup panels keep named step navigation, status announcements and labeled fields', () => {
  const html = source('ui/index.html');
  const nav = html.match(/<nav\b[^>]*id="androidCosSetupSteps"[^>]*>[\s\S]*?<\/nav>/)?.[0] || '';
  assert.match(nav,/aria-label="[^"]+"/,
    'The ten-step navigation needs an accessible name');
  const buttons = [...nav.matchAll(/<button\b[^>]*data-cos-setup-step="([^"]+)"[^>]*>([\s\S]*?)<\/button>/g)];
  assert.equal(buttons.length,10,
    'All ten Setup stages must be real focusable buttons with readable names');
  for (const [,step,contents] of buttons) {
    assert.match(contents,/<span>[^<]+<\/span>/,step+' must have readable navigation text');
    assert.match(contents,/<small>[^<]+<\/small>/,step+' must expose its setup status');
  }
  assert.match(html,/<[^>]*id="androidCosSetupProgress"[^>]*role="progressbar"[^>]*aria-label="[^"]+"/,
    'Progress needs a named and updatable semantic progressbar');
  assert.match(html,/<[^>]*id="androidCosSetupChecking"[^>]*role="status"[^>]*aria-live="polite"/,
    'Native setup scanning must announce status changes accessibly');
  assert.match(html,/<[^>]*id="androidCosSetupCurrent"[^>]*aria-live="polite"/,
    'Active stage changes must announce the selected content');
  for (const fieldId of ['androidOpenAiTunnelId','androidTunnelKeyNeededInput','androidCosMcpPhoneName'])
    assert.match(html,new RegExp('<label\\b[^>]*for="'+fieldId+'"'),
      'Step 6, 7 and 9 text fields need visible programmatic labels: '+fieldId);
});

test('Android Workspace borrows Setup Folder and Access controls without cloning native state', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const css = source('ui/app.css');
  for (const section of ['permissions','activity','folders','health'])
    assert.match(html,new RegExp('android-workspace-'+section+'\\b'),
      'Workspace must show '+section+' section');
  for (const id of ['androidWorkspacePermissionsSlot','androidWorkspaceFoldersSlot',
    'androidWorkspaceReadOnly','androidWorkspaceChooseFolder','androidWorkspaceCopyActivity',
    'androidWorkspaceActivityFeed','androidWorkspaceActivityStatus',
    'androidWorkspaceActivityCopyStatus','androidWorkspaceRunChecks',
    'androidWorkspaceVerifiedLink','androidWorkspaceLastCall',
    'androidWorkspaceRoute','androidWorkspacePollErrors','androidWorkspaceTunnelStatus',
    'androidWorkspaceUptime','androidWorkspaceLastTool','androidWorkspaceTunnelClient',
    'androidWorkspaceLocalServer','androidWorkspacePluginStatus','androidWorkspaceToolCount',
    'androidWorkspaceChecksResult','androidWorkspaceChecksSummary',
    'androidWorkspaceChecksList','androidWorkspaceCloseChecks']) {
    assert.equal([...html.matchAll(new RegExp('id="'+id+'"','g'))].length,1,
      'Workspace must declare exactly one '+id);
  }
  for (const id of ['androidCosAccessPanel','androidCosFolderPanel',
    'androidMcpAccessPresets','androidCosSelectedPath','androidCosFolderReset'])
    assert.equal([...html.matchAll(new RegExp('id="'+id+'"','g'))].length,1,
      'Setup and Workspace must share the same control node '+id);
  assert.match(ui,/function placeAndroidWorkspaceSurfaces\(view:string\)/);
  assert.match(ui,/container\.appendChild\(node\)/);
  assert.match(ui,/container\.insertBefore\(node,anchor\.nextSibling\)/);
  assert.match(ui,/androidMainSelection/);
  assert.match(ui,/refreshAndroidMainSelection\(true\)/);
  assert.match(ui,/android_mcp_permissions_configure/);
  assert.match(ui,/androidWorkspaceReadOnly[\s\S]*?configureAndroidMcpAccess\('read_only'/);
  assert.doesNotMatch(html,/id="androidWorkspaceOpenReview"/);
  assert.match(html,/id="androidWorkspaceActivityFeed"[^>]*role="log"[^>]*aria-label="Recent local events"/);
  assert.match(html,/id="androidWorkspaceActivityCopyStatus"[^>]*role="status"[^>]*aria-live="polite"/);
  assert.match(html,/id="androidWorkspaceChecksResult"[^>]*hidden/);
  assert.match(html,/id="androidWorkspaceCloseChecks"[^>]*aria-label="Close check results"/);
  const health = html.match(/<section class="android-workspace-card android-workspace-health"[\s\S]*?<\/section>/)?.[0] || '';
  assert.equal([...health.matchAll(/class="android-workspace-health-metrics"/g)].length,1,
    'Workspace Health needs one pair of native-backed recency metrics');
  assert.equal([...health.matchAll(/class="android-workspace-health-row"/g)].length,9,
    'Workspace Health must show nine distinct diagnostic facts');
  for (const label of ['Route to OpenAI','Poll errors','Tunnel → Android',
    'Tunnel uptime','ChatGPT ran a tool','Tunnel client','Local server','Core Plugin','Core tools'])
    assert.ok(health.includes(label),'Missing Health diagnostic '+label);
  assert.match(html,/id="androidWorkspaceLegacyBindings" hidden inert aria-hidden="true"/);
  assert.match(css,/#androidWorkspaceDashboard/);
});

test('All ten Android Setup content headers have one canonical STEP NN prefix without legacy suffix labels', () => {
  const html = source('ui/index.html');
  const expected = ['PERMISSIONS','EXTENSIONS','BROWSER','FOLDER','ACCESS',
    'TUNNEL','API KEY','CONNECT','MCP PLUGIN','READY'].map((label,index) =>
      `STEP ${String(index+1).padStart(2,'0')} · ${label}`);
  // This is an inventory of *content* markers; navigation tile numbers,
  // instructional lists and hidden legacy tool controls cannot satisfy it.
  // A separate live browser assertion also verifies that one badge appears
  // above exactly one heading and description in the selected stage.
  const markers = [...html.matchAll(/<span\b[^>]*class="[^"]*\bandroid-cos-step-eyebrow\b[^"]*"[^>]*>([^<]+)<\/span>/g)]
    .map(match => match[1].trim());
  assert.equal(markers.length,10,
    'Each of the 10 content panels must declare exactly one standard step eyebrow');
  assert.deepEqual(markers.sort(),expected.sort(),
    'Every panel must begin with STEP 01 · PERMISSIONS through STEP 10 · READY, with no reversed/suffixed step number');
});

test('Every Setup entry scans native receipts and chooses first unresolved, with Ready when all steps are resolved', () => {
  const ui = source('ui/app.ts');
  const scan = ui.split('async function scanAndroidCosSetupOnOpen()')[1]
    ?.split('// The native GeckoView profile')[0] || '';
  assert.ok(scan.length > 350, 'Missing native read-only Setup scanner');
  assert.match(ui, /scanAndroidCosSetupOnOpen\(\)/,
    'Setup entry must invoke a native scan instead of restoring a stale selected tab');
  assert.match(scan, /Promise\.allSettled\(/,
    'Unavailable native statuses must not prevent checking other stages');
  for (const fn of [
    'refreshStorageAccess','refreshAndroidAccessibility','refreshAndroidOptionalPermissions',
    'refreshAndroidMainStorageStatus','refreshAndroidCosFolderGrant',
    'refreshAndroidCosExtensions','refreshAndroidCosOAuthStatus','refreshAndroidMcpAccess',
    'refreshAndroidTunnelSavedId','refreshAndroidTunnelCredentials',
    'refreshAndroidOpenAiTunnel','refreshAndroidTunnelStoredAccess'
  ]) assert.match(scan, new RegExp('\\b' + fn + '\\('),
    'Opening Setup must check ' + fn + ' without entering its tab');
  assert.match(scan, /androidCosFirstUnresolvedStep\(freshResults\)/,
    'The initial scan and later auto-follow must share one first-unresolved selector');
  assert.match(ui, /!result\.verified && !result\.notNeeded/,
    'Pending, Checking and Unable to check count as unresolved; Not needed does not');
  assert.match(ui, /return index < 0 \? 'ready' : androidCosSetupOrder\[index\]/,
    'When all earlier steps are Verified or Not needed, jump to Step 10');
  assert.match(scan, /androidCosSetupUserSelectedDuringScan/,
    'Manual tab choice during a scan must not be overwritten');
  assert.doesNotMatch(scan, /android_openai_tunnel_connect_stored|android_permissions_request|android_chatgpt_oauth_start/,
    'Setup scans must be read-only: no consent prompts or new connections');
});

test('Completed Android Setup reopens without a blocking scanner but fresh OS receipts still revoke stale Verified state', () => {
  const ui = source('ui/app.ts');
  const render = ui.split('function renderAndroidCosSetup(): void')[1]
    ?.split('function chooseAndroidCosSetupStep(')[0] || '';
  const scan = ui.split('async function scanAndroidCosSetupOnOpen()')[1]
    ?.split('// The native GeckoView profile')[0] || '';
  const completion = ui.split('function androidCosAllStagesComplete(')[1]
    ?.split('function renderAndroidCosSetup(): void')[0] || '';
  assert.ok(scan.length > 800 && render.length > 800 && completion.length > 100,
    'Setup requires a full native refresh and an explicit all-stages-complete policy');
  assert.match(ui, /androidCosAllStagesComplete\(results:readonly AndroidCosSetupResult\[\]\)/,
    'A complete state must derive from all ten native stage results');
  assert.match(completion, /results\.every\(result => result\.verified \|\| result\.notNeeded\)/);
  assert.match(completion, /results\[results\.length - 1\]\.verified/,
    'Ready itself must be verified, not only the earlier stages');
  assert.match(scan, /const currentlyComplete = androidCosAllStagesComplete\(nativeResults\)/);
  assert.match(scan, /androidCosSetupHeldVerifiedResults = currentlyComplete \? nativeResults : null/,
    'Previous Verified badges may be held only if live in-memory receipts were fully verified');
  assert.match(scan, /androidCosSetupQuietScan = currentlyComplete \|\| androidCosSetupPreviouslyComplete/,
    'A completed setup can suppress a purely cosmetic Checking overlay on reentry');
  assert.match(render, /!androidCosSetupQuietScan && !androidCosSetupUserSelectedDuringScan/,
    'Reentry must hide the blocking Checking overlay without suppressing manual tab choice');
  assert.match(render, /androidCosSetupHeldVerifiedResults[\s\S]*?\? androidCosSetupHeldVerifiedResults : liveResults/,
    'Only known native Verified receipts may be presented during a background refresh');
  assert.match(render,
    /!androidCosSetupScanInFlight && !androidCosSetupPreviouslyComplete &&\s+androidCosAllStagesComplete\(liveResults\)\)\s+rememberAndroidCosSetupCompletion\(true\)/,
    'Late native completion after a scan must also save the cosmetic reentry hint');
  assert.match(scan, /Promise\.allSettled\(/);
  assert.match(scan, /const freshResults = androidCosSetupOrder\.map\(step => androidCosSetupResult\(step\)\)/);
  assert.match(scan, /rememberAndroidCosSetupCompletion\(androidCosAllStagesComplete\(freshResults\)\)/,
    'Revoked native receipts must clear the stored cosmetic completed hint');
  assert.match(scan, /androidCosSetupHeldVerifiedResults = null/);
  assert.match(scan, /androidCosFirstUnresolvedStep\(freshResults\)/,
    'After refresh the first unresolved native step wins, not a cached Ready stage');
  assert.match(scan, /!androidCosSetupUserSelectedDuringScan/,
    'Manual navigation during the background scan must take precedence');
  assert.match(completion, /localStorage\.setItem\(androidCosSetupCompletedHintKey,'1'\)/);
  assert.match(completion, /localStorage\.removeItem\(androidCosSetupCompletedHintKey\)/);
  assert.doesNotMatch(completion, /JSON\.stringify\(.*(?:results|nativeResults|freshResults|permissions|token)/,
    'The persisted hint must never serialize native receipts, privileges or tokens');
  assert.doesNotMatch(scan, /android_permissions_request|android_chatgpt_oauth_start|android_openai_tunnel_connect_stored/,
    'Silent rescans must never request consent or launch a new connection');
});

test('Android Setup auto-follows native tunnel verification without stealing user-selected tabs or trusting local counters', () => {
  const ui = source('ui/app.ts');
  assert.match(ui, /androidCosSetupAutoFollow/,
    'A separate automatic-follow intent must survive Step8 connecting after Setup scan');
  assert.match(ui, /function renderAndroidCosSetup\(\)/,
    'Stage transitions must be derived from the current native Setup result');
  assert.match(ui, /function chooseAndroidCosSetupStep\(step: AndroidCosSetupStep\)/,
    'Explicit step selection must be distinguishable from automatic navigation');
  assert.match(ui, /androidCosSetupUserSelectedDuringScan/,
    'Manual selections during Setup scan must still win');
  assert.match(ui, /function androidOpenAiTunnelVerified\(\)/);
  assert.match(ui, /androidOpenAiTunnelSnapshot\?\.state === 'tool_verified'/);
  assert.match(ui, /androidOpenAiTunnelSnapshot\.pollActive === true/);
  assert.match(ui, /androidOpenAiTunnelSnapshot\.toolCallsVerified > 0/,
    'Current-session tool verification requires a native acknowledged response');
  assert.match(ui, /function androidCorePluginVerified\(\)/);
  assert.match(ui, /corePluginLinked === true/);
  assert.match(ui, /\['polling','tool_verified'\]\.includes\(tunnel\.state\)/);
  assert.match(ui, /tunnel\.pollActive === true/,
    'Historical Core pairing needs a live healthy tunnel, not the saved ID alone');
  assert.match(ui, /android_openai_tunnel_status/,
    'Auto-follow observes backend tunnel receipt, never invents local success');
  assert.doesNotMatch(ui, /\$\('androidCosMcpPluginRefresh'\)/,
    'Step9 must not regain its removed manual Refresh action');
});

test('Step7 authorizes Secure MCP key; Step8 connects native tunnel without any ChatGPT login', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const keyPanel = html.split('id="androidCosKeyPanel"')[1]?.split('id="androidCosConnectPanel"')[0] || '';
  const connectPanel = html.split('id="androidCosConnectPanel"')[1]?.split('id="androidCosMcpPluginPanel"')[0] || '';
  assert.match(keyPanel, /Create an API key/i);
  assert.match(keyPanel, /id="androidTunnelKeyNeededOpenApiKeys"/);
  const guide = keyPanel.match(/<ol\b[^>]*class="android-tunnel-step7-steps"[\s\S]*?<\/ol>/)?.[0] || '';
  assert.equal((guide.match(/<li\b/g) || []).length, 3, 'Step7 should have exactly three instructions');
  assert.match(keyPanel, /id="androidTunnelKeyNeededInput"[^>]*type="password"/);
  for (const id of ['androidTunnelKeyNeededConfirm','androidTunnelRemoveStoredKey','androidTunnelKeyNeededStatus','androidTunnelStep7Status'])
    assert.match(keyPanel, new RegExp(`id="${id}"`), id);
  assert.doesNotMatch(keyPanel, /id="androidTunnelVerifyCore"|id="androidTunnelKeyNeededCancel"|id="androidCosProvider"|id="androidCosModel"|id="androidCosApiKey"|id="androidCosUseKey"|id="androidCosClearKey"/);
  for (const id of ['androidCosChatgptConnect','androidCosTunnelDisconnect','androidCosConnectStatus'])
    assert.match(connectPanel, new RegExp(`id="${id}"`), `Step8 must own ${id}`);
  assert.doesNotMatch(connectPanel,
    /id="androidCosChatgptOpen"|id="androidCosChatgptBring"|id="androidCosChatgptCheck"|id="androidCosPrepareTest"|id="androidCosProvider"|id="androidCosModel"|id="androidCosApiKey"|id="androidCosUseKey"|id="androidCosClearKey"|id="androidCosProviderSettings"|Ollama|Claude|OpenRouter/);
  assert.equal((connectPanel.match(/<button\b[^>]*>Connect<\/button>/g) || []).length, 1,
    'Step8 must contain exactly one primary Tunnel Connect action');
  assert.match(connectPanel, /Core Tunnel ID|Secure MCP Tunnel/i);
  const tunnelConnect = ui.slice(ui.indexOf("$('androidCosChatgptConnect').addEventListener"),
    ui.indexOf("$('androidCosTunnelDisconnect').addEventListener"));
  assert.ok(tunnelConnect.length > 150);
  assert.match(tunnelConnect, /android_openai_tunnel_connect_stored/);
  assert.doesNotMatch(tunnelConnect,
    /android_chatgpt_(oauth_start|oauth_bring|open|bring_session|check_session)|chatgpt_browser_send/,
    'Step8 tunnel Connect must neither repeat Step3 OAuth approval nor send a website chat message');
  assert.match(ui, /chatgpt_browser_finalize/);
  assert.match(ui, /verifiedAiConnection = \{provider:'chatgpt-browser',model:'chatgpt.com'\}/);
  assert.match(ui, /android_tunnel_credentials_store/);
  assert.match(ui, /android_tunnel_credentials_remove/);
  assert.match(ui, /verifyAndroidSavedCoreTunnel\(/);
  assert.match(ui, /case 'connect': \{/);
  assert.match(ui, /\['polling','tool_verified'\]\.includes\(tunnel\.state\)/);
});

test('Step9 recognizes current acknowledged tools/call or previously linked native Core while healthy; no duplicate Tunnel Connect', () => {
  const ui = source('ui/app.ts');
  const html = source('ui/index.html');
  const step9 = html.split('id="androidCosMcpPluginPanel"')[1]?.split('id="androidCosReadyPanel"')[0] || '';
  for (const id of [
    'androidCosMcpPluginStatus','androidCosMcpPluginEvidence',
    'androidCosMcpPhoneName','androidCosMcpName',
    'androidCosMcpDescription','androidCosMcpCopyName','androidCosMcpCopyDescription',
    'androidCosMcpPluginOpen','androidCosMcpPluginCopyStatus'
  ]) assert.match(step9, new RegExp(`id="${id}"`), `Step9 requires ${id}`);
  assert.doesNotMatch(step9,/id="androidCosMcpPluginRefresh"|Refresh status/i,
    'Step9 must not require a manual Refresh button');
  assert.doesNotMatch(step9, /id="androidCosMcpPluginConnect"|id="androidCosMcpPluginDisconnect"/);
  assert.doesNotMatch(step9, /admin\s+(help|workspace)|desktop\s+plugin/i,
    'Android Step9 must use its own Core Plugin only');
  assert.match(step9, /actual|approved MCP tool|tool call/i);
  assert.match(ui, /androidOpenAiTunnelSnapshot\?\.state === 'tool_verified'/);
  assert.match(ui, /androidOpenAiTunnelSnapshot\.pollActive === true/);
  assert.match(ui, /androidOpenAiTunnelSnapshot\.toolCallsVerified > 0/);
  assert.match(ui, /function androidCorePluginVerified\(\)/);
  assert.match(ui, /corePluginLinked === true/);
  assert.match(ui, /androidCorePluginVerified\(\)/);
  assert.match(ui, /Core Plugin linked previously/);
  assert.match(ui, /No new tool call has been claimed for this session/,
    'Historical pairing must not be presented as a fresh accepted call after restart');
  assert.match(ui, /typeof report\.corePluginLinked !== 'boolean'/,
    'Malformed native historical pairing evidence must fail closed');
  assert.doesNotMatch(ui, /androidOpenAiTunnelSkipped|androidCosMcpPluginSkip/,
    'Step 9 must stay pending until real Core Plugin verification');
  assert.match(ui, /case 'mcpPlugin':/);
  assert.match(ui, /androidCosSetupResult\('mcpPlugin'\)/);
  assert.match(ui, /android_openai_tunnel_status/);
  assert.match(ui, /android_openai_tunnel_connect_stored/);
  assert.match(ui, /android_openai_tunnel_disconnect/);
  assert.doesNotMatch(step9, /tunnel-client\.exe|startAtLogin|autoConnect|minimizeToTray/);
});

test('Step9 Core Plugin deep link is native-allowlisted, explicit, and copyable with an editable phone name', () => {
  const ui = source('ui/app.ts');
  const kotlin = source('android/TunnelCredentialPlugin.kt');
  const rust = source('src-tauri/src/android_tunnel_credentials.rs');
  const html = source('ui/index.html');
  const step9 = html.split('id="androidCosMcpPluginPanel"')[1]?.split('id="androidCosReadyPanel"')[0] || '';
  assert.match(step9, /Core Plugin/i);
  assert.match(step9, /Name for this core/);
  assert.match(ui, /label \? `Mcp Agent Ai Core \(\$\{label\}\)` : 'Mcp Agent Ai Core'/);
  assert.match(ui, /Mcp Agent Ai Core/);
  assert.match(ui, /androidCosMcpPhoneName/);
  assert.match(ui, /androidCosMcpCopyName/);
  assert.match(ui, /androidCosMcpCopyDescription/);
  assert.match(ui, /androidCosMcpPluginOpen/);
  assert.match(ui, /android_tunnel_open_platform_page/);
  assert.match(ui, /kind:'chatgpt_plugins'|kind: 'chatgpt_plugins'/,
    'The browser command must be called for the ChatGPT Plugins destination');
  assert.match(ui, /userInitiated:true|userInitiated: true/);
  assert.match(rust, /"chatgpt_plugins"/);
  assert.match(kotlin, /"chatgpt_plugins"\s*->/);
  assert.match(kotlin, /CustomTabsIntent\.Builder\(\)\.build\(\)\.launchUrl\(activity, Uri\.parse\(url\)\)/);
  assert.match(kotlin, /if \(!args\.userInitiated\)/);
  assert.doesNotMatch(step9, /id=".*(?:Desktop|Admin).*Plugin/i);
});

test('Android chat-first automation keeps ChatGPT text and API model configuration without a per-message tool checkbox', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  assert.match(html, /<option value="chatgpt-browser">ChatGPT built-in browser/);
  assert.doesNotMatch(ui, /document\.querySelector\('#provider option\[value="chatgpt-browser"\]'\)\?\.remove\(\)/);
  assert.match(ui, /chatgpt-browser/);
  assert.match(ui, /submitOwnedBrowserChat\(prompt\)/);
  assert.match(ui, /verifiedAiConnection = \{provider:'chatgpt-browser',model:'chatgpt.com'\}/);
  assert.match(ui, /chatgpt_browser_finalize/);
  assert.match(ui, /chatgpt_browser_observe/);
  assert.match(ui, /\['chatgpt-browser','openai','anthropic','openrouter'\]\.includes\(option\.value\)/);
  assert.match(ui, /state\.providers\.filter\(provider =>/);
  assert.doesNotMatch(ui, /state\.hasKeys = \[\]/);
  for (const id of ['androidAgentProvider','androidAgentModel','androidAgentApiKey',
    'androidAgentSaveKey','androidAgentClearKey'])
    assert.match(html,new RegExp('id="'+id+'"'));
  assert.match(ui, /allowMcpTools, allowLocalTools/);
  assert.doesNotMatch(html,/id="androidLocalToolsOptIn"/);
  assert.match(ui, /const allowLocalTools = nativeAndroidRuntime && isTauri\(\) && !allowMcpTools/);
  assert.match(ui, /submitDirectAndroidChat\(prompt,directTask\)/);
  assert.match(ui, /api\('\/api\/chat-local-action'/);
  assert.match(ui, /androidAgentProviderModels/);
  assert.match(source('src-tauri/src/lib.rs'),/android_agent_tools::register\(&engine, app\.handle\(\)\.clone\(\)\)/);
  const historySelect = ui.slice(ui.indexOf('selectedSession = session.id;'),
    ui.indexOf('displayMessages(session);', ui.indexOf('selectedSession = session.id;')));
  assert.doesNotMatch(historySelect, /state\.providers\.find\([^)]*provider !== 'chatgpt-browser'/,
    'Reopening native ChatGPT history must retain the selected provider');
  assert.match(historySelect, /chatgpt-browser/,
    'Reopening native ChatGPT history must retain the private website provider');
  const nativeCore = source('native-core/src/lib.rs');
  assert.match(nativeCore, /chatgpt-browser/);
  assert.match(html, /id="androidCosTunnelMethod"/);
  assert.match(html, /id="backgroundChatgptSettings"/);
});

test('Android Settings retires both legacy AI Connection and ChatGPT built-in browser cards, not just provider option', () => {
  const css = source('ui/app.css');
  const ui = source('ui/app.ts');
  for (const id of ['mobileAiDelegates','mobileGoalDraftControls','mobileGoalRunAi',
    'mobileGoalStopAi','mobileGoalDelegateControls','mobileGoalMcpControls','mobileGoalMcpReview'])
    assert.match(css,new RegExp('#'+id+'[,\\s]'),
      'ChatGPT-only Android must retire direct AI delegate and Goal controls: '+id);
  assert.match(css, /html\.native-android #androidSettingsContent > #connectionSection/);
  assert.match(css, /html\.native-android #androidSettingsContent > #backgroundChatgptSettings/);
  assert.match(css, /#backgroundChatgptSettings\.android-settings-active \{\s*display: none !important/);
  assert.match(ui, /for \(const id of \['connectionSection', 'backgroundChatgptSettings'\]\)/);
  assert.match(ui, /section\.hidden = true/);
  assert.match(ui, /section\.inert = true/);
  assert.match(ui, /section\.setAttribute\('aria-hidden', 'true'\)/);
  assert.match(ui, /!\['connectionSection','backgroundChatgptSettings'\]\.includes\(group\.id\)/);
  assert.match(source('ui/index.html'), /id="androidCosTunnelMethod"/);
  assert.match(source('ui/index.html'), /id="accessibilityManage"/);
});

test('Android Setup isolates five native App permissions in step 1, hides duplicate status/navigation, and auto-advances only on five OS receipts', () => {
  const html = source('ui/index.html');
  const css = source('ui/app.css');
  const app = source('ui/app.ts');
  assert.equal((html.match(/data-permission-row="/g) || []).length, 5);
  assert.match(html, /id="androidCosSetupCurrent"/);
  for (const id of ['androidCosSetupPrev','androidCosSetupNext','androidCosSetupRefresh','androidCosSetupSummary']) {
    assert.doesNotMatch(html, new RegExp(`id="${id}"`));
    assert.doesNotMatch(app, new RegExp(`\\$\\('${id}'\\)`));
  }
  assert.match(css, /#storageAccessSection\.android-settings-active:not\(\.android-permissions-step\) \{\s*display:none !important/);
  assert.match(css, /#androidCosSetupCurrent\.hidden \{ display:none; \}/);
  assert.match(app, /permissionSection\.classList\.toggle\('android-permissions-step', permissionsSelected\)/);
  assert.match(app, /permissionSection\.inert = !permissionsSelected/);
  assert.match(app, /permissionSection\.setAttribute\('aria-hidden',String\(!permissionsSelected\)\)/);
  assert.match(app, /access\.total === 5 && access\.allowed === 5/);
  assert.match(app, /!androidPermissionRefreshPending && !androidAccessibilityRefreshPending/);
  assert.match(app, /void scanAndroidCosSetupOnOpen\(\)/,
    'When Step1 OS grants become complete, scan all stages instead of blindly opening Step2');
  assert.doesNotMatch(app, /androidCosSetupSelected = 'extension'/,
    'Step1 auto-advance must not overwrite the scanner with a hardcoded Step2');
  assert.match(html, /id="androidCosExtensionControls"/);
  assert.match(html, /id="androidCosTunnelMethod"/);
  assert.match(html, /id="accessibilityManage"/);
});

test('Step 1 optional camera, microphone and coarse location request native OS consent without blocking five required grants', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const lib = source('src-tauri/src/lib.rs');
  const section = html.split('id="androidOptionalPermissionsSection"')[1]?.split('</details>')[0] || '';
  assert.ok(section.length > 100, 'Optional permissions must appear together within Step 1');
  assert.match(html, /<details id="androidOptionalPermissionsSection"[^>]*>/,
    'Optional permission list must use native accessible details disclosure');
  assert.match(section, /<summary\b[^>]*>/);
  assert.doesNotMatch(html, /<details id="androidOptionalPermissionsSection"[^>]*\bopen(?:\s|=|>)/,
    'Optional permissions must start folded');
  for (const [key, stateId] of [
    ['camera','androidOptionalCameraState'],
    ['microphone','androidOptionalMicrophoneState'],
    ['location','androidOptionalLocationState'],
  ]) {
    assert.equal((section.match(new RegExp('data-optional-permission="' + key + '"','g')) || []).length, 1,
      key + ' must appear as exactly one optional permission row');
    assert.equal((section.match(new RegExp('id="' + stateId + '"','g')) || []).length, 1,
      key + ' must have one native status indicator');
    assert.equal((section.match(new RegExp('data-optional-permission-request="' + key + '"','g')) || []).length, 1,
      key + ' must expose an individual Allow action');
  }
  assert.match(section, /optional/i);
  assert.match(ui, /android_optional_permissions_status/);
  assert.match(ui, /android_optional_permissions_request/);
  assert.match(ui, /userInitiated\s*:\s*true/,
    'Each optional request must carry explicit user consent');
  assert.match(lib, /android_optional_permissions_status/);
  assert.match(lib, /android_optional_permissions_request/);
  const requiredGate = ui.slice(ui.indexOf('function androidCosFivePermissionsConfirmed()'),
    ui.indexOf('function androidOwnedChatgptLinked()'));
  assert.ok(requiredGate.length > 150, 'Five-required-permission gate must remain distinct');
  assert.match(requiredGate, /access\.total === 5 && access\.allowed === 5/);
  assert.doesNotMatch(requiredGate, /optional|camera|microphone|location/i,
    'Optional permissions must never be part of Step 1 automatic progression gate');
  assert.equal((html.match(/data-permission-row="/g) || []).length, 5,
    'Five required rows remain distinct from three optional rows');
});

test('Step 2 shows polished full-text extension actions with read-only native installed-state checks', () => {
  const html = source('ui/index.html');
  const css = source('ui/app.css');
  const app = source('ui/app.ts');
  const rust = source('src-tauri/src/lib.rs');
  const gecko = source('android/CustomGeckoBrowserPlugin.kt');
  const patcher = source('scripts/patch-tauri-android.mjs');
  const extensionSection = html.split('id="androidCosExtensionControls"')[1]?.split('id="androidCosBrowserChoices"')[0] || '';
  for (const id of ['androidCosExtensionOpen','androidCosExtensionInstall']) {
    assert.equal((html.match(new RegExp('id="'+id+'"', 'g')) || []).length,1,id);
  }
  assert.equal((extensionSection.match(/<button\b/g) || []).length,2,
    'Only two direct Step 2 action buttons are visible');
  assert.match(extensionSection, /<strong>Open Extensions<\/strong>/);
  assert.match(extensionSection, /<strong>Install Extension<\/strong>/);
  assert.match(extensionSection, /android-cos-extension-action-icon/);
  assert.match(extensionSection, /Manage your extensions/);
  assert.doesNotMatch(extensionSection, /\bOpen Exten<\/|\bInstall Exten<\//);
  assert.match(html, /id="androidCosExtensionManager"/);
  assert.match(html, /id="androidCosExtensionList"/);
  assert.match(html, /id="androidCosExtensionSearch"/);
  assert.match(html, /id="androidCosExtensionRefresh"/);
  assert.match(html, /class="android-cos-extension-page-toolbar"/);
  assert.match(html, /GECKOVIEW EXTENSION MANAGER/);
  assert.match(css, /html\.native-android \.android-cos-extension-manager \{\s*box-sizing:border-box; position:fixed; inset:0;/);
  assert.match(css, /width:100vw; max-width:none; height:100dvh; max-height:none/);
  assert.match(app, /androidCosExtensionSearch'\)\.addEventListener\('input',renderAndroidCosExtensionManager\)/);
  assert.match(app, /custom_gecko_list_extensions/);
  assert.doesNotMatch(app, /androidCosExtensionOpen'\)\.addEventListener\([^]*?invoke\(['"]custom_gecko_open['"]/);
  assert.doesNotMatch(html, /id="androidCosExtensionGuide"|id="androidCosExtensionAlternate"|id="customBrowserSettings"/);
  assert.doesNotMatch(html, /Custom browser &amp; extensions|Phase A: desktop COS/);
  assert.match(app, /custom_gecko_list_extensions/);
  assert.match(app, /custom_gecko_install_probe/);
  assert.match(app, /custom_gecko_manage/);
  assert.match(app, /validateAndroidCosExtensions\(response\)/);
  assert.match(app, /androidCosExtensionBusy/);
  assert.match(app, /receipt\.runtimeStarted !== true/);
  assert.doesNotMatch(app, /androidCosExtensionAlternate|import '.\/custom-browser-ui'/);
  assert.match(css, /\.android-cos-extension-actions \{/);
  for (const id of ['customBrowserUrl','customGeckoXpiUrl','customGeckoInstallSigned']) {
    assert.doesNotMatch(html, new RegExp('id="'+id+'"'), id+' must not be rendered');
  }
  assert.doesNotMatch(html, /id="customBrowserWindowsOpen"|class="custom-browser-windows"/);
  assert.doesNotMatch(app, /custom_gecko_install_signed|installSigned|customGeckoXpiUrl/);
  assert.doesNotMatch(rust, /custom_gecko_install_signed/);
  assert.match(app, /userInitiated:true/);
  assert.doesNotMatch(rust, /mod custom_browser_windows|mod windows_background_browser|custom_browser_windows_status|windows_chatgpt_status/);
  assert.match(gecko, /GeckoRuntime\.create\(activity\)/);
  assert.match(gecko, /val r = try \{ runtime\(\) \}/,
    'Native status check must reopen persisted GeckoView runtime without starting tabs');
  assert.match(gecko, /WebExtensionController\.PromptDelegate/);
  assert.match(gecko, /ensureBuiltIn\(demoExtensionUri, demoExtensionId\)/);
  assert.match(gecko, /addActionRow\(root,/);
  assert.match(gecko, /newButton\("Done"\) \{ dialog\?\.dismiss\(\) \}/);
  assert.match(gecko, /Close GeckoView before reviewing or installing an extension/);
  assert.match(gecko, /setTextColor\(Color\.rgb\(235, 241, 250\)\)/);
  assert.doesNotMatch(gecko, /fun installSigned|validSignedXpi|webExtensionController\.install\(/);
  assert.doesNotMatch(gecko, /chrome\.runtime|chrome\.debugger/);
  assert.match(patcher, /CustomGeckoBrowserPlugin\.kt/);
  assert.match(patcher, /geckoview:geckoview/);
  assert.match(rust, /android_gecko_browser::init\(\)/);
  assert.match(rust, /custom_gecko_manage/);
  const probe = JSON.parse(source('android/mcp_extension_probe/manifest.json'));
  assert.equal(probe.permissions.length,0);
  assert.equal(probe.browser_specific_settings.gecko.id,'mcp-agent-browser-probe@example.com');
});

test('Android General offers device-local COS preferences with a strict backup and existing native destinations', () => {
  const html = source('ui/index.html');
  const app = source('ui/app.ts');
  const css = source('ui/app.css');
  const general = html.split('id="androidGeneralSettings"')[1]?.split('id="androidWorkspaceDashboard"')[0] || '';
  assert.ok(general.length > 2000, 'Full General panel must exist before Workspace');
  const headings = ['Chats','Notifications','Privacy','Backup','For developers'];
  assert.deepEqual([...general.matchAll(/<h2 id="androidGeneral\w+Title">([^<]+)<\/h2>/g)]
    .map(match => match[1]),headings,'General must contain exactly the five real Android groups');
  for (const id of ['androidGeneralSettings','androidGeneralFollowOutput','androidGeneralNotifyFinish',
    'androidGeneralNotificationDetail','androidGeneralNotificationPriority','androidGeneralMessageDuringTurn','androidGeneralOpenPermissions','androidGeneralOpenWorkspace',
    'androidGeneralExport','androidGeneralImport','androidGeneralBackupText',
    'androidGeneralBackupPreview','androidGeneralBackupApply','androidGeneralBackupStatus',
    'androidGeneralOpenHealth','androidGeneralOpenReview'])
    assert.equal((html.match(new RegExp('id="'+id+'"','g')) || []).length,1,
      id+' must have exactly one DOM identity');
  assert.match(general,/id="androidGeneralFollowOutput" type="checkbox" checked/);
  assert.match(general,/id="androidGeneralNotifyFinish" type="checkbox"/);
  assert.match(general,/option value="task_finished"/);
  assert.match(general,/option value="full_message"/);
  for (const priority of ['foreground','background','both'])
    assert.match(general,new RegExp('option value="'+priority+'"'));
  const notificationGroup = general.slice(general.indexOf('id="androidGeneralNotificationsTitle"'),
    general.indexOf('id="androidGeneralPrivacyTitle"'));
  assert.match(notificationGroup,/id="androidGeneralNotificationPriority"/);
  assert.match(general,/option value="steer"/);
  assert.match(general,/option value="queue"/);
  assert.doesNotMatch(general,/androidGeneralSendShortcut|androidGeneralSoundCues|Send messages with|Play sounds/);
  assert.match(general,/id="androidGeneralBackupApply"[^>]*disabled/);
  assert.match(general,/id="androidGeneralBackupStatus"[^>]*role="status" aria-live="polite"/);
  assert.doesNotMatch(general,/Background Session|Acquire wakelock|Release wakelock|androidForegroundRuntime/);

  assert.match(app,/const androidGeneralStorageKey = 'mcp-agent-ai-android-general-v1'/);
  assert.match(app,/followOutput:true, notifyOnFinish:false/);
  assert.match(app,/notificationDetail:'task_finished', notificationPriority:'both', messageDuringTurn:'queue'/);
  const validator = app.slice(app.indexOf('function parseAndroidGeneralPreferences('),
    app.indexOf('function loadAndroidGeneralPreferences('));
  assert.match(validator,/data\.version !== 2 && data\.version !== 3/);
  assert.match(validator,/typeof data\.followOutput !== 'boolean'/);
  assert.match(validator,/typeof data\.notifyOnFinish !== 'boolean'/);
  assert.match(validator,/data\.notificationDetail !== 'task_finished'/);
  assert.match(validator,/data\.messageDuringTurn !== 'steer'/);
  assert.match(validator,/Object\.keys\(data\)\.some/);
  assert.match(validator,/data\.version === 3 \? \['notificationPriority'\] : \[\]/);
  assert.match(validator,/data\.version === 2 \? 'both' : data\.notificationPriority/);
  assert.match(validator,/notificationPriority !== 'foreground'/);
  assert.match(validator,/data\.version === 1/);
  assert.match(validator,/return \{\.\.\.androidGeneralDefaults,followOutput:data\.followOutput\}/);
  assert.doesNotMatch(validator,/apiKey|accessToken|secret|sessionId|folderGrant/);
  const backup = app.slice(app.indexOf('async function copyAndroidGeneralBackup('),
    app.indexOf('function bindAndroidGeneralSettings('));
  assert.match(backup,/navigator\.clipboard\?\.writeText/);
  assert.match(backup,/Clipboard unavailable/);
  assert.match(backup,/androidGeneralImportPending = \{source:value,preferences:settings\}/);
  assert.match(backup,/apply\.disabled = false/);
  assert.match(backup,/Invalid backup/);
  assert.match(app,/if \(!pending \|\| current !== pending\.source\)/,
    'Backup text must be unchanged since preview before Apply');
  assert.match(app,/androidGeneralImportPending = null/);
  assert.match(app,/saveAndroidGeneralPreferences\(pending\.preferences\)/);
  assert.match(app,/setAndroidSettingsView\('setup'\);\s*chooseAndroidCosSetupStep\('access'\)/);
  assert.match(app,/setAndroidSettingsView\('workspace'\)/);
  assert.match(app,/\$\('androidWorkspaceHealthTitle'\)/);
  assert.match(app,/openToolPanel\('review'\)/);
  assert.match(app,/\$\('androidGeneralNotificationDetail'\)[^\n]*\.disabled\s*=\s*\n?\s*!androidGeneralPreferences\.notifyOnFinish/);
  assert.match(app,/\$\('androidGeneralNotificationPriority'\)[^\n]*\.disabled\s*=\s*\n?\s*!androidGeneralPreferences\.notifyOnFinish/);
  assert.match(app,/!event\.shiftKey && !event\.isComposing && !androidNewline/);
  assert.match(app,/return !nativeAndroidRuntime \|\| androidGeneralPreferences\.followOutput/);
  assert.match(app,/androidGeneralProgrammaticScroll/);
  assert.match(app,/pane\.scrollTop = pane\.scrollHeight/);
  assert.match(app,/android_turn_notification/);
  assert.match(app,/androidTurnNotificationClaims\.has\(requestId\)/);
  assert.match(app,/message:mode === 'full_message' \? answer : ''/);
  assert.match(app,/notificationPriority:androidGeneralPreferences\.notificationPriority/);
  assert.match(app,/userInitiated:true/);
  assert.match(app,/void sendAndroidFinishedNotification\(result\.answer,requestId\)/);
  assert.match(app,/void sendAndroidFinishedNotification\(result\.answer,activeChatRequestId\)/);
  assert.match(app,/function acceptAndroidDuringTurn\(/);
  assert.match(app,/function finishAndroidDuringTurn\(/);
  assert.match(app,/if \(!sameConversation \|\| !verified \|\| \$\('prompt'\)\.value\.trim\(\)\)/);
  assert.match(app,/if \(!working && \$\('prompt'\)\.value\.trim\(\) === queued\.prompt/);
  assert.match(app,/if \(working && nativeAndroidRuntime && !pendingModelToolApproval\)/);
  assert.match(app,/\$\('prompt'\)\.readOnly = busy && \(!nativeAndroidRuntime \|\| !!androidDuringTurnPending\)/);
  assert.match(app,/\$\('androidDuringTurnCancel'\)\.addEventListener\('click'/);
  assert.doesNotMatch(app,/playAndroidGeneralReplySound|androidGeneralPreferences\.soundCues/);
  assert.match(css,/html\.native-android #androidGeneralSettings\.android-settings-active/);
  assert.match(css,/@media\(max-width:390px\)/);
  assert.match(css,/#androidGeneralSettings \.android-general-import textarea/);
});

test('Android completed-turn pop-ups migrate from the old quiet channel without bypassing disabled alerts', () => {
  const kotlin = source('android/TurnCompletionPlugin.kt');
  const html = source('ui/index.html');
  const patcher = source('scripts/patch-tauri-android.mjs');
  assert.match(kotlin,/LEGACY_CHANNEL_ID = "mcp_agent_ai_turn_completions_v1"/);
  assert.match(kotlin,/CHANNEL_ID = "mcp_agent_ai_turn_completions_popup_v2"/);
  assert.match(kotlin,/getNotificationChannel\(LEGACY_CHANNEL_ID\)\?\.importance ==\s*NotificationManager\.IMPORTANCE_NONE/);
  assert.match(kotlin,/NotificationManager\.IMPORTANCE_HIGH/);
  assert.match(kotlin,/getNotificationChannel\(CHANNEL_ID\)\?\.importance ==\s*NotificationManager\.IMPORTANCE_NONE/);
  assert.match(kotlin,/\.setPriority\(Notification\.PRIORITY_HIGH\)/);
  assert.match(kotlin,/\.setAutoCancel\(true\)/);
  assert.doesNotMatch(kotlin,/setFullScreenIntent|FLAG_ONGOING_EVENT|setOngoing\(true\)/);
  assert.match(patcher,/'TurnCompletionPlugin\.kt'/);
  assert.match(html,/Request a brief pop-up when a reply finishes/);
});

test('Android native notification filters delivery using Activity lifecycle, never foreground service state', () => {
  const kotlin = source('android/TurnCompletionPlugin.kt');
  const rust = source('src-tauri/src/android_turn_notifications.rs');
  assert.match(kotlin,/Application\.ActivityLifecycleCallbacks/);
  assert.match(kotlin,/onActivityResumed\(a: Activity\)[\s\S]*?activityResumed = true/);
  assert.match(kotlin,/onActivityPaused\(a: Activity\)[\s\S]*?activityResumed = false/);
  assert.match(kotlin,/registerActivityLifecycleCallbacks\(lifecycle\)/);
  assert.match(kotlin,/unregisterActivityLifecycleCallbacks\(this\)/);
  assert.match(kotlin,/activity\.window\?\.decorView\?\.hasWindowFocus\(\) == true/,
    'A plugin initialized after onResume must still classify the visible Activity correctly');
  assert.match(kotlin,/args\.notificationPriority == "foreground" && appForeground/);
  assert.match(kotlin,/args\.notificationPriority == "background" && !appForeground/);
  assert.match(kotlin,/args\.notificationPriority == "both"/);
  assert.match(kotlin,/receipt\(false, "priority_filtered"\)/);
  assert.match(rust,/"notificationPriority": notification_priority/);
  assert.match(rust,/"invalid_priority"/);
  assert.match(rust,/"priority_filtered"/);
});

test('Android automation preferences live once in Setup Access Fine-tune, never as Settings Agents launches or legacy controls', () => {
  const html = source('ui/index.html');
  const ui = source('ui/app.ts');
  const rust = source('src-tauri/src/lib.rs');
  const kotlin = source('android/BackgroundBrowserAutomationPlugin.kt');
  const indicator = source('android/AgentAccessibilityService.kt');
  const patcher = source('scripts/patch-tauri-android.mjs');
  for (const id of ['androidAutomationEngines','androidAutomationBrowserForm',
    'androidAutomationBrowserStart','androidAutomationBrowserInspect',
    'androidAutomationAppPackage','androidAutomationAppStart',
    'androidAutomationAppStop'])
    assert.doesNotMatch(html,new RegExp('id="'+id+'"'),`${id} must not be a manual Setup control`);
  // Automation modes are now preferences ONLY inside Setup -> Access ->
  // Fine-tune permissions. The global Agents Settings category never owns a
  // second copy; URL navigation and Start/Stop are chat-tool actions.
  const accessPanel = html.split('id="androidCosAccessPanel"')[1]?.split('id="androidCosTunnelMethod"')[0] ?? '';
  const fineTune = accessPanel.split('class="android-mcp-fine-tune"')[1] ?? '';
  const agentsSettings = html.split('<section id="androidAutomationGuide"')[1]?.split('</section>')[0] ?? '';
  assert.match(fineTune, /Fine-tune permissions/);
  for (const title of ['Look at files','Change files','Run programs','Sub-agents','Automation modes'])
    assert.match(fineTune,new RegExp(title),`Fine-tune must expose ${title}`);
  for (const id of ['androidAutomationBrowserMode','androidAutomationBrowserStatus',
    'androidAutomationAppMode','androidAutomationAppStatus']) {
    assert.match(fineTune,new RegExp('id="'+id+'"'),`${id} must be inside Setup Access Fine-tune`);
    assert.equal((html.match(new RegExp('id="'+id+'"','g')) || []).length,1,
      `${id} must exist exactly once in the Android UI`);
    assert.doesNotMatch(agentsSettings,new RegExp('id="'+id+'"'),
      `${id} must not be duplicated in Settings Agents`);
  }
  assert.match(fineTune, /<option value="realtime">RealTime Screen<\/option>/);
  assert.match(fineTune,/id="androidMcpAllowLook"/);
  assert.match(fineTune,/id="androidMcpAllowChange"/);
  assert.match(fineTune,/id="androidMcpAllowTerminal"/);
  assert.match(fineTune,/data-mcp-capability="readFiles"/);
  assert.match(fineTune,/data-mcp-capability="createFiles"/);
  assert.match(fineTune,/data-mcp-capability="terminal"/);
  assert.match(fineTune,/Sub-agents[\s\S]*?(?:no|not|separate|independent|without)/i,
    'Sub-agents group must clarify it is not an automatically granted native permission');
  assert.doesNotMatch(fineTune,/Browser control|Desktop control/,
    'Old browser/desktop permission group must be replaced by Automation modes');
  for (const removed of ['androidAutomationBrowserUrl','androidAutomationBrowserOpen',
    'androidAutomationBrowserStop','androidSettingsOpenAgents',
    'androidSettingsOpenSkills','androidSettingsOpenPlugins']) {
    assert.doesNotMatch(html,new RegExp('id="'+removed+'"'),
      `Removed manual/shortcut control ${removed} must not be rendered`);
    assert.doesNotMatch(ui,new RegExp(`\\$\\('${removed}'\\)`),
      `Removed Settings control ${removed} must not be accessed`);
  }
  assert.doesNotMatch(agentsSettings,/id="androidAutomationBrowserMode"|id="androidAutomationAppMode"|Website \(HTTPS\)/);
  assert.match(ui, /android_automation_preferences_status/);
  assert.match(ui, /android_automation_preferences_set/);
  assert.match(ui, /accessibility_automation_mode_status/);
  assert.match(agentsSettings,/id="androidAutomationOpenFineTune"/,
    'Agents Settings should provide only a navigation shortcut to Setup Access Fine-tune');
  assert.match(ui,/getElementById\('androidAutomationOpenFineTune'\)\?\.addEventListener\('click'/,
    'Agents navigation must open Fine-tune rather than duplicate the controls');
  assert.match(ui,/directAndroidChatAction/);
  assert.match(ui,/submitDirectAndroidChat/);
  assert.match(ui,/api\('\/api\/chat-local-action'/);
  assert.match(source('src-tauri/src/android_agent_tools.rs'),/"android_browser_start"/);
  assert.match(source('src-tauri/src/android_agent_tools.rs'),/"android_app_start"/);
  for (const command of ['android_background_browser_start','android_background_browser_stop',
    'android_background_browser_snapshot','accessibility_automation_start',
    'accessibility_automation_status','accessibility_automation_stop',
    'accessibility_automation_resolve_target'])
    assert.ok(rust.includes(command), `${command} must be registered by Tauri`);
  assert.match(kotlin,/WebViewCompat\.setProfile\(web, owned\)/);
  assert.match(kotlin,/WebViewFeature\.MULTI_PROFILE/);
  assert.match(kotlin,/if \(!args\.userInitiated \|\| !allowedHttps\(args\.url\)\)/);
  assert.match(kotlin,/if \(args\.chatAuthorized\)/);
  assert.match(kotlin,/web\.evaluateJavascript\(snapshotJs\)/);
  assert.match(kotlin,/fun stopBrowser\(\)/);
  assert.match(kotlin,/allProfileNames\.filter\(::isOwnedProfile\)/);
  assert.match(kotlin,/cleanupPrefs\.edit\(\)\.putStringSet\("profiles", pendingProfileCleanup\.keys\.toSet\(\)\)/);
  assert.match(kotlin,/profile_cleanup_deferred_until_restart/);
  assert.doesNotMatch(kotlin,/CookieManager\.getInstance\(\)\.removeAllCookies/);
  assert.match(indicator,/TYPE_ACCESSIBILITY_OVERLAY/);
  assert.match(indicator,/event\.eventType == AccessibilityEvent\.TYPE_WINDOW_STATE_CHANGED/);
  assert.match(indicator,/private fun isDifferentApp\(/);
  assert.match(indicator,/transient_window_ignored/);
  assert.match(indicator,/targetEverFocused && isDifferentApp\(/);
  assert.match(indicator,/revokeSession\("app_switched"\)/);
  assert.match(indicator,/Intent\.ACTION_SCREEN_OFF/);
  assert.match(indicator,/registerReceiver\(screenOffReceiver/);
  assert.match(indicator,/putBoolean\("screenUnlocked", screenUnlocked\)/);
  assert.match(indicator,/text = "Mcp Agent Ai is using this app"/);
  assert.match(indicator,/setOnClickListener \{ endSession\(true\) \}/);
  assert.match(indicator,/observedPackage == targetPackageName/);
  // The Accessibility overlay may remain drawn while the approval Activity
  // is actually foreground. A resumed action must prove the same app is the
  // OS-focused root, rather than taking overlay visibility as proof.
  assert.match(indicator,/val targetFocused = active && screenUnlocked && overlayVisible &&\s+targetFocusPackage\(\) == targetPackageName/);
  assert.match(indicator,/putBoolean\("targetFocused", targetFocused\)/);
  const toolPlugin = source('android/AccessibilityToolsPlugin.kt');
  assert.match(toolPlugin,/fun launchAutomationTarget\(targetPackage: String\): Boolean/);
  assert.match(toolPlugin,/getLaunchIntentForPackage\(targetPackage\)/);
  assert.match(toolPlugin,/fun automationResolveTarget\(invoke: Invoke\)/);
  assert.match(toolPlugin,/if \(!launched\) \{/);
  assert.match(toolPlugin,/automationCall\("automationStop"\)/);
  assert.match(toolPlugin,/indicator_not_visible/);
  assert.match(toolPlugin,/if \(before\?\.getBoolean\("visible", false\) != true \|\|\s+before\?\.getBoolean\("targetFocused", false\) != true\)/);
  assert.match(toolPlugin,/if \(observed\.getBoolean\("visible", false\) &&\s+observed\.getBoolean\("targetFocused", false\) &&\s+observed\.getBoolean\("screenUnlocked", false\)\) break/);
  assert.match(toolPlugin,/latest\.getBoolean\("targetFocused", false\) &&\s+latest\.getBoolean\("screenUnlocked", false\) &&\s+!latest\.getBoolean\("stopRequested", true\)/);
  assert.match(patcher,/launchableAppVisibility/);
  assert.match(patcher,/'BackgroundBrowserAutomationPlugin\.kt'/);
});
