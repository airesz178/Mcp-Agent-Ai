//! User-controlled Android foreground service and its persistent notification.
//! Every status comes from the registered native Android plugin. An absent or
//! malformed native receipt never becomes a synthetic running state.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct ForegroundRuntimeHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("foreground-runtime")
        .setup(|app, api| {
            let plugin =
                api.register_android_plugin("ai.mcpagent.app", "ForegroundRuntimePlugin")?;
            app.manage(ForegroundRuntimeHandle(plugin));
            Ok(())
        })
        .build()
}

fn require_user_action(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("Android background runtime Start/Stop requires an explicit user action".into())
    }
}

/// Native contract for status/start/stop:
/// {supported:boolean,running:boolean,notificationVisible:boolean,stopAvailable:boolean}
/// Only a native confirmed running service can report a notification; only a
/// visible notification can report the notification's Stop action as available.
fn verify_receipt(receipt: Value, expected_running: Option<bool>) -> Result<Value, String> {
    let invalid = || "Invalid Android foreground runtime receipt".to_owned();
    let fields = receipt.as_object().ok_or_else(invalid)?;
    if fields.len() != 4
        || fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "supported" | "running" | "notificationVisible" | "stopAvailable"
            )
        })
    {
        return Err(invalid());
    }
    let supported = fields
        .get("supported")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let running = fields
        .get("running")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let notification_visible = fields
        .get("notificationVisible")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let stop_available = fields
        .get("stopAvailable")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;

    if !supported
        || (notification_visible && !running)
        || (stop_available && (!running || !notification_visible))
        || expected_running.is_some_and(|expected| running != expected)
    {
        return Err("Android foreground runtime state was not confirmed".into());
    }
    // A start is complete only after Android actually starts the foreground
    // service and exposes the persistent notification's Stop action.
    if expected_running == Some(true) && (!notification_visible || !stop_available) {
        return Err("Android foreground notification or Stop action is not available".into());
    }
    Ok(receipt)
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, method: &str, args: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<ForegroundRuntimeHandle<tauri::Wry>>()
        .ok_or("Android foreground runtime plugin is not registered")?;
    handle
        .0
        .run_mobile_plugin(method, args)
        .map_err(|_| "Android foreground runtime unavailable or operation denied".to_owned())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _method: &str, _args: Value) -> Result<Value, String> {
    Err("Foreground runtime is available only in the Android application".into())
}

#[tauri::command]
pub async fn android_foreground_runtime_status(app: AppHandle) -> Result<Value, String> {
    let receipt = tauri::async_runtime::spawn_blocking(move || call(app, "status", json!({})))
        .await
        .map_err(|_| "Android foreground runtime status interrupted".to_owned())??;
    verify_receipt(receipt, None)
}

#[tauri::command]
pub async fn android_foreground_runtime_start(
    app: AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    require_user_action(user_initiated)?;
    let receipt = tauri::async_runtime::spawn_blocking(move || {
        call(app, "start", json!({"userInitiated": true}))
    })
    .await
    .map_err(|_| "Android foreground runtime start interrupted".to_owned())??;
    verify_receipt(receipt, Some(true))
}

#[tauri::command]
pub async fn android_foreground_runtime_stop(
    app: AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    require_user_action(user_initiated)?;
    let receipt = tauri::async_runtime::spawn_blocking(move || {
        call(app, "stop", json!({"userInitiated": true}))
    })
    .await
    .map_err(|_| "Android foreground runtime stop interrupted".to_owned())??;
    verify_receipt(receipt, Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_stop_require_an_explicit_user_action() {
        assert!(require_user_action(false).is_err());
        assert!(require_user_action(true).is_ok());
    }

    #[test]
    fn status_handles_stopped_running_and_degraded_notification_truthfully() {
        let stopped = json!({"supported":true,"running":false,"notificationVisible":false,"stopAvailable":false});
        let running = json!({"supported":true,"running":true,"notificationVisible":true,"stopAvailable":true});
        let permission_denied = json!({"supported":true,"running":true,"notificationVisible":false,"stopAvailable":false});
        assert_eq!(verify_receipt(stopped.clone(), None).unwrap(), stopped);
        assert_eq!(verify_receipt(running.clone(), None).unwrap(), running);
        assert_eq!(
            verify_receipt(permission_denied.clone(), None).unwrap(),
            permission_denied
        );
        assert!(verify_receipt(permission_denied, Some(true)).is_err());
        assert!(verify_receipt(running, Some(false)).is_err());
        assert!(verify_receipt(stopped, Some(true)).is_err());
    }

    #[test]
    fn actions_require_verified_android_state_and_notification_button() {
        let started = json!({"supported":true,"running":true,"notificationVisible":true,"stopAvailable":true});
        let stopped = json!({"supported":true,"running":false,"notificationVisible":false,"stopAvailable":false});
        assert!(verify_receipt(started.clone(), Some(true)).is_ok());
        assert!(verify_receipt(stopped.clone(), Some(false)).is_ok());
        assert!(verify_receipt(started.clone(), Some(false)).is_err());
        assert!(verify_receipt(stopped.clone(), Some(true)).is_err());
        let missing_stop = json!({"supported":true,"running":true,"notificationVisible":true,"stopAvailable":false});
        assert!(verify_receipt(missing_stop.clone(), None).is_ok());
        assert!(verify_receipt(missing_stop, Some(true)).is_err());
    }

    #[test]
    fn reject_invalid_extra_missing_or_conflicting_native_receipts() {
        for invalid in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"supported":true,"running":true,"notificationVisible":true}),
            json!({"supported":true,"running":true,"notificationVisible":true,"stopAvailable":true,"token":"unexpected"}),
            json!({"supported":"true","running":true,"notificationVisible":true,"stopAvailable":true}),
            json!({"supported":false,"running":false,"notificationVisible":false,"stopAvailable":false}),
            json!({"supported":true,"running":false,"notificationVisible":true,"stopAvailable":false}),
            json!({"supported":true,"running":false,"notificationVisible":false,"stopAvailable":true}),
            json!({"supported":true,"running":true,"notificationVisible":false,"stopAvailable":true}),
        ] {
            assert!(verify_receipt(invalid.clone(), None).is_err(), "{invalid}");
        }
    }
}
