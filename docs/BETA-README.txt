MCP AGENT AI PREVIEW 0.2.2 - ANDROID ARM64
==========================================

Current product target: Android 8.0+ (API 26), ARM64 only.
Windows is used to develop/build/test the Android app; no Windows installer.

Preview APK: release/Mcp-Agent-Ai-Android.apk
Package: ai.mcpagent.app; versionName 0.2.2; versionCode 2002.
This is an EARLY DEBUG-SIGNED PREVIEW, not a finished COS replacement.

SAFE INSTALLATION / UPDATE
- Verify package identity, signing certificate, APK v2 signature and SHA256.
- Only install on a personally owned/authorized phone that is idle and cool.
- For an existing installation, use adb install -r -t to preserve app data.
- NEVER uninstall, clear app data or grant Android permissions silently.
- Obtain any requested Android permissions through the OS consent screen.

NATIVE ANDROID BROWSER
- Main UI and private browser use Android WebView.
- Optional Custom Browser uses GeckoView with user-reviewed WebExtensions.
- Chrome unpacked extensions/CRX and privileged COS Chrome extensions do
  NOT automatically run on Android; do not claim Chrome parity.
- GeckoView does not promise unrestricted background/headless operation.

USER DATA / PROVIDERS
- Prompt, answer and session content may be stored inside app-private storage.
- API keys are intended to stay in process memory; review actual provider flows
  before using sensitive conversations or files.
- Paid API providers may charge for model use.
- Setup and connection panels must never infer verification from a toggle,
  a loaded page, a local listener or a configured key alone.

LIMITATIONS / TEST STATUS
- Full COS v2.1.49 feature parity has NOT been verified.
- A previous GeckoView build rendered HTTPS on an Android 11 realme device.
- The latest toolbar and extension install/management implementation requires
  acceptance testing after a safe in-place APK update.
- Production release requires Android security review, up-to-date GeckoView,
  trusted signing, lifecycle/data migration, permission and provider tests.

No Windows, macOS, Linux or iOS package is distributed.
