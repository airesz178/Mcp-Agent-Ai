import { copyFile, readFile, writeFile, mkdir, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const config = JSON.parse(await readFile(path.join(root, 'src-tauri', 'tauri.conf.json'), 'utf8'));
// Tauri's app.windows is a cross-platform WebView list, not a Windows
// distribution option. An empty list silently produces a blank Android
// Activity with no main browser view even when Kotlin and Rust launch.
if (!Array.isArray(config?.app?.windows) ||
    config.app.windows.length !== 1 ||
    config.app.windows[0]?.label !== 'main') {
  throw new Error('Android requires exactly one Tauri main WebView window; refusing a blank-screen APK');
}
const desired = config?.bundle?.android?.minSdkVersion;
if (desired !== 26) throw new Error('Supported Android minimum must be exactly API 26');
const gradlePath = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'build.gradle.kts');
const source = await readFile(gradlePath, 'utf8');
const sdk = source.match(/^(\s*minSdk = )(\d+)(\s*)$/m);
if (!sdk || Number(sdk[2]) < 24 || Number(sdk[2]) > 37) {
  throw new Error('Generated Gradle minimum SDK not recognized; refusing blind edit');
}
let updated = source.replace(/^(\s*minSdk = )\d+(\s*)$/m, (_, start, end) => `${start}${desired}${end}`);
// Idempotent: first and subsequent builds must have the same HTTPS-only policy.
const placeholder = /manifestPlaceholders\["usesCleartextTraffic"\] = "(true|false)"/g;
if (![...updated.matchAll(placeholder)].length) {
  throw new Error('Generated Gradle cleartext policy not recognized');
}
updated = updated.replace(placeholder, 'manifestPlaceholders["usesCleartextTraffic"] = "false"');
// Google OAuth cannot run inside app-owned WebView. Keep AndroidX Custom Tabs
// pinned here so regenerated Tauri Android projects open the user's signed-in
// Chrome profile with a close control returning to the calling activity.
const browserDependency = 'implementation("androidx.browser:browser:1.10.0")';
if (!updated.includes(browserDependency)) {
  if (!updated.includes('dependencies {')) {
    throw new Error('Generated Gradle dependencies block not recognized');
  }
  updated = updated.replace('dependencies {', `dependencies {\n    ${browserDependency}`);
}
if (updated !== source) await writeFile(gradlePath, updated);
// Tauri's generated template enables edge-to-edge without applying WebView insets,
// which draws our header under Android's status bar and footer under navigation.
// Keep this patch idempotent if Tauri regenerates MainActivity during a build.
const activityPath = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', 'MainActivity.kt');
const activity = await readFile(activityPath, 'utf8');
if (!/class MainActivity\s*:\s*TauriActivity\(\)/.test(activity) ||
    !/super\.onCreate\(savedInstanceState\)/.test(activity)) {
  throw new Error('Generated Android MainActivity not recognized; refusing blind edit');
}
const hasImport = /import androidx\.activity\.enableEdgeToEdge\s*\r?\n/.test(activity);
const hasCall = /^\s*enableEdgeToEdge\(\)\s*$/m.test(activity);
if (hasImport !== hasCall) throw new Error('MainActivity edge-to-edge source inconsistent');
// Windows-generated Kotlin may use CRLF. Normalize the generated Activity
// once so guarded imports and idempotent Back-handler matching are stable.
let fixedActivity = activity.replace(/\r\n/g, '\n')
  .replace(/^import androidx\.activity\.enableEdgeToEdge\s*\r?\n/m, '')
  .replace(/^\s*enableEdgeToEdge\(\)\s*\r?\n/m, '\n');
