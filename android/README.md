# Mcp Agent Ai — Android

Target: Android 8.0+ (API 26). Build with Windows, JDK 17+ and Android SDK 35.

The Android APK contains the same HTML/CSS/JS frontend as the Windows app.
The Gradle `syncSharedUi` task bundles `../ui/` directly into the APK. The
WebView blocks external pages and network resources. Native Java code makes
**real HTTPS requests** to fixed official endpoints for OpenAI, Anthropic and
OpenRouter, using Android's system TLS verification.

Users choose a provider, supply an exact supported model ID and enter an API
key. Keys live **only in process memory**, are never included in saved chat
records and disappear when the app process dies. Provider responses and recent
chat history are saved in app-private SharedPreferences only after a successful
provider response. No built-in test responses or simulated provider is exposed.
Provider usage can incur charges.

Windows-only capabilities such as Hermes CLI, Ollama loopback, skills, file
access, desktop/browser automation and MCP are not offered by the Android
frontend yet. The Android build does not include those engines.

Build:

```powershell
$env:JAVA_HOME = 'C:\Users\haris\.jdks\jbr-21.0.11'
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
cd android
.\gradlew.bat :app:assembleDebug
```

APK: `android/app/build/outputs/apk/debug/app-debug.apk`

After a successful `assembleDebug`, the build automatically stages the APK
and SHA256 in `../release/Mcp-Agent-Ai-<version>-Android-debug.apk` and prunes
only older Android release artifacts. Windows builds, newer versions, Gradle
caches, app data and unrelated files remain untouched.

Android requires the INTERNET permission for native provider requests. It does
not require external storage, Accessibility, screen capture or background access.
The debug APK uses Android's debug signing. A successful build does not prove a
real provider conversation or physical-phone compatibility; those require
testing with a configured provider and a device.
