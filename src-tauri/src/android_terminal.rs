//! Explicit, read-only Android terminal in an isolated app-private folder.
//! Separately provides user-driven installation and launch of the real Termux
//! interactive terminal app. An optional permissioned Termux RUN_COMMAND
//! bridge runs in the separate Termux app UID, never in the in-app PTY.
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
//!     android_terminal::android_termux_open,
//!     android_terminal::android_termux_command_status,
//!     android_terminal::android_termux_command_run]
//! Generated Kotlin must include android/AppTerminalPlugin.kt in package
//! ai.mcpagent.app (the same generated source tree as other Android plugins).
//! Android exposes no separate runtime permission for app-private process I/O.
//! This is NOT a shell, not user-data filesystem access and not background work.

use serde_json::{json, Value};
use std::sync::Mutex;
#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::AppHandle;
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_FILENAME_LENGTH: usize = 64;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_DURATION_MS: usize = 5_000;
const MAX_TERMUX_OUTPUT_BYTES: usize = 16 * 1024;
static TERMUX_COMMAND_GATE: Mutex<()> = Mutex::new(());
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

fn validate_termux_command(
    request_id: &str,
    tool: &str,
    action: &str,
    project: Option<&str>,
    entry: Option<&str>,
    host: Option<&str>,
    username: Option<&str>,
    port: Option<u16>,
    user_initiated: bool,
) -> Result<Option<String>, String> {
    require_termux_user_action(user_initiated)?;
    validate_uuid(request_id)?;
    let no_extra = || -> Result<(), String> {
        if project.is_none()
            && entry.is_none()
            && host.is_none()
            && username.is_none()
            && port.is_none()
        {
            Ok(())
        } else {
            Err("Unsupported extra Termux command arguments".into())
        }
    };
    match (tool, action) {
        ("node" | "npm" | "ssh", "version") => {
            no_extra()?;
            Ok(None)
        }
        ("node", "check" | "run") => {
            let project = project.ok_or("Termux project slug is missing")?;
            let entry = entry.ok_or("Termux script entry is missing")?;
            if host.is_some()
                || username.is_some()
                || port.is_some()
                || !safe_termux_project(project)
                || !safe_termux_entry(entry)
            {
                return Err("Invalid Termux project/script filename".into());
            }
            // Kotlin's TermuxRunPolicy.fromTarget repeats both checks and
            // locates the file only under /data/data/com.termux/files/home/projects.
            Ok(Some(format!("{project}/{entry}")))
        }
        ("npm", "list" | "install") => {
            let project = project.ok_or("Termux project slug is missing")?;
            if entry.is_some()
                || host.is_some()
                || username.is_some()
                || port.is_some()
                || !safe_termux_project(project)
            {
                return Err("Invalid Termux npm project slug or extra arguments".into());
            }
            // Kotlin enforces --offline and --ignore-scripts for npm install.
            Ok(Some(project.into()))
        }
        ("ssh", "test") => {
            let user = username.ok_or("SSH username is missing")?;
            let host = host.ok_or("SSH host is missing")?;
            let port = port.ok_or("SSH port is missing")?;
            if project.is_some()
                || entry.is_some()
                || !safe_ssh_user(user)
                || !safe_ssh_host(host)
                || port == 0
            {
                return Err("Invalid SSH target or extra arguments".into());
            }
            // Kotlin pins StrictHostKeyChecking=yes and runs remote 'true';
            // user-supplied SSH options, passwords and remote commands blocked.
            Ok(Some(format!("{user}@{host}:{port}")))
        }
        _ => Err("Unsupported Termux executable/action".into()),
    }
}

fn safe_termux_project(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn safe_termux_entry(value: &str) -> bool {
    let Some((base, ext)) = value.rsplit_once('.') else {
        return false;
    };
    matches!(ext, "js" | "mjs" | "cjs")
        && (1..=63).contains(&base.len())
        && base.as_bytes()[0].is_ascii_alphanumeric()
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn safe_ssh_user(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0] == b'_')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn safe_ssh_host(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 252
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || value.contains("..")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
    })
}