// The bundled Android app is intentionally portrait-only. Setting the launch
// activity orientation in the manifest is the primary request; reinforce that
// request on recreation/resume when returning from another application.
// Do not change the Windows UI or the global device auto-rotate setting.
const orientationRequest = 'requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT';
if (!fixedActivity.includes('import android.content.pm.ActivityInfo')) {
  if (!fixedActivity.includes('import android.os.Bundle')) {
    throw new Error('Generated Android MainActivity imports not recognized');
  }
  fixedActivity = fixedActivity.replace(
    'import android.os.Bundle', 'import android.content.pm.ActivityInfo\nimport android.os.Bundle');
}
if (!fixedActivity.includes(orientationRequest)) {
  fixedActivity = fixedActivity.replace(
    '    super.onCreate(savedInstanceState)',
    '    ' + orientationRequest + '\n    super.onCreate(savedInstanceState)');
}
if (!fixedActivity.includes('override fun onResume()')) {
  if (!/\n}\s*$/.test(fixedActivity)) {
    throw new Error('Generated Android MainActivity body not recognized');
  }
  fixedActivity = fixedActivity.replace(/\n}(\s*)$/,
    '\n  override fun onResume() {\n' +
    '    super.onResume()\n' +
    '    if (requestedOrientation != ActivityInfo.SCREEN_ORIENTATION_PORTRAIT) {\n' +
    '      ' + orientationRequest + '\n' +
    '    }\n' +
    '  }\n}$1');
}
if ((fixedActivity.match(/override fun onResume\(\)/g) || []).length !== 1 ||
    !fixedActivity.includes('    super.onResume()') ||
    (fixedActivity.match(/requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT/g) || []).length !== 2) {
  throw new Error('Generated MainActivity portrait guard does not match expected lifecycle');
}
// TauriActivity disables Wry's WebView-history Back handler. Without our
// AndroidX dispatcher, Android finishes MainActivity even when Setup, a
// Settings category or another app panel is still open inside the main
// WebView. Only the exact primary Tauri WebView delivered by WryActivity's
// onWebViewCreate is eligible; private, automation and ChatGPT WebViews are
// intentionally never searched or injected.
const backImports = [
  'import android.net.Uri',
  'import android.os.Handler',
  'import android.os.Looper',
  'import android.webkit.WebView',
  'import androidx.activity.OnBackPressedCallback',
  'import androidx.lifecycle.Lifecycle',
  'import java.lang.ref.WeakReference',
];
for (const importLine of backImports) {
  if (!fixedActivity.includes(importLine + '\n')) {
    if (!fixedActivity.includes('import android.os.Bundle\n')) {
      throw new Error('Generated MainActivity import insertion anchor unavailable');
    }
    fixedActivity = fixedActivity.replace('import android.os.Bundle\n',
      'import android.os.Bundle\n' + importLine + '\n');
  }
}
// A fixed, zero-argument script (no DOM input, URLs, credentials or arbitrary
// statements from the renderer). Event listeners handle navigation
// synchronously and preventDefault() makes the receipt true.
const backDispatchImplementation = String.raw`
  private var mcpMainWebView: WeakReference<WebView>? = null
  private var mcpBackInFlight = false
  private var mcpBackGeneration = 0
  private val mcpBackScript =
    """(function(){const event=new Event('mcp-android-back',{cancelable:true});window.dispatchEvent(event);return event.defaultPrevented;})()"""

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    // Wry calls this hook only for its primary Activity WebView; a dialog or
    // automation browser does not register its WebView here.
    mcpMainWebView = WeakReference(webView)
  }

  private fun isMcpAppAssetUrl(url: String?): Boolean {
    if (url.isNullOrBlank()) return false
    return try {
      val uri = Uri.parse(url)
      uri.userInfo == null && uri.port == -1 && (
        (uri.host == "tauri.localhost" && (uri.scheme == "http" || uri.scheme == "https")) ||
        (uri.host == "localhost" && uri.scheme == "tauri")
      )
    } catch (_: Exception) {
      false
    }
  }

  private fun dispatchMcpAndroidBack(unhandled: () -> Unit) {
    if (isFinishing || isDestroyed || !lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) return
    // A second hardware/gesture Back cannot skip ahead while JS is deciding.
    if (mcpBackInFlight) return
    val webView = mcpMainWebView?.get()
    val initialUrl = webView?.url
    if (webView == null || !isMcpAppAssetUrl(initialUrl)) {
      // No app-owned loaded WebView: do not evaluate JS on another origin.
      unhandled()
      return
    }
    mcpBackInFlight = true
    val generation = ++mcpBackGeneration
    // If WebView is destroyed or its callback never fires, unlock Back for a
    // later attempt; an unknown receipt must NOT accidentally exit Setup.
    Handler(Looper.getMainLooper()).postDelayed({
      if (generation == mcpBackGeneration) {
        mcpBackInFlight = false
        ++mcpBackGeneration
      }
    }, 1500L)
    try {
      webView.evaluateJavascript(mcpBackScript) { receipt ->
        if (generation != mcpBackGeneration) return@evaluateJavascript
        mcpBackInFlight = false
        ++mcpBackGeneration
        // evaluateJavascript returns literal JSON true/false. Never assume
        // a null or malformed receipt means the root chat was unhandled.
        if (receipt == "false" && !isFinishing && !isDestroyed &&
          lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED) &&
          mcpMainWebView?.get() === webView && webView.url == initialUrl &&
          isMcpAppAssetUrl(webView.url)) {
          unhandled()
        }
      }
    } catch (_: RuntimeException) {
      if (generation == mcpBackGeneration) {
        mcpBackInFlight = false
        ++mcpBackGeneration
        // No JavaScript was dispatched: preserve Android's default Back.
        if (!isFinishing && !isDestroyed) unhandled()
      }
    }
  }

  override fun onStop() {
    ++mcpBackGeneration
    mcpBackInFlight = false
    super.onStop()
  }

  override fun onDestroy() {
    ++mcpBackGeneration
    mcpBackInFlight = false
    mcpMainWebView = null
    super.onDestroy()
  }

  private fun registerMcpAndroidBack() {
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        dispatchMcpAndroidBack {
          // Call the real Android/Tauri fallback only when JS did not cancel
          // the event, or when no app WebView exists. Disable ourselves first
          // so ComponentActivity's dispatcher does not recursively invoke us.
          isEnabled = false
          try {
            @Suppress("DEPRECATION")
            super@MainActivity.onBackPressed()
          } finally {
            isEnabled = true
          }
        }
      }
    })
  }
`;
const backRegistration = '    registerMcpAndroidBack()';
const backMarker = '  private var mcpMainWebView: WeakReference<WebView>? = null';
if (!fixedActivity.includes(backMarker)) {
  if (!/\n}\s*$/.test(fixedActivity) ||
      fixedActivity.includes('override fun onBackPressed()') ||
      fixedActivity.includes('registerMcpAndroidBack()')) {
    throw new Error('Generated MainActivity Back handler has unexpected source');
  }
  fixedActivity = fixedActivity.replace(/\n}(\s*)$/,
    '\n' + backDispatchImplementation + '\n}$1');
} else if (!fixedActivity.includes(backDispatchImplementation)) {
  throw new Error('Generated MainActivity Back handler drifted; refusing an unsafe duplicate');
}
if (!fixedActivity.includes(backRegistration)) {
  const onCreateAnchor = '    super.onCreate(savedInstanceState)';
  if (!fixedActivity.includes(onCreateAnchor))
    throw new Error('Generated MainActivity onCreate Back registration anchor missing');
  fixedActivity = fixedActivity.replace(onCreateAnchor,
    onCreateAnchor + '\n' + backRegistration);
}
if ((fixedActivity.match(/private fun registerMcpAndroidBack\(\)/g) || []).length !== 1 ||
    (fixedActivity.match(/    registerMcpAndroidBack\(\)/g) || []).length !== 1 ||
    (fixedActivity.match(/override fun onWebViewCreate\(webView: WebView\)/g) || []).length !== 1 ||
    (fixedActivity.match(/'mcp-android-back'/g) || []).length !== 1) {
  throw new Error('Generated MainActivity Back integration must be unique and event-scoped');
}
if (fixedActivity !== activity) await writeFile(activityPath, fixedActivity);

