//! Android one-shot MediaProjection consent and one-frame PNG result.
//!
//! Prime wiring: `mod android_screen_capture;`, Android-only
//! `.plugin(android_screen_capture::init())`, and generate_handler entries
//! `android_screen_capture::screen_capture_status`,
//! `android_screen_capture::screen_capture_one_frame`,
//! `android_screen_capture::screen_capture_cancel`.
//! The Kotlin plugin and service must be staged in the generated Android app,
//! and its manifest must declare the mediaProjection FGS type and permissions.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct OneShotHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("one-shot-capture")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "OneShotCapturePlugin")?;
            app.manage(OneShotHandle(plugin));
            Ok(())
        })
        .build()
}

fn require_gesture(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("One-shot screen capture requires an explicit user action".into())
    }
}

const MAX_PNG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BASE64_CHARS: usize = ((MAX_PNG_BYTES as usize + 2) / 3) * 4;

/// The UI receives only a bounded PNG result from the local Android plugin.
/// Reject malformed dimensions/metadata before delivering the bridge result.
fn validate_frame(value: &Value) -> Result<(), String> {
    let width = value.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = value.get("height").and_then(Value::as_u64).unwrap_or(0);
    let bytes = value.get("pngBytes").and_then(Value::as_u64).unwrap_or(0);
    let base64 = value.get("base64").and_then(Value::as_str).unwrap_or("");
    if value.get("captured") != Some(&Value::Bool(true))
        || value.get("mimeType").and_then(Value::as_str) != Some("image/png")
        || width == 0
        || height == 0
        || width > 1280
        || height > 1280
        || width * height > 921_600
        || bytes == 0
        || bytes > MAX_PNG_BYTES
        || base64.len() > MAX_BASE64_CHARS
        || base64.len() != ((bytes as usize + 2) / 3) * 4
        || !base64
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return Err("Android capture returned an invalid or oversized PNG frame".into());
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, name: &str, args: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<OneShotHandle<tauri::Wry>>()
        .ok_or_else(|| "Android screen capture plugin not registered".to_owned())?;
    handle
        .0
        .run_mobile_plugin(name, args)
        .map_err(|_| "Android screen capture unavailable or cancelled".to_owned())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _name: &str, _args: Value) -> Result<Value, String> {
    Err("Screen capture is available only in the Android app".into())
}

#[tauri::command]
pub async fn screen_capture_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || call(app, "status", json!({})))
        .await
        .map_err(|_| "Screen capture status interrupted".to_owned())?
}

/// UI must set `userInitiated: true` directly from a visible capture button.
/// Result: `{captured:true,mimeType:"image/png",width,height,pngBytes,base64}`.
/// Errors include denied consent, app backgrounded, timeout and oversized PNG.
#[tauri::command]
pub async fn screen_capture_one_frame(
    app: AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    require_gesture(user_initiated)?;
    let frame = tauri::async_runtime::spawn_blocking(move || {
        call(app, "capture", json!({"userInitiated": true}))
    })
    .await
    .map_err(|_| "Screen capture interrupted".to_owned())??;
    validate_frame(&frame)?;
    Ok(frame)
}

#[tauri::command]
pub async fn screen_capture_cancel(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    require_gesture(user_initiated)?;
    tauri::async_runtime::spawn_blocking(move || {
        call(app, "cancelCapture", json!({"userInitiated": true}))
    })
    .await
    .map_err(|_| "Screen capture cancellation interrupted".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_automated_capture_or_cancellation() {
        assert!(require_gesture(false).is_err());
        assert!(require_gesture(true).is_ok());
    }

    #[test]
    fn validates_frame_bounding_and_metadata() {
        let ok = json!({"captured":true, "mimeType":"image/png", "width":1280,
            "height":720,"pngBytes":3,"base64":"QUJD"});
        assert!(validate_frame(&ok).is_ok());
        for bad in [
            json!({"captured":true, "mimeType":"image/png", "width":1281,
                "height":720,"pngBytes":3,"base64":"QUJD"}),
            json!({"captured":true, "mimeType":"image/png", "width":1280,
                "height":1280,"pngBytes":3,"base64":"QUJD"}),
            json!({"captured":true, "mimeType":"image/jpeg", "width":10,
                "height":10,"pngBytes":3,"base64":"QUJD"}),
            json!({"captured":true, "mimeType":"image/png", "width":10,
                "height":10,"pngBytes":0,"base64":""}),
            json!({"captured":true, "mimeType":"image/png", "width":10,
                "height":10,"pngBytes":3,"base64":"A$CD"}),
            json!({"captured":false, "mimeType":"image/png", "width":10,
                "height":10,"pngBytes":3,"base64":"QUJD"}),
        ] {
            assert!(validate_frame(&bad).is_err());
        }
        assert!(
            validate_frame(&json!({"captured":true,"mimeType":"image/png",
            "width":1,"height":1,"pngBytes":MAX_PNG_BYTES+1,
            "base64":"QUJD"}))
            .is_err()
        );
    }
}