fn normalize_termux_command_status(source: &Value) -> Result<Value, String> {
    let bool_field = |name: &str| {
        source
            .get(name)
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("Termux command status missing {name}"))
    };
    let supported = bool_field("supported")?;
    let installed = bool_field("installed")?;
    let permission = bool_field("runCommandPermissionGranted")?;
    let foreground = bool_field("foreground")?;
    let result_supported = bool_field("termuxResultSupported")?;
    let service_visible = bool_field("serviceVisible")?;
    let running = bool_field("running")?;
    let external_apps_verified = bool_field("allowExternalAppsVerified")?;
    // A Termux package install or permission grant cannot prove Node, npm,
    // SSH, user Termux setup, or that an actual command has executed.
    if source.get("termuxExecutionContext").and_then(Value::as_str) != Some("external-termux-app")
        || source.get("termuxWorkspaceShared").and_then(Value::as_bool) != Some(false)
        || source.get("userInitiatedRequired").and_then(Value::as_bool) != Some(true)
        || source.get("autoRun").and_then(Value::as_bool) != Some(false)
        || source.get("maxConcurrency").and_then(Value::as_u64) != Some(1)
        || source.get("maxOutputBytes").and_then(Value::as_u64)
            != Some(MAX_TERMUX_OUTPUT_BYTES as u64)
        || source.get("maxWaitMs").and_then(Value::as_u64) != Some(30_000)
        || (supported && (!installed || !permission || !result_supported || !service_visible))
    {
        return Err("Termux command execution context is unverified".into());
    }
    Ok(json!({
        "supported":supported, "installed":installed,
        "runCommandPermissionGranted":permission,
        "foreground":foreground,
        "termuxResultSupported":result_supported,
        "serviceVisible":service_visible,
        "running":running,
        "mode":"external-termux-run-command",
        "executionContext":"external-termux-app",
        "termuxExecutionContext":"external-termux-app",
        "termuxWorkspaceShared":false, "inAppShellAvailable":false,
        "maxConcurrency":1,"userInitiatedRequired":true,
        "maxOutputBytes":MAX_TERMUX_OUTPUT_BYTES,
        "maxWaitMs":30_000,
        "programs":["node","npm","ssh"],
        "autoRun":false,
        "requiresPerCallApproval":true,
        "nodeInstalledVerified":false,
        "npmInstalledVerified":false,
        "sshInstalledVerified":false,
        "executionVerified":false,
        "readyForRequest":supported && installed && permission && result_supported
            && service_visible && foreground && !running,
        "allowExternalAppsVerified":external_apps_verified,
        "allowExternalAppsRequiresUserSetup":true
    }))
}