// Explicit all-files permission only: no auto grant or file access outside
// the private workspace. This patch survives Tauri regenerating Android files.
const manifestPath = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'AndroidManifest.xml');
const manifest = await readFile(manifestPath, 'utf8');
const permissionTags = [
  // User-granted Android "Additional permissions" for external Termux
  // RUN_COMMAND. Declaring it is NOT an OS grant or Termux approval.
  '<uses-permission android:name="com.termux.permission.RUN_COMMAND" />',
  '<uses-permission android:name="android.permission.MANAGE_EXTERNAL_STORAGE" />',
  // Android requires this normal manifest permission before it will show the
  // package-scoped battery exemption consent dialog. The OS grant is still
  // explicitly user-controlled and separately verified by PowerManager.
  '<uses-permission android:name="android.permission.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS" />',
  '<uses-permission android:name="android.permission.SYSTEM_ALERT_WINDOW" />',
  '<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />',
  '<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />',
  // Android 14+ requires a type-specific permission. This is a user-started,
  // visible long-running assistant/runtime session, not a dataSync transfer
  // (which has a timeout on Android 15+) or a privileged systemExempted job.
  '<uses-permission android:name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE" />',
  '<uses-permission android:name="android.permission.WAKE_LOCK" />',
  // Optional Android App info > Permissions capabilities. These are NOT
  // prerequisites for Setup 1-9, are never requested at startup, and do not
  // grant any capability before the OS gives explicit user consent.
  '<uses-permission android:name="android.permission.CAMERA" />',
  '<uses-permission android:name="android.permission.RECORD_AUDIO" />',
  '<uses-permission android:name="android.permission.ACCESS_COARSE_LOCATION" />',
];
const optionalHardwareTags = [
  '<uses-feature android:name="android.hardware.camera" android:required="false" />',
  '<uses-feature android:name="android.hardware.camera.autofocus" android:required="false" />',
  '<uses-feature android:name="android.hardware.microphone" android:required="false" />',
  '<uses-feature android:name="android.hardware.location" android:required="false" />',
  '<uses-feature android:name="android.hardware.location.network" android:required="false" />',
];
if (!manifest.includes('<manifest ') || !manifest.includes('<uses-permission android:name="android.permission.INTERNET" />')) {
  throw new Error('Generated Android manifest layout unexpected; refusing storage patch');
}
let updatedManifest = manifest;
// One-frame MediaProjection was removed from the Android product. Tauri's
// generated manifest is persistent between builds, so strip only its exact
// former service and permission as well as omitting them from fresh builds.
updatedManifest = updatedManifest
  .replace(/^\s*<uses-permission android:name="android\.permission\.FOREGROUND_SERVICE_MEDIA_PROJECTION"\s*\/>\s*\r?\n/m, '\n')
  .replace(/^\s*<service android:name="\.ScreenCaptureService"\s+android:exported="false"\s+android:foregroundServiceType="mediaProjection"\s*\/>\s*\r?\n/m, '\n');
