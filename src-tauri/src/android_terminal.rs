//! Explicit, read-only Android terminal in an isolated app-private folder.
//! Separately provides user-driven installation and launch of the real Termux
//! interactive terminal app; no in-app Termux PTY or shell is claimed.
//!
//! Prime integration (not performed in this worker's file ownership):
//!   mod android_terminal;
//!   #[cfg(target_os = "android")]
//!   let builder = builder.plugin(android_terminal::init());
//!   tauri::generate_handler![...,
//!     android_terminal::android_terminal_status,
//!     android_terminal::android_terminal_run,
//!     android_terminal::android_terminal_cancel,
//!     android_terminal::android_termux_status,
//!     android_terminal::android_termux_install,
//!     android_terminal::android_termux_open]
//! Generated Kotlin must include android/AppTerminalPlugin.kt in package
//! ai.mcpagent.app (the same generated source tree as other Android plugins).
//! Android exposes no separate runtime permission for app-private process I/O.
//! This is NOT a shell, not user-data filesystem access and not background work.

use serde_json::{json, Value};
#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::AppHandle;
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_FILENAME_LENGTH: usize = 64;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_DURATION_MS: usize = 5_000;
const RESERVED: &[&str] = &[
    "secret",
    "token",
    "key",
    "history",
    "session",
    "credential",
    "config",
    "private",
    "project",
    "auth",
    "password",
    "backup",
];

#[cfg(target_os = "android")]
struct AndroidTerminal<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("app-terminal")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "AppTerminalPlugin")?;
            app.manage(AndroidTerminal(plugin));
            Ok(())
        })
        .build()
}

fn validate_uuid(id: &str) -> Result<(), String> {
    let bytes = id.as_bytes();
    if bytes.len() != 36
        || [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-')
        || bytes.iter().enumerate().any(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                false
            } else {
                !c.is_ascii_hexdigit() || c.is_ascii_uppercase()
            }
        })
        || !(b'1'..=b'8').contains(&bytes[14])
        || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
    {
        return Err("A canonical lowercase request UUID is required".into());
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.len() < 5
        || name.len() > MAX_FILENAME_LENGTH
        || !name.ends_with(".txt")
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name.as_bytes()[..name.len() - 4]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        || RESERVED
            .iter()
            .any(|word| name.to_ascii_lowercase().contains(word))
    {
        return Err("Only simple non-sensitive single-segment .txt filenames are allowed".into());
    }
    Ok(())
}

fn validate_run(
    request_id: &str,
    command: &str,
    name: Option<&str>,
    user_initiated: bool,
) -> Result<(), String> {
    if !user_initiated {
        return Err("Terminal requires explicit user initiation".into());
    }
    validate_uuid(request_id)?;
    match command {
        "pwd" | "ls" if name.is_none() => Ok(()),
        "stat" | "cat" => {
            validate_name(name.ok_or("This terminal command requires a .txt filename")?)
        }
        _ => Err("Unsupported terminal command or arguments".into()),
    }
}

fn validate_cancel(request_id: &str, user_initiated: bool) -> Result<(), String> {
    if !user_initiated {
        return Err("Terminal cancellation requires an explicit user action".into());
    }
    validate_uuid(request_id)
}

fn mobile_call(app: &AppHandle, method: &str, args: Value) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let handle = app
            .try_state::<AndroidTerminal<tauri::Wry>>()
            .ok_or("Android app-terminal plugin is not registered")?;
        return handle
            .0
            .run_mobile_plugin(method, args)
            .map_err(|_| "Android terminal operation did not complete safely".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, method, args);
        Err("App-private ProcessBuilder terminal is Android only".into())
    }
}

pub fn status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        mobile_call(&app, "status", json!({}))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({
            "supported": false, "permissionRequired": false,
            "permissionGranted": false, "running": false,
            "maxOutputBytes": MAX_OUTPUT_BYTES, "maxDurationMs": MAX_DURATION_MS,
            "maxConcurrency": 1, "commands": ["pwd", "ls", "stat", "cat"],
            "scope": "Dedicated app-private terminal-public folder",
            "autoRun": false, "userInitiatedRequired": true
        }))
    }
}

pub fn run(
    app: AppHandle,
    request_id: String,
    command: String,
    name: Option<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    validate_run(&request_id, &command, name.as_deref(), user_initiated)?;
    mobile_call(
        &app,
        "run",
        json!({
            "requestId": request_id, "command": command,
            "name": name, "userInitiated": true
        }),
    )
}

