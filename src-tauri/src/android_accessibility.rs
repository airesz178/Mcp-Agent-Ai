//! Android Accessibility status, consent Settings and targeted app automation
//! indicator and separately approved, targeted Accessibility actions.
//! Action entrypoints are deliberately NOT Tauri commands: a WebView JSON bool
//! cannot authorize LLM-initiated OS actions. The trusted caller must obtain
//! a separate per-call approval before invoking any action function below.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct AccessibilityHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("accessibility-tools")
        .setup(|app, api| {
            let plugin =
                api.register_android_plugin("ai.mcpagent.app", "AccessibilityToolsPlugin")?;
            app.manage(AccessibilityHandle(plugin));
            Ok(())
        })
        .build()
}

fn explicit(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("Opening Android Accessibility settings requires explicit user action".into())
    }
}

fn validate_automation_start(
    user_initiated: bool,
    session_id: &str,
    target_package: &str,
) -> Result<(), String> {
    if !user_initiated {
        return Err("Starting app automation requires explicit user action".into());
    }
    if !(8..=128).contains(&session_id.len())
        || !session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("Invalid app automation session ID".into());
    }
    let target_ok = (3..=200).contains(&target_package.len())
        && target_package.split('.').count() >= 2
        && target_package.split('.').all(|segment| {
            let mut bytes = segment.bytes();
            matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    if !target_ok
        || target_package == "ai.mcpagent.app"
        || target_package == "android"
        || target_package.starts_with("com.android.")
        || target_package.starts_with("com.google.android.permissioncontroller")
    {
        return Err("Target must be an allowed third-party app package".into());
    }
    Ok(())
}

fn validate_selected_mode(selected_mode: &str) -> Result<(), String> {
    match selected_mode {
        "realtime" | "screenshot_click" => Ok(()),
        _ => Err("Unknown Android app automation mode".into()),
    }
}

/// The connected Accessibility service independently reports its OS-granted
/// content, gesture and screenshot capabilities. A saved mode is not a grant.
fn validate_automation_mode_receipt(selected_mode: &str, receipt: Value) -> Result<Value, String> {
    validate_selected_mode(selected_mode)?;
    let invalid = || "Invalid Android automation capability probe".to_owned();
    let object = receipt.as_object().ok_or_else(invalid)?;
    if object.len() != 13
        || object.get("selectedMode").and_then(Value::as_str) != Some(selected_mode)
    {
        return Err(invalid());
    }
    let read = |key: &str| -> Result<bool, String> {
        object.get(key).and_then(Value::as_bool).ok_or_else(invalid)
    };
    let supported = read("supported")?;
    let enabled = read("enabled")?;
    let connected = read("connected")?;
    let reconnect = read("needsReconnect")?;
    let semantic = read("semanticActionAvailable")?;
    let screenshot = read("screenCaptureAvailable")?;
    let gesture = read("tapAvailable")?;
    let realtime = read("realtimeAvailable")?;
    let screenshot_click = read("screenshotClickAvailable")?;
    if (enabled && !supported)
        || (connected && !enabled)
        || (reconnect && (!enabled || connected))
        || (!connected && (semantic || screenshot || gesture))
        || realtime != semantic
        || screenshot_click != (screenshot && gesture)
    {
        return Err(invalid());
    }
    let effective = object
        .get("effectiveMode")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let fallback = read("fallbackUsed")?;
    if !matches!(effective, "realtime" | "screenshot_click" | "unavailable")
        || (effective == "realtime" && (!semantic || selected_mode != "realtime"))
        || (effective == "screenshot_click" && !screenshot_click)
        || fallback != (selected_mode == "realtime" && effective == "screenshot_click")
    {
        return Err(invalid());
    }
    let reason = if !supported {
        "android_unsupported"
    } else if !enabled {
        "accessibility_permission_missing"
    } else if !connected {
        "accessibility_service_disconnected"
    } else if effective == "realtime" {
        "semantic_ready"
    } else if selected_mode == "realtime" && effective == "screenshot_click" {
        "semantic_fallback_to_screenshot_click"
    } else if effective == "screenshot_click" {
        "screenshot_click_ready"
    } else if matches!(
        object.get("reason").and_then(Value::as_str),
        Some("capability_probe_failed" | "target_session_required")
    ) {
        object
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else if selected_mode == "realtime" {
        "semantic_action_and_screenshot_tap_unavailable"
    } else {
        "screenshot_tap_unavailable"
    };
    if object.get("reason").and_then(Value::as_str) != Some(reason) {
        return Err(invalid());
    }
    Ok(receipt)
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, command: &str, args: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<AccessibilityHandle<tauri::Wry>>()
        .ok_or_else(|| "Android accessibility plugin is not registered".to_string())?;
    handle
        .0
        .run_mobile_plugin(command, args)
        .map_err(|_| "Android accessibility status or settings unavailable".into())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _command: &str, _args: Value) -> Result<Value, String> {
    Err("Android accessibility is available only on Android".into())
}

#[tauri::command]
pub async fn accessibility_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || call(app, "status", json!({})))
        .await
        .map_err(|_| "Accessibility status interrupted".to_string())?
}