const activityTags = [...updatedManifest.matchAll(/<activity\b[^>]*>/g)]
  .map(match => match[0]).filter(tag => tag.includes('android:name=".MainActivity"'));
if (activityTags.length !== 1) {
  throw new Error('Expected exactly one generated Android launcher activity');
}
const mainActivityTag = activityTags[0];
if (!mainActivityTag.includes('android:screenOrientation="portrait"')) {
  if (mainActivityTag.includes('android:screenOrientation=') ||
      !mainActivityTag.includes('android:exported="true"')) {
    throw new Error('Android launcher orientation/exported attributes do not match expected shape');
  }
  updatedManifest = updatedManifest.replace(mainActivityTag,
    mainActivityTag.replace('android:exported="true"',
      'android:screenOrientation="portrait"\n            android:exported="true"'));
}
// Android's IME must shrink the Activity content rather than pan a full-height
// WebView behind the keyboard. Keep the manifest patch idempotent on rebuild.
const orientedActivityTag = [...updatedManifest.matchAll(/<activity\b[^>]*>/g)]
  .map(match => match[0]).filter(tag => tag.includes('android:name=".MainActivity"'))[0];
if (!orientedActivityTag.includes('android:windowSoftInputMode="adjustResize"')) {
  if (orientedActivityTag.includes('android:windowSoftInputMode=')) {
    throw new Error('Unexpected Android soft keyboard mode; refusing unsafe replacement');
  }
  updatedManifest = updatedManifest.replace(orientedActivityTag,
    orientedActivityTag.replace('android:exported="true"',
      'android:windowSoftInputMode="adjustResize"\n            android:exported="true"'));
}
// The OAuth callback itself stays strictly bound to 127.0.0.1. The scheme
// below contains NO tokens, authorization code or state; it only focuses the
// app after the listener has validated and consumed the callback.
const oauthReturnFilter = `<intent-filter>
                <action android:name="android.intent.action.VIEW" />
                <category android:name="android.intent.category.DEFAULT" />
                <category android:name="android.intent.category.BROWSABLE" />
                <data android:scheme="mcpagentai" android:host="auth-complete" />
            </intent-filter>`;