pub fn cancel(app: AppHandle, request_id: String, user_initiated: bool) -> Result<Value, String> {
    validate_cancel(&request_id, user_initiated)?;
    mobile_call(
        &app,
        "cancel",
        json!({
            "requestId": request_id, "userInitiated": true
        }),
    )
}

fn require_termux_user_action(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("Termux handoff requires an explicit user action".into())
    }
}

pub fn termux_status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        mobile_call(&app, "termuxStatus", json!({}))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({
            "supported": false,
            "installed": false,
            "launchable": false,
            "mode": "external-termux-app",
            "inAppShellAvailable": false,
            "setupVerified": false,
            "installUrl": "https://f-droid.org/packages/com.termux/",
            "installIsUserManaged": true,
            "userInitiatedRequired": true,
            "requiresRunCommandPermission": false,
            "commandsAvailableInApp": false,
            "note": "Termux integration is Android-only"
        }))
    }
}

pub fn termux_install(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    require_termux_user_action(user_initiated)?;
    mobile_call(&app, "termuxInstall", json!({"userInitiated": true}))
}

pub fn termux_open(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    require_termux_user_action(user_initiated)?;
    mobile_call(&app, "termuxOpen", json!({"userInitiated": true}))
}

#[tauri::command]
pub async fn android_termux_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || termux_status(app))
        .await
        .map_err(|_| "Android Termux status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_termux_install(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || termux_install(app, user_initiated))
        .await
        .map_err(|_| "Android Termux install handoff interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_termux_open(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || termux_open(app, user_initiated))
        .await
        .map_err(|_| "Android Termux launch interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_terminal_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || status(app))
        .await
        .map_err(|_| "Android terminal status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_terminal_run(
    app: AppHandle,
    request_id: String,
    command: String,
    name: Option<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        run(app, request_id, command, name, user_initiated)
    })
    .await
    .map_err(|_| "Android terminal request interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_terminal_cancel(
    app: AppHandle,
    request_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || cancel(app, request_id, user_initiated))
        .await
        .map_err(|_| "Android terminal cancellation interrupted".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "123e4567-e89b-42d3-a456-426614174000";

    #[test]
    fn allowlisted_read_only_commands_only() {
        for command in ["pwd", "ls"] {
            assert!(validate_run(ID, command, None, true).is_ok());
            assert!(validate_run(ID, command, Some("a.txt"), true).is_err());
        }
        for command in ["stat", "cat"] {
            assert!(validate_run(ID, command, Some("notes.txt"), true).is_ok());
            assert!(validate_run(ID, command, None, true).is_err());
        }
        for command in [
            "sh",
            "bash",
            "env",
            "echo",
            "rm",
            "ps",
            "find",
            "ls -l",
            "cat /proc",
        ] {
            assert!(validate_run(ID, command, None, true).is_err());
        }
    }

    #[test]
    fn rejects_path_traversal_flags_secrets_and_control_files() {
        for bad in [
            "",
            ".",
            "..",
            "../notes.txt",
            "/data/data/notes.txt",
            "-n.txt",
            "a\\b.txt",
            "a/b.txt",
            "a.txt\nfoo",
            "sessions.txt",
            "api_key.txt",
            "token.txt",
            "project.txt",
            "config.txt",
            "history.txt",
            "credentials.txt",
            "example.TXT",
            "semi;colon.txt",
            "star*.txt",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name("a.txt").is_ok());
        assert!(validate_name("a-b_c.1.txt").is_ok());
        assert!(validate_name(&format!("{}.txt", "a".repeat(61))).is_err());
    }

    #[test]
    fn requires_explicit_consent_and_valid_request_uuid_per_call() {
        assert!(validate_run(ID, "pwd", None, false).is_err());
        assert!(validate_cancel(ID, false).is_err());
        assert!(validate_cancel(ID, true).is_ok());
        for bad in [
            "",
            "not-a-uuid",
            "../",
            "123e4567-e89b-42d3-a456-42661417400z",
            "123E4567-E89B-42D3-A456-426614174000",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert!(validate_run(bad, "pwd", None, true).is_err());
            assert!(validate_cancel(bad, true).is_err());
        }
    }

    #[test]
    fn published_limits_are_conservative() {
        assert_eq!(MAX_OUTPUT_BYTES, 16_384);
        assert_eq!(MAX_DURATION_MS, 5_000);
        assert_eq!(MAX_FILENAME_LENGTH, 64);
    }

    #[test]
    fn termux_handoff_only_after_user_action() {
        assert!(require_termux_user_action(false).is_err());
        assert!(require_termux_user_action(true).is_ok());
    }
}