#[tauri::command]
pub async fn accessibility_request_settings(
    app: AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    explicit(user_initiated)?;
    tauri::async_runtime::spawn_blocking(move || {
        call(app, "requestSettings", json!({"userInitiated": true}))
    })
    .await
    .map_err(|_| "Accessibility settings interrupted".to_string())?
}

#[tauri::command]
pub async fn accessibility_automation_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || call(app, "automationStatus", json!({})))
        .await
        .map_err(|_| "App automation status interrupted".to_string())?
}

/// Read-only Browser & App controls mode probe; never change persisted mode.
#[tauri::command]
pub async fn accessibility_automation_mode_status(
    app: AppHandle,
    selected_mode: String,
) -> Result<Value, String> {
    validate_selected_mode(&selected_mode)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let receipt = call(
            app,
            "automationModeStatus",
            json!({"selectedMode":selected_mode}),
        )?;
        validate_automation_mode_receipt(&selected_mode, receipt)
    })
    .await
    .map_err(|_| "Android app automation mode probe interrupted".to_owned())??;
    Ok(result)
}

/// This is a private trusted-host API, NOT a WebView Tauri command. A caller
/// must first obtain per-call approval for the exact action and arguments.
fn approved_action(approved: bool, session: &str, target: &str) -> Result<(), String> {
    validate_automation_start(approved, session, target)
}

fn snapshot_uuid(value: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| "A fresh Android automation snapshot is required".to_owned())
}

fn bounded_text(value: &str) -> Result<(), String> {
    if (1..=512).contains(&value.chars().count())
        && value.len() <= 2_048
        && !value.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err("Automation text must be 1-512 printable characters".into())
    }
}

fn native_reason(value: &Value, fallback: &str) -> String {
    value
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| {
            !reason.is_empty()
                && reason.len() <= 90
                && reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        })
        .unwrap_or(fallback)
        .to_owned()
}

fn sensitive_label(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if text.contains('@')
        || [
            "password",
            "passcode",
            "otp",
            "one-time",
            "security code",
            "api key",
            "access token",
            "authorization",
            "bearer",
            "private key",
            "recovery code",
            "seed phrase",
        ]
        .iter()
        .any(|word| lower.contains(word))
    {
        return true;
    }
    let mut digits = 0u8;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits = digits.saturating_add(1);
            if digits >= 4 {
                return true;
            }
        } else {
            digits = 0;
        }
    }
    false
}