if (!updatedManifest.includes('android:scheme="mcpagentai"')) {
  const launcherEnd = '</activity>';
  if (!updatedManifest.includes(launcherEnd) ||
      !updatedManifest.includes('android:name=".MainActivity"')) {
    throw new Error('Android launcher OAuth return insertion point not found');
  }
  updatedManifest = updatedManifest.replace(launcherEnd,
    `    ${oauthReturnFilter}\n        ${launcherEnd}`);
}
// Android 11+ filters PackageManager.resolveActivity without an explicit
// visibility declaration. Expose only the system speech-recognition intent;
// no broad QUERY_ALL_PACKAGES access or app-owned microphone permission.
const speechRecognizerQuery = `<queries>
        <intent><action android:name="android.speech.action.RECOGNIZE_SPEECH" /></intent>
    </queries>`;
if (!updatedManifest.includes('android.speech.action.RECOGNIZE_SPEECH')) {
  if (!updatedManifest.includes('<uses-permission android:name="android.permission.INTERNET" />'))
    throw new Error('Cannot insert bounded speech recognizer visibility query');
  updatedManifest = updatedManifest.replace(
    '<uses-permission android:name="android.permission.INTERNET" />',
    `${speechRecognizerQuery}\n    <uses-permission android:name="android.permission.INTERNET" />`);
}
// Only expose the exact Termux package to Android package visibility.
// Termux is a separate application and can only be installed/opened by a
// deliberate user action. No QUERY_ALL_PACKAGES or shell permission.
const termuxVisibility = '<package android:name="com.termux" />';
if (!updatedManifest.includes(termuxVisibility)) {
  const queriesEnd = '</queries>';
  if (!updatedManifest.includes(queriesEnd))
    throw new Error('Cannot find Android package visibility query container');
  updatedManifest = updatedManifest.replace(queriesEnd,
    '    ' + termuxVisibility + '\n    ' + queriesEnd);
}
// An explicitly named installed app can be opened after the user starts a
// visible Accessibility indicator session. Android 11+ restricts discovery
// of third-party launch activities without this intent-scoped query.
// No QUERY_ALL_PACKAGES or permission to inspect app data is needed.
const launchableAppVisibility = '<intent><action android:name="android.intent.action.MAIN" /><category android:name="android.intent.category.LAUNCHER" /></intent>';
if (!updatedManifest.includes(launchableAppVisibility)) {
  const queriesEnd = '</queries>';
  if (!updatedManifest.includes(queriesEnd))
    throw new Error('Cannot find Android package visibility query container');
  updatedManifest = updatedManifest.replace(queriesEnd,
    '    ' + launchableAppVisibility + '\n    ' + queriesEnd);
}
for (const permissionTag of permissionTags) {
  if (!updatedManifest.includes(permissionTag))
    updatedManifest = updatedManifest.replace(
      '<uses-permission android:name="android.permission.INTERNET" />',
      '<uses-permission android:name="android.permission.INTERNET" />\n    ' + permissionTag);
}
for (const featureTag of optionalHardwareTags) {
  if (!updatedManifest.includes(featureTag)) {
    if (!updatedManifest.includes('<application')) {
      throw new Error('Cannot add optional hardware compatibility declaration');
    }
    updatedManifest = updatedManifest.replace(
      '<application', featureTag + '\n    <application');
  }
}
// Restore the Android OS-controlled AccessibilityService. Selected-window
// inspection remains absent; OS grant is never silently enabled by the app.
const accessibilityService = `<service android:name=".AgentAccessibilityService"
            android:label="Mcp Agent Ai Accessibility"
            android:permission="android.permission.BIND_ACCESSIBILITY_SERVICE"
            android:process=":accessibility"
            android:exported="true">
          <intent-filter>
            <action android:name="android.accessibilityservice.AccessibilityService" />
          </intent-filter>
          <meta-data android:name="android.accessibilityservice"
            android:resource="@xml/accessibility_config" />
        </service>`;