fn normalize_termux_command_receipt(
    source: &Value,
    request_id: &str,
    tool: &str,
    action: &str,
) -> Result<Value, String> {
    if source.get("requestId").and_then(Value::as_str) != Some(request_id)
        || source.get("tool").and_then(Value::as_str) != Some(tool)
        || source.get("action").and_then(Value::as_str) != Some(action)
        || source.get("source").and_then(Value::as_str) != Some("termux_run_command")
        || source.get("termuxExecutionContext").and_then(Value::as_str)
            != Some("external-termux-app")
        || source.get("termuxWorkspaceShared").and_then(Value::as_bool) != Some(false)
        || source.get("resultVerified").and_then(Value::as_bool) != Some(true)
    {
        return Err("Termux command receipt did not match the user request".into());
    }
    if source.get("processStarted").and_then(Value::as_bool) != Some(true)
        || source.get("completed").and_then(Value::as_bool) != Some(true)
        || source.get("timedOut").and_then(Value::as_bool) != Some(false)
    {
        return Err("Termux command did not complete with verified process evidence".into());
    }
    let code = source
        .get("exitCode")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or("Termux exit code is missing or invalid")?;
    let success = source
        .get("success")
        .and_then(Value::as_bool)
        .ok_or("Termux success result missing")?;
    if success != (code == 0) {
        return Err("Termux process exit code contradicts success claim".into());
    }
    let output = source
        .get("stdout")
        .and_then(Value::as_str)
        .ok_or("Termux stdout was not returned")?;
    let error = source
        .get("stderr")
        .and_then(Value::as_str)
        .ok_or("Termux stderr was not returned")?;
    let original_stdout = source
        .get("stdoutOriginalLength")
        .and_then(Value::as_u64)
        .ok_or("Termux stdout byte length was not verified")?;
    let original_stderr = source
        .get("stderrOriginalLength")
        .and_then(Value::as_u64)
        .ok_or("Termux stderr byte length was not verified")?;
    if original_stdout.saturating_add(original_stderr) > MAX_TERMUX_OUTPUT_BYTES as u64
        || output.len().saturating_add(error.len()) > MAX_TERMUX_OUTPUT_BYTES
        || original_stdout < output.len() as u64
        || original_stderr < error.len() as u64
    {
        return Err("Termux output lengths were truncated, inconsistent or too large".into());
    }
    Ok(json!({
        "requestId":request_id, "tool":tool, "action":action,
        "source":"termux_run_command",
        "processStarted":true, "completed":true,
        "resultVerified":true,
        "exitCode":code, "timedOut":false,
        "success":success,
        "exitCodeVerified":true,
        "stdout":output,"stderr":error,
        "stdoutOriginalLength":original_stdout,
        "stderrOriginalLength":original_stderr,
        "mode":"external-termux-run-command",
        "executionContext":"external-termux-app",
        "termuxExecutionContext":"external-termux-app",
        "termuxWorkspaceShared":false, "inAppShellAvailable":false,
        "perCallApprovalRequired":true
    }))
}

fn termux_command_status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let value = mobile_call(&app, "termuxCommandStatus", json!({}))?;
        normalize_termux_command_status(&value)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({
            "supported":false,"installed":false,"runCommandPermissionGranted":false,
            "foreground":false,"mode":"external-termux-run-command",
            "executionContext":"external-termux-app",
            "termuxExecutionContext":"external-termux-app",
            "termuxWorkspaceShared":false,"inAppShellAvailable":false,
            "maxConcurrency":1,"userInitiatedRequired":true,"autoRun":false,
            "termuxResultSupported":false,"serviceVisible":false,"running":false,
            "maxOutputBytes":MAX_TERMUX_OUTPUT_BYTES,"maxWaitMs":30_000,
            "programs":["node","npm","ssh"],
            "requiresPerCallApproval":true,
            "nodeInstalledVerified":false,"npmInstalledVerified":false,
            "sshInstalledVerified":false,"executionVerified":false,"readyForRequest":false,
            "allowExternalAppsVerified":false,"allowExternalAppsRequiresUserSetup":true
        }))
    }
}

fn termux_command_run(
    app: AppHandle,
    request_id: String,
    tool: String,
    action: String,
    project: Option<String>,
    entry: Option<String>,
    host: Option<String>,
    username: Option<String>,
    port: Option<u16>,
    user_initiated: bool,
) -> Result<Value, String> {
    let target = validate_termux_command(
        &request_id,
        &tool,
        &action,
        project.as_deref(),
        entry.as_deref(),
        host.as_deref(),
        username.as_deref(),
        port,
        user_initiated,
    )?;
    #[cfg(target_os = "android")]
    {
        let _active = TERMUX_COMMAND_GATE
            .try_lock()
            .map_err(|_| "Another Termux command is still active")?;
        let status = termux_command_status(app.clone())?;
        if status["readyForRequest"] != true {
            return Err("Termux RUN_COMMAND is not installed, foreground, or OS-authorized".into());
        }
        let native = mobile_call(
            &app,
            "termuxCommandRun",
            json!({
                "requestId":request_id,
                "tool":tool, "action":action, "target":target,
                "userInitiated":true
            }),
        )?;
        normalize_termux_command_receipt(&native, &request_id, &tool, &action)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, target);
        Err("Permissioned Termux RUN_COMMAND is available only on Android".into())
    }
}