fn check_snapshot_receipt(selected: &str, value: Value) -> Result<Value, String> {
    if value["ok"] != true {
        return Err(native_reason(
            &value,
            "android_automation_snapshot_unavailable",
        ));
    }
    validate_selected_mode(selected)?;
    let mode = value["effectiveMode"]
        .as_str()
        .ok_or("Snapshot mode missing")?;
    if value["selectedMode"] != selected
        || !matches!(mode, "realtime" | "screenshot_click")
        || value["fallbackUsed"] != json!(selected == "realtime" && mode == "screenshot_click")
    {
        return Err("Android snapshot changed selected mode".into());
    }
    snapshot_uuid(value["snapshotId"].as_str().unwrap_or_default())?;
    let sw = value["screenWidth"].as_u64().unwrap_or(0);
    let sh = value["screenHeight"].as_u64().unwrap_or(0);
    if sw == 0 || sw > 10_000 || sh == 0 || sh > 10_000 {
        return Err("Android snapshot dimensions invalid".into());
    }
    match mode {
        "realtime" => {
            if value["mimeType"] != "none"
                || value.as_object().is_none_or(|fields| fields.len() != 9)
                || value.get("imageBase64").is_some()
            {
                return Err("Semantic snapshot unexpectedly included pixels".into());
            }
            let nodes = value["elements"]
                .as_array()
                .ok_or("Semantic nodes missing")?;
            if nodes.is_empty() || nodes.len() > 48 {
                return Err("Semantic snapshot count out of bounds".into());
            }
            for (pos, node) in nodes.iter().enumerate() {
                if node.as_object().is_none_or(|fields| fields.len() != 11)
                    || node["index"] != (pos + 1) as u64
                    || ["className", "text", "description"].iter().any(|key| {
                        node[*key].as_str().is_none_or(|value| {
                            value.chars().count() > 96 || sensitive_label(value)
                        })
                    })
                    || ["clickable", "editable", "protected"]
                        .iter()
                        .any(|key| !node[*key].is_boolean())
                    || ["left", "top", "right", "bottom"]
                        .iter()
                        .any(|key| node[*key].as_i64().is_none())
                    || ((node["protected"] == true || node["editable"] == true)
                        && ((node["protected"] == true && node["clickable"] == true)
                            || node["text"] != ""
                            || node["description"] != ""))
                {
                    return Err("Unsafe Android semantic snapshot".into());
                }
            }
        }
        "screenshot_click" => {
            if value["mimeType"] != "image/jpeg"
                || value.as_object().is_none_or(|fields| fields.len() != 12)
                || value.get("elements").is_some()
            {
                return Err("Screenshot data type inconsistent".into());
            }
            let bytes = value["imageBytes"].as_u64().unwrap_or(0);
            let width = value["imageWidth"].as_u64().unwrap_or(0);
            let height = value["imageHeight"].as_u64().unwrap_or(0);
            let image = value["imageBase64"].as_str().unwrap_or_default();
            if bytes < 32
                || bytes > 128 * 1024
                || width == 0
                || height == 0
                || width > 900
                || height > 900
                || image.len() != ((bytes as usize + 2) / 3) * 4
                || !image.starts_with("/9j/")
                || !image
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
            {
                return Err("Android screenshot asset invalid or oversized".into());
            }
        }
        _ => return Err("Unsupported Android effective mode".into()),
    }
    Ok(value)
}

fn check_action_receipt(selected: &str, value: Value) -> Result<Value, String> {
    if value["ok"] != true {
        return Err(native_reason(
            &value,
            "android_automation_action_unavailable",
        ));
    }
    let mode = value["effectiveMode"]
        .as_str()
        .ok_or("Action mode missing")?;
    if value.as_object().is_none_or(|fields| fields.len() != 6)
        || value["selectedMode"] != selected
        || !matches!(mode, "realtime" | "screenshot_click")
        || value["fallbackUsed"] != json!(selected == "realtime" && mode == "screenshot_click")
        || value["dispatched"] != true
        || value["completed"] != false
    {
        return Err("Android action receipt invalid".into());
    }
    Ok(value)
}

async fn dispatch_approved(
    app: AppHandle,
    operation: &'static str,
    args: Value,
    selected_mode: String,
    snapshot: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let receipt = call(app, operation, args)?;
        if snapshot {
            check_snapshot_receipt(&selected_mode, receipt)
        } else {
            check_action_receipt(&selected_mode, receipt)
        }
    })
    .await
    .map_err(|_| "Android automation interrupted".to_owned())?
}

pub(crate) async fn android_automation_snapshot(
    app: AppHandle,
    approved: bool,
    selected_mode: String,
    session_id: String,
    target_package: String,
) -> Result<Value, String> {
    approved_action(approved, &session_id, &target_package)?;
    validate_selected_mode(&selected_mode)?;
    dispatch_approved(
        app,
        "automationSnapshot",
        json!({"approved":true,"selectedMode":selected_mode,"sessionId":session_id,
            "targetPackage":target_package}),
        selected_mode,
        true,
    )
    .await
}