if (!updatedManifest.includes('android:name=".AgentAccessibilityService"')) {
  if (!updatedManifest.includes('    </application>'))
    throw new Error('AccessibilityService manifest insertion point unavailable');
  updatedManifest = updatedManifest.replace('    </application>',
    `        ${accessibilityService}\n    </application>`);
}
// The browser/runtime can restart independently of the OS service. Use one
// non-exported provider in the service process to attest its live binding.
const accessibilityServiceTag = [...updatedManifest.matchAll(/<service\b[^>]*>/g)]
  .map(match => match[0])
  .find(tag => tag.includes('android:name=".AgentAccessibilityService"'));
if (!accessibilityServiceTag) throw new Error('Missing Accessibility service manifest entry');
if (!accessibilityServiceTag.includes('android:process=":accessibility"')) {
  updatedManifest = updatedManifest.replace(accessibilityServiceTag,
    accessibilityServiceTag.replace('android:exported="true"',
      'android:process=":accessibility"\n            android:exported="true"'));
}
const accessibilityHealthProvider = `<provider
          android:name=".AccessibilityHealthProvider"
          android:authorities="\${applicationId}.accessibilityhealth"
          android:process=":accessibility"
          android:exported="false" />`;
if (!updatedManifest.includes('android:name=".AccessibilityHealthProvider"')) {
  updatedManifest = updatedManifest.replace('    </application>',
    `        ${accessibilityHealthProvider}\n    </application>`);
}
const foregroundRuntimeService = `<service android:name=".ForegroundRuntimeService"
            android:label="Mcp Agent Ai background runtime"
            android:exported="false"
            android:stopWithTask="false"
            android:foregroundServiceType="specialUse">
          <property android:name="android.app.PROPERTY_SPECIAL_USE_FGS_SUBTYPE"
            android:value="Explicit user-started persistent assistant runtime and native MCP connection while the app is in the background; user-visible notification with Stop action" />
        </service>`;
if (!updatedManifest.includes('android:name=".ForegroundRuntimeService"')) {
  if (!updatedManifest.includes('    </application>'))
    throw new Error('Foreground runtime service insertion point unavailable');
  updatedManifest = updatedManifest.replace('    </application>',
    `        ${foregroundRuntimeService}\n    </application>`);
} else {
  const serviceTag = [...updatedManifest.matchAll(/<service\b[^>]*>/g)]
    .map(match => match[0]).find(tag => tag.includes('android:name=".ForegroundRuntimeService"'));
  if (!serviceTag || !serviceTag.includes('android:exported="false"') ||
      !serviceTag.includes('android:foregroundServiceType="specialUse"'))
    throw new Error('Foreground runtime service declaration does not match guarded manifest');
}
// Termux returns RUN_COMMAND results through a one-shot PendingIntent to a
// private receiver; no exported Android callback accepts arbitrary broadcasts.
const termuxResultReceiver = '<receiver android:name=".TermuxRunResultReceiver" android:exported="false" />';
if (!updatedManifest.includes('android:name=".TermuxRunResultReceiver"')) {
  if (!updatedManifest.includes('    </application>'))
    throw new Error('Termux result receiver manifest insertion point unavailable');
  updatedManifest = updatedManifest.replace('    </application>',
    '        ' + termuxResultReceiver + '\n    </application>');
} else if (!updatedManifest.includes(termuxResultReceiver)) {
  throw new Error('Existing Termux result receiver does not match non-exported policy');
}
if (updatedManifest !== manifest) await writeFile(manifestPath, updatedManifest);
await copyFile(
  path.join(root, 'android', 'StorageAccessPlugin.kt'),
  path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', 'StorageAccessPlugin.kt')
);
await copyFile(
  path.join(root, 'android', 'MainStoragePlugin.kt'),
  path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', 'MainStoragePlugin.kt')
);
await copyFile(
  path.join(root, 'android', 'DocumentTreePlugin.kt'),
  path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', 'DocumentTreePlugin.kt')
);
await copyFile(
  path.join(root, 'android', 'PrivateBrowserPlugin.kt'),
  path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', 'PrivateBrowserPlugin.kt')
);
await copyFile(
  path.join(root, 'android', 'BackgroundChatGptPlugin.kt'),
  path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', 'BackgroundChatGptPlugin.kt')
);
await copyFile(
  path.join(root, 'android', 'ChatGptOAuthPlugin.kt'),
  path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java',
    'ai', 'mcpagent', 'app', 'ChatGptOAuthPlugin.kt')
);
for (const file of [
  'AppTerminalPlugin.kt', 'TermuxRunPolicy.kt',
  'TermuxRunResultReceiver.kt', 'ReviewSharePlugin.kt',
  'PetOverlayPlugin.kt',
  'SpeechDictationPlugin.kt',
  'TunnelCredentialPlugin.kt',
  'ForegroundRuntimeService.kt', 'ForegroundRuntimePlugin.kt',
  'TurnCompletionPlugin.kt',
  'BackgroundBrowserAutomationPlugin.kt',
  'AgentAccessibilityService.kt', 'AccessibilityToolsPlugin.kt',
  'AccessibilityHealthProvider.kt',
]) {
  await copyFile(path.join(root, 'android', file),
    path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app', file));
}
const generatedTerminalTestDir = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'test', 'java', 'ai', 'mcpagent', 'app');
await mkdir(generatedTerminalTestDir, {recursive:true});
await copyFile(path.join(root, 'android', 'TermuxRunPolicyTest.kt'),
  path.join(generatedTerminalTestDir, 'TermuxRunPolicyTest.kt'));