#[tauri::command]
pub async fn android_termux_command_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || termux_command_status(app))
        .await
        .map_err(|_| "Termux command status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_termux_command_run(
    app: AppHandle,
    request_id: String,
    tool: String,
    action: String,
    project: Option<String>,
    entry: Option<String>,
    host: Option<String>,
    username: Option<String>,
    port: Option<u16>,
    user_initiated: bool,
) -> Result<Value, String> {
    validate_termux_command(
        &request_id,
        &tool,
        &action,
        project.as_deref(),
        entry.as_deref(),
        host.as_deref(),
        username.as_deref(),
        port,
        user_initiated,
    )?;
    tauri::async_runtime::spawn_blocking(move || {
        termux_command_run(
            app,
            request_id,
            tool,
            action,
            project,
            entry,
            host,
            username,
            port,
            user_initiated,
        )
    })
    .await
    .map_err(|_| "Termux command request interrupted".to_owned())?
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

    #[test]
    fn termux_structured_request_converts_only_known_safe_arguments() {
        let check = |tool, action, project, entry, host, username, port, approved| {
            validate_termux_command(
                ID, tool, action, project, entry, host, username, port, approved,
            )
        };
        assert_eq!(
            check("node", "version", None, None, None, None, None, true).unwrap(),
            None
        );
        assert_eq!(
            check("npm", "version", None, None, None, None, None, true).unwrap(),
            None
        );
        assert_eq!(
            check("ssh", "version", None, None, None, None, None, true).unwrap(),
            None
        );
        assert_eq!(
            check(
                "node",
                "run",
                Some("demo_1"),
                Some("main.mjs"),
                None,
                None,
                None,
                true
            )
            .unwrap(),
            Some("demo_1/main.mjs".into())
        );
        assert_eq!(
            check(
                "node",
                "check",
                Some("demo-2"),
                Some("app.js"),
                None,
                None,
                None,
                true
            )
            .unwrap(),
            Some("demo-2/app.js".into())
        );
        assert_eq!(
            check(
                "npm",
                "install",
                Some("project-1"),
                None,
                None,
                None,
                None,
                true
            )
            .unwrap(),
            Some("project-1".into())
        );
        assert_eq!(
            check(
                "ssh",
                "test",
                None,
                None,
                Some("example.org"),
                Some("alice"),
                Some(2222),
                true
            )
            .unwrap(),
            Some("alice@example.org:2222".into())
        );
        assert!(check("node", "version", None, None, None, None, None, false).is_err());
        assert!(check("node", "eval", None, None, None, None, None, true).is_err());
        assert!(check(
            "npm",
            "run",
            Some("demo"),
            Some("build.js"),
            None,
            None,
            None,
            true
        )
        .is_err());
        assert!(check(
            "node",
            "run",
            Some("../secrets"),
            Some("main.js"),
            None,
            None,
            None,
            true
        )
        .is_err());
        assert!(check(
            "node",
            "run",
            Some("demo"),
            Some("../../main.js"),
            None,
            None,
            None,
            true
        )
        .is_err());
        assert!(check(
            "node",
            "run",
            Some("demo"),
            Some("-e.js"),
            None,
            None,
            None,
            true
        )
        .is_err());
        assert!(check(
            "node",
            "run",
            Some("demo"),
            Some("x.js;id"),
            None,
            None,
            None,
            true
        )
        .is_err());
        assert!(check(
            "npm",
            "install",
            Some("demo"),
            None,
            Some("evil"),
            None,
            None,
            true
        )
        .is_err());
        assert!(check(
            "ssh",
            "test",
            None,
            None,
            Some("-oProxyCommand=sh"),
            Some("alice"),
            Some(22),
            true
        )
        .is_err());
        assert!(check(
            "ssh",
            "test",
            None,
            None,
            Some("example.org"),
            Some("alice"),
            Some(0),
            true
        )
        .is_err());
        assert!(check(
            "ssh",
            "test",
            None,
            None,
            Some("example.org"),
            Some("root;id"),
            Some(22),
            true
        )
        .is_err());
    }

    fn example_termux_status() -> Value {
        json!({
            "supported":true, "installed":true, "termuxResultSupported":true,
            "serviceVisible":true, "runCommandPermissionGranted":true,
            "foreground":true, "allowExternalAppsVerified":false,
            "running":false, "termuxExecutionContext":"external-termux-app",
            "termuxWorkspaceShared":false, "userInitiatedRequired":true,
            "maxConcurrency":1, "maxOutputBytes":16384, "maxWaitMs":30000,
            "autoRun":false
        })
    }

    #[test]
    fn termux_status_is_external_not_a_ready_native_pty() {
        let s = normalize_termux_command_status(&example_termux_status()).unwrap();
        assert_eq!(s["termuxExecutionContext"], "external-termux-app");
        assert_eq!(s["inAppShellAvailable"], false);
        assert_eq!(s["termuxWorkspaceShared"], false);
        assert_eq!(s["readyForRequest"], true);
        assert_eq!(s["allowExternalAppsVerified"], false);
        assert_eq!(s["termuxResultSupported"], true);
        assert_eq!(s["maxOutputBytes"], 16384);
        assert_eq!(s["maxWaitMs"], 30000);
        assert_eq!(s["executionVerified"], false);
        let mut blocked = example_termux_status();
        blocked["runCommandPermissionGranted"] = json!(false);
        blocked["supported"] = json!(false);
        assert_eq!(
            normalize_termux_command_status(&blocked).unwrap()["readyForRequest"],
            false
        );
        let mut busy = example_termux_status();
        busy["running"] = json!(true);
        assert_eq!(
            normalize_termux_command_status(&busy).unwrap()["readyForRequest"],
            false
        );
        let mut fake_context = example_termux_status();
        fake_context["termuxExecutionContext"] = json!("app-private-uid");
        assert!(normalize_termux_command_status(&fake_context).is_err());
        let mut inconsistent = example_termux_status();
        inconsistent["termuxResultSupported"] = json!(false);
        assert!(normalize_termux_command_status(&inconsistent).is_err());
    }

    fn example_termux_receipt() -> Value {
        json!({
            "requestId":ID, "tool":"node", "action":"version",
            "processStarted":true, "completed":true, "resultVerified":true,
            "success":true, "exitCode":0, "stdout":"v22.0.0\n", "stderr":"",
            "stdoutOriginalLength":8, "stderrOriginalLength":0,
            "source":"termux_run_command",
            "termuxExecutionContext":"external-termux-app",
            "termuxWorkspaceShared":false,"timedOut":false
        })
    }

    #[test]
    fn termux_receipt_requires_true_pending_intent_exit_evidence() {
        let source = example_termux_receipt();
        let r = normalize_termux_command_receipt(&source, ID, "node", "version").unwrap();
        assert_eq!(r["resultVerified"], true);
        assert_eq!(r["exitCodeVerified"], true);
        assert_eq!(r["success"], true);
        assert_eq!(r["stdoutOriginalLength"], 8);
        assert_eq!(r["termuxExecutionContext"], "external-termux-app");
        let mut failed = source.clone();
        failed["exitCode"] = json!(17);
        failed["success"] = json!(false);
        let returned = normalize_termux_command_receipt(&failed, ID, "node", "version").unwrap();
        assert_eq!(returned["completed"], true);
        assert_eq!(returned["success"], false);
        assert_eq!(returned["exitCode"], 17);

        for (field, invalid) in [
            ("resultVerified", json!(false)),
            ("completed", json!(false)),
            ("processStarted", json!(false)),
            ("timedOut", json!(true)),
            ("exitCode", Value::Null),
            ("success", json!(false)),
            ("source", json!("startService")),
            ("termuxExecutionContext", json!("app-private-uid")),
            ("termuxWorkspaceShared", json!(true)),
            ("requestId", json!("other")),
            ("tool", json!("ssh")),
            ("action", json!("run")),
            ("stdoutOriginalLength", json!(1)),
            ("stderrOriginalLength", Value::Null),
            ("stdoutOriginalLength", json!(20000)),
            ("stdout", json!("a".repeat(16385))),
        ] {
            let mut broken = source.clone();
            broken[field] = invalid;
            assert!(
                normalize_termux_command_receipt(&broken, ID, "node", "version").is_err(),
                "{field}"
            );
        }
    }
}
