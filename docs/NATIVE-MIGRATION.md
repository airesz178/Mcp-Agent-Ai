# Mcp Agent Ai — TypeScript + Rust + Tauri 2

**Current platform target (changed 2026-11-04): Android 8.0+ ARM64 only.**
Windows is the SDK, ADB and test host, not a deployable application target.
Do not publish Windows, Linux, macOS or iOS packages.

## Structure

- `ui/app.ts`: TypeScript source for the EXISTING app layout; `npm run build:ui`
  compiles the bundle to `ui/app.js`. The static HTML/CSS remain shared.
- `native-core/`: shared Rust network/provider, in-memory credentials and
  app-local session persistence. The core does not generate fake responses.
- `src-tauri/`: native Android Tauri v2 host, Tauri commands and
  Android packaging. Android Gradle files are generated with
  `npx tauri android init --ci`.
- `src/` and `android/`: previous Node and Java backends are RETAINED as
  verified fallback while native feature parity is implemented. Never delete
  their source or data as part of release artifact cleanup.

## Historical development and current Android status

- Windows desktop build and custom browser tests were historical migration
  experiments, not a supported product after the Android-only decision.
  Windows-specific source commands, NSIS scripts/workflow and installer are
  removed from the active app/release path.
- Android ARM64: Tauri Android debug APK produced by the Windows NDK toolchain;
  the native library is built from the same `native-core` source.
- TypeScript, Node, Rust, Gradle and Playwright are checked on the Windows
  development host before Android packaging. Source compilation does not
  establish network/provider verification or complete COS parity.
- Realme Android 11 previously executed a native GeckoView build and loaded
  HTTPS content. The newer toolbar and extension-install flow still require
  acceptance on an updated installed APK.

## Local development / checks

```powershell
npm ci
npm run typecheck
npm run build:ui
npm run native:test
npm run native:check
npm run qa:ui
```

For Android, set JDK 21, `ANDROID_HOME` and `NDK_HOME`, then:

```powershell
npx tauri android init --ci --skip-targets-install
npm run native:android
```

The Android native app has HTTPS OpenAI, Anthropic and OpenRouter provider
code. Provider connection must be verified through a real response.
No API keys are written to storage. Chat sessions persist inside Tauri's
app data directory. Tauri Android does not expose desktop-only tools.

**Not full parity**: COS privileged extension, desktop-only browser APIs,
general unrestricted device automation and several integration surfaces.
The Node/Playwright developer preview is retained for source checks, **not**
as a shipped Windows application.

No live model calls, Android device install or public release is implied by
unit tests or a successfully compiled binary. Installed Android runtime QA,
signing and profile/migration reviews remain required.

Only **one canonical Android APK** is a deliverable:
`release/Mcp-Agent-Ai-Android.apk`. It is a debug-signed preview. After a
verified native Android build, staging replaces that filename and removes
older recognized Android package variants. **Source** for old backends,
app settings/conversations, installed application and Rust/Gradle caches
are never removed by release cleanup. Android debug builds retain
Rust debug symbols and are larger than optimized production builds.

The generated Android Gradle template may default to API 24 even when Tauri's
minSdkVersion is configured as 26. `prenative:android` enforces API 26
(Android 8.0) and disables debug cleartext traffic before every Android build.
Do not bypass this check or commit generated Gradle files as source of truth.

## Android-only artifact checkpoint — 2026-11-04

The previous Windows product target was removed from npm, NSIS config,
workflow, native Tauri command registration, Windows browser modules and
the Custom Browser settings UI. The known canonical Windows EXE was
deleted from release after checking its exact original SHA256/size.
The Windows development host is retained solely for builds and tests.

The updated Android build ran successfully using JDK 21 and Tauri
android build with ARM64 target. Final canonical release:

- release/Mcp-Agent-Ai-Android.apk
- **402,498,919 bytes**
- SHA256 **CE74F90537CCDF369F8CC76C0E8D9D55D57DA0EB49A34ACCB52C3455133F0F30**
- v2 signature **verified**, **one signer**, same original signer
  certificate SHA256 **c37260ac6d930ae28632a843cc5fa395111769038db98b38685525164bdae685**
- Package **ai.mcpagent.app**, versionName **0.2.2**,
  versionCode **2002**, minSdk **26**, targetSdk **37**, ABI **arm64-v8a**
- The release folder contains **only this APK** and no EXE/Windows setup.

Host regressions after removing the platform: TypeScript PASS, Node
**30/30 PASS**, src-tauri Rust **77/77 PASS**, Playwright UI
**661 assertions PASS** (desktop used only as test harness, Android
320/360/390px included). Real-device installation of this APK was **not**
performed: the previously used wireless ADB serial was offline, so
installed APK hash, updated GeckoView UI and extension management
remain physically unverified. No Android uninstall, data clear or
permission mutation was attempted.

## Later Android Custom Browser trim — 2026-11-04

The separate Custom browser & extensions Settings panel has no external
URL entry, signed-XPI URL entry or XPI review button. The signed-XPI
native command/Kotlin importer were removed. GeckoView launches its
prior starter page and retains its own address bar, bundled test
extension, and existing extension management. No app data migration,
uninstall or permission changes performed.

New Android-only ARM64 debug APK built and verified:
`release/Mcp-Agent-Ai-Android.apk`, **402253159 bytes**,
SHA256 **B09BFF4651498C2EC566A6BDD0444017FA0BC8D6E6226FABC2265510F44125FA**,
v2 signature true and original signer, package/version unchanged.
Node **30/30**, Rust Tauri **77/77**, Playwright UI **661** checks and
TypeScript all passed. ADB phone was online but the existing app was
foreground at the most recent check, so **no in-place install** was made.

## Android black-screen regression: root cause and fix (2026-11-04)

After Android-only refactoring, the installed APK showed a completely
blank/dark Activity. ADB screenshot confirmed no UI, and the Android
view hierarchy contained an empty content FrameLayout. The generated
Tauri config inside that APK contained \`"app":{"windows":[]}\`:
removing the desktop installer had incorrectly removed Tauri's
**cross-platform main WebView window configuration**.

The source \`src-tauri/tauri.conf.json\` now declares one \`main\`
window. \`scripts/patch-tauri-android.mjs\` fails early if that
cross-platform WebView definition disappears. Added a regression
assertion to \`test/android-orientation.test.mjs\`.
Rebuilt Android debug APK; directly inspected its ZIP entry
\`assets/tauri.conf.json\` and verified **\`app.windows[0].label=main\`**.
Node 31/31 PASS, Rust Tauri 77/77 PASS, TypeScript PASS, Playwright
661 checks PASS. APK v2 signature and original signer verified.

New canonical APK: \`release/Mcp-Agent-Ai-Android.apk\`,
**402269907 bytes**, SHA256
**73C8DFB81D1563B5DFB1B76E52260980D3BFED2752BE0722A5839A43FBDEA3CD**.
The **fixed APK has not been installed** as of this checkpoint:
the realme was actively in TikTok and still receiving touch input;
38 guarded safety checks never reached the required idle-on-launcher
state. The phone's installed APK still reports \`lastUpdateTime
2026-11-04 12:48:18\`. No uninstall, data clear or permission
mutation occurred. **Next: install in place with \`adb install -r -t\`
only when launcher foreground + no user input for 120s + cool, then
visually verify actual chat UI and check logcat before claiming the
black-screen bug fixed on the device.**