// Delete generated copies of retired functionality. The old Android
// Plugins approval notification/dialog has no UI or native registration:
// ChatGPT's own Plugins permission setting now governs tool calls.
const generatedKotlinDir = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'java', 'ai', 'mcpagent', 'app');
for (const oldName of ['ScreenCaptureService.kt', 'OneShotCapturePlugin.kt',
  'PluginsApprovalAlertPlugin.kt'])
  await rm(path.join(generatedKotlinDir, oldName), {force:true});
const accessibilityXmlDir = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'res', 'xml');
await mkdir(accessibilityXmlDir, {recursive:true});
await copyFile(path.join(root, 'android', 'accessibility_config.xml'),
  path.join(accessibilityXmlDir, 'accessibility_config.xml'));

// Keep Android's system bars consistent with the WebView's dark shell, in both
// system appearance modes. The generated Material3 default is pale lavender.
const systemBarItems = new Map([
  ['android:statusBarColor', '#0c111b'],
  ['android:navigationBarColor', '#0c111b'],
  ['android:windowLightStatusBar', 'false'],
  ['android:windowLightNavigationBar', 'false'],
]);
for (const mode of ['values', 'values-night']) {
  const themePath = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'res', mode, 'themes.xml');
  const theme = await readFile(themePath, 'utf8');
  const declaration = '<style name="Theme.mcp_agent_ai_native" parent="Theme.Material3.DayNight.NoActionBar">';
  if (!theme.includes(declaration) || !theme.includes('</style>')) {
    throw new Error(`Generated ${mode} Android theme not recognized`);
  }
  let fixedTheme = theme;
  for (const [name, value] of systemBarItems) {
    const item = `<item name="${name}">${value}</item>`;
    if (fixedTheme.includes(item)) continue;
    if (fixedTheme.includes(`<item name="${name}">`)) {
      throw new Error(`Unexpected custom Android theme item: ${name}`);
    }
    fixedTheme = fixedTheme.replace(declaration, `${declaration}\n        ${item}`);
  }
  if (fixedTheme !== theme) await writeFile(themePath, fixedTheme);
}
// The checked-in branded icon is canonical. Tauri's generated Android template
// still ships a stock launcher icon unless these resources are synchronized.
// Keep launcher identity and the Android welcome-screen icon consistent.
const iconRoot = path.join(root, 'src-tauri', 'icons', 'android');
const resRoot = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'res');
for (const density of ['mdpi', 'hdpi', 'xhdpi', 'xxhdpi', 'xxxhdpi']) {
  for (const name of ['ic_launcher.png', 'ic_launcher_round.png']) {
    await copyFile(
      path.join(iconRoot, `mipmap-${density}`, name),
      path.join(resRoot, `mipmap-${density}`, name),
    );
  }
}

