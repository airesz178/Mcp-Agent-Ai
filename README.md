# Mcp Agent Ai

**An Android AI agent workspace with ChatGPT sign-in, local tools, projects, private browsing, skills and MCP integration.**

> Preview 0.2.2 · Android 8.0+ (API 26) · ARM64 (arm64-v8a). This is an Android app, not a Windows desktop program.

## Screenshots

![Android chat and composer](docs/screenshots/android-chat.png)

![Android settings and permissions](docs/screenshots/android-settings.png)

![Skills library](docs/screenshots/android-skills.png)

*Screenshots are reproducible UI previews from the Android layout test harness; OS permission prompts and actual device content may differ.*

## Install

1. Get **Mcp-Agent-Ai-Android.apk** from a published GitHub release, when available, or build it yourself using the steps below.
2. On a compatible ARM64 Android phone, open the APK and allow installation from your chosen file manager when Android asks.
3. Open **Mcp Agent Ai → Settings → Setup**. Grant only the permissions needed for the tools you want to use. Accessibility and screen access are controlled by Android.
4. Sign in using the app's ChatGPT authorization flow, or configure a supported API provider and model. A saved credential is not proof that a reply or tool call has succeeded.
5. Choose a project and optionally share a folder using Android's system folder picker. Enable individual plugin capabilities only when needed.

**Updating an existing installation:** install the same-signed APK over the existing app. Do not uninstall or clear app data if you need to retain chats, settings, and folder grants. Preview builds may be debug-signed and may not upgrade a differently signed release.

## What you can do

- **Chat:** save conversations, search chat history, use normal messages and manually reviewed Goal or Plan workflows. Prime can coordinate up to two explicit provider-backed workers with verified lifecycle and history.
- **Browser:** use an app-owned private Android browser with HTTPS navigation, tabs, back/forward and a native right-side drawer. It uses its own WebView data profile rather than your personal Chrome cookies.
- **Android tools:** explicitly approve supported app, browser, file, terminal, and automation actions. Actual availability depends on Android permissions and the current app session.
- **Files and projects:** organize conversations, work with app-private files, grant a specific phone folder through Android's Storage Access Framework, and configure a remote SSH folder using an independently verified Ed25519 host key and a real authenticated connection check (requires an accessible SSH client).
- **Plugins and MCP:** inspect bundled native plugin capabilities, explicitly install or unregister their tool aliases, enable or disable them separately, and connect to supported MCP endpoints and the OpenAI Secure MCP Tunnel when configured. Update checks inspect official release metadata, not auto-install binaries.
- **Skills and pets:** create or import reviewed Markdown instruction skills; enable them individually. Choose from animated in-app pets and reposition one on the chat screen.
- **Settings:** change follow-output behavior, choose whether follow-ups are sent automatically after a verified turn or saved as a draft, configure finished-turn notifications, and adjust appearance.

## Optional downloads and current limits

- **Node.js and npm:** these Android ARM64 executables are not bundled inside the APK. The app can inspect executable availability within its own UID and allows user-initiated, bounded project commands only when the binaries actually exist there. An **optional Termux RUN_COMMAND bridge** can invoke specific Node/npm/SSH operations within a separately installed Termux environment, subject to Android's `com.termux.permission.RUN_COMMAND` permission, Termux 0.109+ with external-app access enabled, and a fresh native Run once approval for every call. It requires a genuine Termux process-exit callback and never claims that Termux packages are installed inside Mcp Agent Ai. Termux scripts belong under its own `~/projects/<project>` and are never automatically shared with app-private projects. npm install is offline-only with lifecycle scripts disabled. Node script execution can still modify Termux-owned files; neither environment is a sandbox.
- **SSH server projects:** the project picker supports an explicit native project, project-specific SSH key generation, independently supplied and pinned Ed25519 host key, and read-only remote probes (`pwd`, `ls`, `git status`). The transport **only works if** an SSH client executable is available to the Android app UID, its public key was authorized by the server, and the real command succeeds. No remote project sync or automatic filesystem mount is implemented; a saved historical connection receipt is not proof of a current session.
- **Marketplace install/update:** only native modules already compiled into this signed APK can be installed into or removed from the tool catalog. Uninstall revokes tool aliases but cannot erase code from the APK. Third-party dynamic plugin downloads and hot swapping are not available. An on-demand update check queries the project's official GitHub stable release metadata with strict HTTPS and limits, without downloading or installing any APK. A newer APK's signing compatibility has not been proven.
- **GeckoView extensions:** the bundled Android browser supports its own compatible extension manager; desktop Chrome/CRX extensions are not Android WebView extensions.
- **AI models:** API access may require a separate account, key, and network connection. Model usage, token billing, continuous autonomous Loop execution, all-language toolchains, third-party plugin auto-updates and full COS desktop parity are not promised in this preview.

## Build from source (Windows development host)

Install **Node.js 22+**, **Rust** with the `aarch64-linux-android` target, **JDK 21**, **Android SDK**, **Android NDK**, and a compatible Gradle environment. Set `JAVA_HOME`, `ANDROID_HOME`, and `NDK_HOME` to your installed SDK paths.

```powershell
npm ci
npm run typecheck
npm test
npm run qa:ui
cargo test --offline --manifest-path native-core/Cargo.toml --lib
cargo test --offline --manifest-path src-tauri/Cargo.toml --lib
npm run native:android
```

The output is `release/Mcp-Agent-Ai-Android.apk`. A Windows host is used for development and QA only; the product target is Android. A manual GitHub Actions workflow is available at `.github/workflows/android-preview.yml`.

## Security and state

Native permissions, model tool proposals, browser DOM access and OS grants require explicit user approval where applicable. The UI only shows a connection as verified when the corresponding native/backend receipt supports it. Browser cookies remain in the app-owned profile; tools must not import personal Chrome credentials. Running jobs interrupted by a process restart must not be silently resumed or marked successful.

See [Android migration notes](docs/NATIVE-MIGRATION.md), [browser design](docs/BROWSER-ENGINE-DESIGN.md), and the [COS parity matrix](docs/COS-ANDROID-PARITY-MATRIX.md) for implementation details, tests, and remaining gaps.