pub(crate) async fn android_automation_click(
    app: AppHandle,
    approved: bool,
    selected_mode: String,
    session_id: String,
    target_package: String,
    snapshot_id: String,
    index: Option<u32>,
    x: Option<u32>,
    y: Option<u32>,
) -> Result<Value, String> {
    approved_action(approved, &session_id, &target_package)?;
    validate_selected_mode(&selected_mode)?;
    snapshot_uuid(&snapshot_id)?;
    if index.is_some_and(|idx| !(1..=48).contains(&idx))
        || x.is_some_and(|coord| coord >= 10_000)
        || y.is_some_and(|coord| coord >= 10_000)
        || (index.is_none() && (x.is_none() || y.is_none()))
    {
        return Err("Invalid Android click target".into());
    }
    dispatch_approved(
        app,
        "automationClick",
        json!({"approved":true,"selectedMode":selected_mode,"sessionId":session_id,
            "targetPackage":target_package,"snapshotId":snapshot_id,"index":index.unwrap_or(0),
            "x":x.unwrap_or(0),"y":y.unwrap_or(0)}),
        selected_mode,
        false,
    )
    .await
}

pub(crate) async fn android_automation_fill(
    app: AppHandle,
    approved: bool,
    session_id: String,
    target_package: String,
    snapshot_id: String,
    index: u32,
    text: String,
) -> Result<Value, String> {
    approved_action(approved, &session_id, &target_package)?;
    snapshot_uuid(&snapshot_id)?;
    bounded_text(&text)?;
    if !(1..=48).contains(&index) {
        return Err("Invalid Android fill index".into());
    }
    dispatch_approved(
        app,
        "automationFill",
        json!({"approved":true,"selectedMode":"realtime","sessionId":session_id,
            "targetPackage":target_package,"snapshotId":snapshot_id,"index":index,"text":text}),
        "realtime".into(),
        false,
    )
    .await
}

pub(crate) async fn android_automation_gesture(
    app: AppHandle,
    approved: bool,
    selected_mode: String,
    session_id: String,
    target_package: String,
    snapshot_id: String,
    kind: String,
    x: u32,
    y: u32,
    to_x: u32,
    to_y: u32,
    duration_ms: u32,
) -> Result<Value, String> {
    approved_action(approved, &session_id, &target_package)?;
    validate_selected_mode(&selected_mode)?;
    snapshot_uuid(&snapshot_id)?;
    if !matches!(kind.as_str(), "tap" | "swipe")
        || [x, y, to_x, to_y].iter().any(|coord| *coord >= 10_000)
        || (kind == "swipe" && !(120..=1_500).contains(&duration_ms))
    {
        return Err("Invalid Android gesture".into());
    }
    dispatch_approved(
        app,
        "automationGesture",
        json!({"approved":true,"selectedMode":selected_mode,"sessionId":session_id,
            "targetPackage":target_package,"snapshotId":snapshot_id,"kind":kind,
            "x":x,"y":y,"toX":to_x,"toY":to_y,"durationMs":duration_ms}),
        selected_mode,
        false,
    )
    .await
}

#[tauri::command]
pub async fn accessibility_automation_resolve_target(
    app: AppHandle,
    query: String,
) -> Result<Value, String> {
    if !(2..=80).contains(&query.chars().count()) || query.chars().any(char::is_control) {
        return Err("One valid Android app name is required".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let result = call(app, "automationResolveTarget", json!({"query":query}))?;
        if !result["resolved"].is_boolean() {
            return Err("App name lookup receipt missing".into());
        }
        if result["resolved"] == true {
            let pkg = result["targetPackage"]
                .as_str()
                .ok_or("App package missing")?;
            validate_automation_start(true, "resolve01", pkg)?;
        }
        Ok(result)
    })
    .await
    .map_err(|_| "App name resolution interrupted".to_string())?
}

#[tauri::command]
pub async fn accessibility_automation_start(
    app: AppHandle,
    user_initiated: bool,
    session_id: String,
    target_package: String,
    open_target: bool,
) -> Result<Value, String> {
    validate_automation_start(user_initiated, &session_id, &target_package)?;
    tauri::async_runtime::spawn_blocking(move || {
        call(
            app,
            "automationStart",
            json!({"userInitiated": true, "sessionId": session_id, "targetPackage": target_package, "openTarget": open_target}),
        )
    })
    .await
    .map_err(|_| "App automation start interrupted".to_string())?
}