// Opt-in extension-capable GeckoView is a *second* Android browser engine.
// Keep it separate from the main Tauri WebView and existing private WebViews.
// Pin a reviewed Mozilla artifact; do not resolve a moving nightly version.
// GeckoView 153's published Kotlin stdlib 2.3.21 is within Kotlin 2.2's
// readable metadata range (up to 2.3.0). GeckoView 154+ requires Kotlin 2.4.
// Pin this reviewed version until the whole Android toolchain can upgrade.
const geckoVersion = '153.0.20260810162159';
const geckoDependency = `implementation("org.mozilla.geckoview:geckoview:${geckoVersion}")`;
const topGradlePath = path.join(root, 'src-tauri', 'gen', 'android', 'build.gradle.kts');
const topGradle = await readFile(topGradlePath, 'utf8');
const repositoryMarker = /allprojects\s*\{\s*repositories\s*\{\s*google\(\)\s*mavenCentral\(\)\s*\}\s*\}/m;
let geckoTopGradle = topGradle;
if (!topGradle.includes('https://maven.mozilla.org/maven2/')) {
  if (!repositoryMarker.test(topGradle)) throw new Error('Generated Gradle repositories unexpectedly changed');
  geckoTopGradle = topGradle.replace(repositoryMarker, `allprojects {
    repositories {
        google()
        mavenCentral()
        maven { url = uri("https://maven.mozilla.org/maven2/") }
    }
}`);
}
// Preserve Tauri's Kotlin 2.2 compiler. A previously attempted 2.4.20 plugin
// upgrade is repaired without changing the generated Android project layout.
geckoTopGradle = geckoTopGradle.replace(
  'kotlin-gradle-plugin:2.4.20', 'kotlin-gradle-plugin:2.2.10');
if (!geckoTopGradle.includes('kotlin-gradle-plugin:2.2.10') ||
    !geckoTopGradle.includes('https://maven.mozilla.org/maven2/'))
  throw new Error('GeckoView Kotlin/repository setup did not apply');
if (geckoTopGradle !== topGradle) await writeFile(topGradlePath, geckoTopGradle);
let geckoAppGradle = await readFile(gradlePath, 'utf8');
geckoAppGradle = geckoAppGradle.replace(
  /implementation\("org\.mozilla\.geckoview:geckoview:[^"]+"\)/g, geckoDependency);
if (!geckoAppGradle.includes(geckoDependency)) {
  const marker = 'implementation("androidx.webkit:webkit:1.14.0")';
  if (!geckoAppGradle.includes(marker)) throw new Error('Generated Gradle dependency anchor missing');
  geckoAppGradle = geckoAppGradle.replace(marker, `${marker}\n    ${geckoDependency}`);
}
geckoAppGradle = geckoAppGradle
  .replace(/JavaVersion\.VERSION_1_8/g, 'JavaVersion.VERSION_17')
  .replace(/JvmTarget\.JVM_1_8/g, 'JvmTarget.JVM_17');
if (!geckoAppGradle.includes('JavaVersion.VERSION_17') ||
    !geckoAppGradle.includes('JvmTarget.JVM_17'))
  throw new Error('GeckoView Java/Kotlin 17 configuration missing');
if (geckoAppGradle !== await readFile(gradlePath, 'utf8'))
  await writeFile(gradlePath, geckoAppGradle);
await copyFile(path.join(root, 'android', 'CustomGeckoBrowserPlugin.kt'),
  path.join(generatedKotlinDir, 'CustomGeckoBrowserPlugin.kt'));
const geckoAssets = path.join(root, 'src-tauri', 'gen', 'android', 'app', 'src', 'main', 'assets', 'mcp_extension_probe');
await mkdir(geckoAssets, {recursive: true});
for (const name of ['manifest.json', 'background.js']) {
  await copyFile(path.join(root, 'android', 'mcp_extension_probe', name),
    path.join(geckoAssets, name));
}
console.log(`Tauri Android validated: minSdk=${desired}, cleartext disabled, inset-safe dark system bars`);
