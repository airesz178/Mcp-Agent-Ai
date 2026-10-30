# Contributing to COS Fusion

COS Fusion is an early experimental project. Contributions should keep model providers replaceable, permissions owned by the local core, and each action auditable. The planned plugin SDK and public source license are not finalized yet; please do not assume a stable extension API.

## Before proposing a change

1. Run `npm ci` and `npm test`.
2. Keep API keys, personal files, cloud credentials and local history out of commits.
3. Add tests for provider protocol changes, tool permission changes, filesystem access and HTTP control endpoints.
4. Be clear about what was verified: source tests, executable smoke, installed desktop and provider/cloud/device operations are different evidence levels.
5. Avoid importing proprietary code or third-party assets without clear distribution rights.

The public issue tracker, community moderation policy, release signing and support channels will be selected before a broad public beta.
