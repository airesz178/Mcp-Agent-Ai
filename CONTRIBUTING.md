# Contributing to Mcp Agent Ai

Mcp Agent Ai is an early experimental project. Contributions should keep model providers replaceable, permissions owned by the local core, and each action auditable. The planned plugin SDK and public source license are not finalized yet; please do not assume a stable extension API.

**Current target: Android 8+ ARM64 only.** Windows is a development/test host,
not an installer target. Do not add Windows release jobs or WebView2 features.

## Before proposing a change

1. Run `npm ci` and `npm test`.
2. Keep API keys, personal files, cloud credentials and local history out of commits.
3. Add tests for provider protocol changes, tool permission changes, filesystem access and HTTP control endpoints.
4. Be clear about what was verified: source tests, Android APK build, installed Android app and provider/cloud/device operations are different evidence levels.
5. Avoid importing proprietary code or third-party assets without clear distribution rights.

The public issue tracker, community moderation policy, release signing and support channels will be selected before a broad public beta.
