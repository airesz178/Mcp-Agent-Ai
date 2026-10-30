COS FUSION PREVIEW 0.2.2 - WINDOWS x64
===================================

This is an EARLY LOCAL PREVIEW, not a finished Codex / Claude Code / Hermes alternative.
Only share it with testers who understand the current limitations.

HOW TO TRY
1. Extract this ZIP and double-click COS-Fusion-Preview.exe.
2. A command window will stay open while COS Fusion is running.
3. The preview opens in the default web browser on 127.0.0.1 (localhost).
4. Choose Demo (no API key). Click Customize, enable Workspace, and type /list.
5. For an actual model, select a provider, enter its exact model ID, and provide
   a valid API key through the API key button (or use your environment variables).
6. Close the command window to stop COS Fusion.
7. To use Hermes Agent, install and configure Hermes separately, choose Hermes
   in the provider selector and enable External agents in Customize first.
   Hermes uses its OWN model, credentials and tool permissions; it can invoke
   tools or edit files according to that configuration. Model ID is optional.

YOUR DATA
- No centralized COS service or telemetry is used by this preview.
- Prompts, responses, and recent conversation history are saved UNENCRYPTED at
  %USERPROFILE%\.cos-fusion\state.json. Use only test data.
- Workspace file access is initially OFF and limited to the local sample folder:
  %USERPROFILE%\.cos-fusion\workspace.
- API keys entered in the UI are held in the local process's memory and disappear
  when the process closes. They are not written to the COS state file.
- When using a real provider, text and any tool output submitted to the model are
  sent to that provider under its terms, and may incur usage charges.
- There is no continuous cloud runner; closing the app stops local work.

WINDOWS WARNING
This prototype EXE is NOT code-signed and may show an Unknown publisher / SmartScreen
warning. We have not established trusted public distribution or reputation. Do not
bypass a security warning for a file that is not from a verified project source.
Verify the SHA256 checksum against the release's SHA256SUMS.txt before testing.

LIMITATIONS
Preview does not include Codex app-server, Claude Code runtime, full Hermes gateway,
desktop control, live browser, subagents, cloud execution or automatic updates.
The first chat is local demo unless you configure a real model; sessions are limited
to 25 recent threads and model responses do not stream yet.

PUBLIC RELEASE READINESS
Before distributing broadly: agree on project license and branding; publish source
and reproducible releases; security review + trusted signing; test on clean Windows
devices and across supported models; write a privacy policy and issue-reporting path.