#[tauri::command]
pub async fn accessibility_automation_stop(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || call(app, "automationStop", json!({})))
        .await
        .map_err(|_| "App automation stop interrupted".to_string())?
}

/// Trusted internal continuation *after* approval of a particular screen
/// action. The approval Activity temporarily hides the target. Return ONLY to
/// that same previously armed, still-active OS-bound session and wait for the
/// foreground window to be attested; never start or authorize a fresh session.
/// This is deliberately NOT a Tauri/WebView command or an MCP tool.
pub(crate) async fn accessibility_automation_resume_approved(
    app: AppHandle,
    session_id: String,
    target_package: String,
) -> Result<Value, String> {
    validate_automation_start(true, &session_id, &target_package)?;
    let check_session = session_id.clone();
    let check_target = target_package.clone();
    let receipt = tauri::async_runtime::spawn_blocking(move || {
        call(
            app,
            "automationResume",
            json!({
                "approved": true,
                "sessionId": session_id,
                "targetPackage": target_package
            }),
        )
    })
    .await
    .map_err(|_| "Approved app continuation interrupted".to_string())??;
    validate_approved_resume(&receipt, &check_session, &check_target)?;
    Ok(json!({"resumed":true,"active":true,"visible":true,
        "connected":true,"stopRequested":false,
        "sessionId":check_session,"targetPackage":check_target}))
}

fn validate_approved_resume(receipt: &Value, session: &str, target: &str) -> Result<(), String> {
    if !receipt.is_object()
        || receipt["resumed"] != true
        || receipt["active"] != true
        || receipt["visible"] != true
        || receipt["connected"] != true
        || receipt["stopRequested"] != false
        || receipt["sessionId"].as_str() != Some(session)
        || receipt["targetPackage"].as_str() != Some(target)
    {
        return Err("Android did not confirm the previously approved foreground app".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_resume_requires_same_active_foreground_session() {
        let session = "qa_session_1234";
        let target = "com.example.app";
        let valid = json!({"resumed":true,"active":true,"visible":true,
            "connected":true,"stopRequested":false,"sessionId":session,
            "targetPackage":target});
        assert!(validate_approved_resume(&valid, session, target).is_ok());
        for change in [
            ("active", json!(false)),
            ("visible", json!(false)),
            ("connected", json!(false)),
            ("stopRequested", json!(true)),
            ("sessionId", json!("another_session")),
            ("targetPackage", json!("com.other.app")),
            ("resumed", json!(false)),
        ] {
            let mut rejected = valid.clone();
            rejected[change.0] = change.1;
            assert!(validate_approved_resume(&rejected, session, target).is_err());
        }
        assert!(validate_approved_resume(&json!({}), session, target).is_err());
    }

    #[test]
    fn accessibility_settings_require_explicit_user_action() {
        assert!(explicit(false).is_err());
        assert!(explicit(true).is_ok());
    }

    #[test]
    fn app_indicator_requires_user_action_and_targeted_session() {
        assert!(validate_automation_start(false, "abcd1234", "com.example.app").is_err());
        assert!(validate_automation_start(true, "bad", "com.example.app").is_err());
        assert!(validate_automation_start(true, "abcd1234", "ai.mcpagent.app").is_err());
        assert!(validate_automation_start(true, "abcd1234", "com.android.settings").is_err());
        assert!(validate_automation_start(true, "abcd1234", "com.example.app").is_ok());
    }

    #[test]
    fn automation_mode_only_accepts_explicit_dropdown_values() {
        assert!(validate_selected_mode("realtime").is_ok());
        assert!(validate_selected_mode("screenshot_click").is_ok());
        for invalid in [
            "",
            "auto",
            "semantic",
            "screenshot",
            "REALTIME",
            "realtime ",
            "all",
        ] {
            assert!(validate_selected_mode(invalid).is_err());
        }
    }

    fn mode_receipt(selected: &str, supported: bool, enabled: bool, connected: bool) -> Value {
        let reason = if !supported {
            "android_unsupported"
        } else if !enabled {
            "accessibility_permission_missing"
        } else if !connected {
            "accessibility_service_disconnected"
        } else if selected == "realtime" {
            "semantic_action_and_screenshot_tap_unavailable"
        } else {
            "screenshot_tap_unavailable"
        };
        json!({
            "selectedMode":selected,"effectiveMode":"unavailable", "fallbackUsed":false,
            "reason":reason,"supported":supported,"enabled":enabled,
            "connected":connected,"needsReconnect":false,
            "semanticActionAvailable":false,"screenCaptureAvailable":false,
            "tapAvailable":false,"realtimeAvailable":false,"screenshotClickAvailable":false
        })
    }

    #[test]
    fn automation_mode_status_is_truthful_and_preserves_selection() {
        for selected in ["realtime", "screenshot_click"] {
            for (supported, enabled, connected) in [
                (false, false, false),
                (true, false, false),
                (true, true, false),
                (true, true, true),
            ] {
                let status = mode_receipt(selected, supported, enabled, connected);
                assert_eq!(
                    validate_automation_mode_receipt(selected, status.clone()).unwrap(),
                    status
                );
                assert_eq!(status["selectedMode"], selected);
                assert_eq!(status["effectiveMode"], "unavailable");
                assert_eq!(status["fallbackUsed"], false);
            }
        }
        let mut reconnecting = mode_receipt("realtime", true, true, false);
        reconnecting["needsReconnect"] = json!(true);
        assert!(validate_automation_mode_receipt("realtime", reconnecting).is_ok());
    }

    #[test]
    fn rejects_fabricated_capability_or_fake_fallback_receipts() {
        let baseline = mode_receipt("realtime", true, true, true);
        for (key, fake) in [
            ("semanticActionAvailable", json!(true)),
            ("realtimeAvailable", json!(true)),
            ("screenshotClickAvailable", json!(true)),
            ("effectiveMode", json!("screenshot_click")),
            ("fallbackUsed", json!(true)),
            ("reason", json!("supported")),
            ("selectedMode", json!("screenshot_click")),
            ("connected", json!("true")),
        ] {
            let mut forged = baseline.clone();
            forged[key] = fake;
            assert!(
                validate_automation_mode_receipt("realtime", forged).is_err(),
                "{key}"
            );
        }
        let mut extra = baseline.clone();
        extra["unverifiedTool"] = json!(true);
        assert!(validate_automation_mode_receipt("realtime", extra).is_err());
        let invalid_service = mode_receipt("realtime", true, false, false);
        let mut invalid_service = invalid_service;
        invalid_service["connected"] = json!(true);
        assert!(validate_automation_mode_receipt("realtime", invalid_service).is_err());
    }

    #[test]
    fn supports_live_os_semantic_and_screenshot_fallback_capabilities() {
        let mut semantic = mode_receipt("realtime", true, true, true);
        semantic["semanticActionAvailable"] = json!(true);
        semantic["realtimeAvailable"] = json!(true);
        semantic["effectiveMode"] = json!("realtime");
        semantic["reason"] = json!("semantic_ready");
        assert!(validate_automation_mode_receipt("realtime", semantic).is_ok());

        let mut fallback = mode_receipt("realtime", true, true, true);
        fallback["screenCaptureAvailable"] = json!(true);
        fallback["tapAvailable"] = json!(true);
        fallback["screenshotClickAvailable"] = json!(true);
        fallback["effectiveMode"] = json!("screenshot_click");
        fallback["fallbackUsed"] = json!(true);
        fallback["reason"] = json!("semantic_fallback_to_screenshot_click");
        assert!(validate_automation_mode_receipt("realtime", fallback.clone()).is_ok());
        assert!(
            validate_automation_mode_receipt("screenshot_click", fallback).is_err(),
            "Native cannot change the selected preference"
        );

        let mut screenshot = mode_receipt("screenshot_click", true, true, true);
        screenshot["screenCaptureAvailable"] = json!(true);
        screenshot["tapAvailable"] = json!(true);
        screenshot["screenshotClickAvailable"] = json!(true);
        screenshot["effectiveMode"] = json!("screenshot_click");
        screenshot["reason"] = json!("screenshot_click_ready");
        assert!(validate_automation_mode_receipt("screenshot_click", screenshot).is_ok());
        let mut gesture_only = mode_receipt("realtime", true, true, true);
        gesture_only["tapAvailable"] = json!(true);
        assert!(validate_automation_mode_receipt("realtime", gesture_only).is_ok());
    }

    fn sample_semantic() -> Value {
        json!({"ok":true,"selectedMode":"realtime","effectiveMode":"realtime",
            "fallbackUsed":false,"snapshotId":"4f456752-2fd4-47bc-becf-1132e63838ef",
            "mimeType":"none","screenWidth":1080,"screenHeight":2400,
            "elements":[{"index":1,"className":"android.widget.Button",
                "text":"Continue","description":"","clickable":true,"editable":false,
                "protected":false,"left":20,"top":30,"right":220,"bottom":100}]})
    }

    #[test]
    fn semantic_snapshot_accepts_only_bounded_redacted_target_nodes() {
        let valid = sample_semantic();
        assert!(check_snapshot_receipt("realtime", valid.clone()).is_ok());
        for (key, malicious) in [
            ("text", json!("123456")),
            ("text", json!("otp")),
            ("description", json!("user@example.org")),
        ] {
            let mut poisoned = valid.clone();
            poisoned["elements"][0][key] = malicious;
            assert!(
                check_snapshot_receipt("realtime", poisoned).is_err(),
                "{key}"
            );
        }
        let mut password = valid.clone();
        password["elements"][0]["protected"] = json!(true);
        assert!(check_snapshot_receipt("realtime", password).is_err());
        let mut editable_value = valid.clone();
        editable_value["elements"][0]["editable"] = json!(true);
        assert!(check_snapshot_receipt("realtime", editable_value).is_err());
        let mut inflated = valid.clone();
        inflated["elements"] = json!(vec![valid["elements"][0].clone(); 49]);
        assert!(check_snapshot_receipt("realtime", inflated).is_err());
        let mut extra = valid.clone();
        extra["unexpected"] = json!("unbounded payload");
        assert!(check_snapshot_receipt("realtime", extra).is_err());
    }

    #[test]
    fn screenshot_snapshot_checks_jpeg_bounds_and_selected_fallback() {
        // 33 synthetic bytes for the shape validator, not a real JPEG.
        let valid = json!({"ok":true,"selectedMode":"realtime",
            "effectiveMode":"screenshot_click","fallbackUsed":true,
            "snapshotId":"4f456752-2fd4-47bc-becf-1132e63838ef",
            "mimeType":"image/jpeg","screenWidth":1080,"screenHeight":2400,
            "imageWidth":405,"imageHeight":900,"imageBytes":33,
            "imageBase64":format!("/9j/{}", "A".repeat(40))});
        assert!(check_snapshot_receipt("realtime", valid.clone()).is_ok());
        assert!(check_snapshot_receipt("screenshot_click", valid.clone()).is_err());
        for (key, fake) in [
            ("imageBytes", json!(131073)),
            ("imageWidth", json!(10_001)),
            ("imageBase64", json!("nope")),
            ("mimeType", json!("image/png")),
            ("fallbackUsed", json!(false)),
        ] {
            let mut poisoned = valid.clone();
            poisoned[key] = fake;
            assert!(
                check_snapshot_receipt("realtime", poisoned).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn action_receipts_cannot_claim_completed_without_os_attestation() {
        let valid = json!({"ok":true,"dispatched":true,"completed":false,
            "selectedMode":"realtime","effectiveMode":"realtime","fallbackUsed":false});
        assert!(check_action_receipt("realtime", valid.clone()).is_ok());
        let mut fake = valid.clone();
        fake["completed"] = json!(true);
        assert!(check_action_receipt("realtime", fake).is_err());
        let mut fake = valid.clone();
        fake["approved"] = json!(true);
        assert!(check_action_receipt("realtime", fake).is_err());
        assert!(approved_action(false, "session001", "com.example.app").is_err());
        assert!(approved_action(true, "session001", "com.example.app").is_ok());
        assert!(snapshot_uuid("not-a-uuid").is_err());
        assert!(bounded_text("").is_err());
        assert!(bounded_text("abc").is_ok());
        assert!(bounded_text("x\nmore").is_err());
        assert!(bounded_text(&"a".repeat(513)).is_err());
    }
}
